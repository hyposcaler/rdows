use std::sync::Arc;

use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::debug;

use rdows_core::error::RdowsError;
use rdows_core::frame::RdowsHeader;
use rdows_core::memory::ProtectionDomain;
use rdows_core::message::{ConnectPayload, RdowsMessage};
use rdows_core::opcode::Opcode;
use rdows_core::{DEFAULT_ICC, DEFAULT_MAX_MSG_SIZE, SUBPROTOCOL};

type WsStream = WebSocketStream<TlsStream<TcpStream>>;

pub type ClientSink = SplitSink<WsStream, Message>;
pub type ClientStream = SplitStream<WsStream>;

pub struct ConnectionParams {
    pub session_id: u32,
    pub pd: ProtectionDomain,
    pub max_msg_size: u32,
    pub initial_remote_seq: u32,
    pub icc: u32,
}

pub async fn connect(
    url: &str,
    tls_config: rustls::ClientConfig,
) -> Result<(ClientSink, ClientStream, ConnectionParams), RdowsError> {
    if url.starts_with("ws://") {
        return Err(RdowsError::ConnectionRejected(
            "TLS required: use wss:// not ws://".to_string(),
        ));
    }

    let mut request = url
        .into_client_request()
        .map_err(|e| RdowsError::WebSocket(e.to_string()))?;

    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(SUBPROTOCOL));

    // Extract host and port for TLS
    let uri = request.uri().clone();
    let host = uri.host().ok_or_else(|| {
        RdowsError::ConnectionRejected("no host in URL".to_string())
    })?;
    let port = uri.port_u16().unwrap_or(443);

    let tcp = TcpStream::connect(format!("{host}:{port}"))
        .await
        .map_err(|e| RdowsError::WebSocket(format!("TCP connect: {e}")))?;

    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| RdowsError::Tls(format!("invalid server name: {e}")))?;

    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| RdowsError::Tls(format!("TLS handshake: {e}")))?;

    debug!("TLS handshake complete");

    let (ws_stream, response) = tokio_tungstenite::client_async(request, tls_stream)
        .await
        .map_err(|e| RdowsError::WebSocket(format!("WS upgrade: {e}")))?;

    // Verify subprotocol was echoed
    let has_rdows = response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').any(|p| p.trim() == SUBPROTOCOL))
        .unwrap_or(false);

    if !has_rdows {
        return Err(RdowsError::ConnectionRejected(
            "server did not echo rdows.v1 subprotocol".to_string(),
        ));
    }

    debug!("WebSocket upgrade complete with rdows.v1 subprotocol");

    let (mut sink, mut stream) = ws_stream.split();

    // Generate session ID
    let session_id = {
        use rand::RngCore;
        let mut buf = [0u8; 4];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        u32::from_be_bytes(buf)
    };

    // Send CONNECT
    let connect_header = RdowsHeader::new(Opcode::Connect, session_id, 0, 0);
    let connect_payload = ConnectPayload {
        pd: ProtectionDomain(1),
        capability_flags: 0,
        max_msg_size: DEFAULT_MAX_MSG_SIZE,
        icc: DEFAULT_ICC,
    };
    let connect_msg = RdowsMessage::Connect(connect_header, connect_payload);
    let data = connect_msg.encode();
    sink.send(Message::Binary(data.to_vec()))
        .await
        .map_err(|e| RdowsError::WebSocket(e.to_string()))?;

    debug!(session_id, "sent CONNECT");

    // Await CONNECT_ACK
    let ack = recv_message(&mut stream).await?;
    match ack {
        RdowsMessage::ConnectAck(ack_header, payload) => {
            let max_msg_size = std::cmp::min(payload.max_msg_size, DEFAULT_MAX_MSG_SIZE);
            debug!(session_id, max_msg_size, "received CONNECT_ACK");
            Ok((
                sink,
                stream,
                ConnectionParams {
                    session_id,
                    pd: payload.pd,
                    max_msg_size,
                    initial_remote_seq: ack_header.sequence.wrapping_add(1),
                    icc: payload.icc,
                },
            ))
        }
        RdowsMessage::Error(_, err) => Err(RdowsError::Protocol(err.error_code)),
        other => Err(RdowsError::UnexpectedMessage {
            expected: "CONNECT_ACK",
            got: other.header().opcode,
        }),
    }
}

pub async fn recv_message(stream: &mut ClientStream) -> Result<RdowsMessage, RdowsError> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Binary(data))) => {
                return RdowsMessage::decode(Bytes::from(data));
            }
            Some(Ok(Message::Close(_))) => {
                return Err(RdowsError::SessionClosed);
            }
            Some(Ok(_)) => continue, // skip text, ping, pong
            Some(Err(e)) => {
                return Err(RdowsError::WebSocket(e.to_string()));
            }
            None => {
                return Err(RdowsError::SessionClosed);
            }
        }
    }
}

pub async fn send_message(sink: &mut ClientSink, msg: &RdowsMessage) -> Result<(), RdowsError> {
    let data = msg.encode();
    sink.send(Message::Binary(data.to_vec()))
        .await
        .map_err(|e| RdowsError::WebSocket(e.to_string()))
}
