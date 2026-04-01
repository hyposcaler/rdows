use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, error, info, warn};

use rdows_core::error::{ErrorCode, RdowsError};
use rdows_core::frame::RdowsHeader;
use rdows_core::memory::ProtectionDomain;
use rdows_core::message::{ConnectPayload, ErrorPayload, RdowsMessage};
use rdows_core::opcode::Opcode;
use rdows_core::{DEFAULT_ICC, DEFAULT_MAX_MSG_SIZE, RDOWS_VERSION};

use crate::handler;

pub type WsStream = WebSocketStream<TlsStream<TcpStream>>;
pub type WsSink = SplitSink<WsStream, Message>;

#[derive(Debug)]
enum SessionState {
    AwaitingConnect,
    Ready,
    Closed,
}

pub struct Session {
    state: SessionState,
    pub session_id: u32,
    pub pd: ProtectionDomain,
    pub max_msg_size: u32,
    pub next_seq: u32,
    pub memory_store: crate::memory_store::MemoryStore,
    pub pending_op: handler::PendingOp,
    pub recv_queue_depth: u32,
}

impl Session {
    fn new(recv_queue_depth: u32) -> Self {
        Self {
            state: SessionState::AwaitingConnect,
            session_id: 0,
            pd: ProtectionDomain(0),
            max_msg_size: DEFAULT_MAX_MSG_SIZE,
            next_seq: 0,
            memory_store: crate::memory_store::MemoryStore::new(),
            pending_op: handler::PendingOp::None,
            recv_queue_depth,
        }
    }

    pub fn next_header(&mut self, opcode: Opcode, wrid: u64) -> RdowsHeader {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        RdowsHeader::new(opcode, self.session_id, seq, wrid)
    }
}

pub async fn run_session(ws: WsStream, recv_queue_depth: u32) {
    let (mut sink, mut stream) = ws.split();
    let mut session = Session::new(recv_queue_depth);

    while let Some(msg_result) = stream.next().await {
        let msg = match msg_result {
            Ok(msg) => msg,
            Err(e) => {
                error!("websocket error: {e}");
                break;
            }
        };

        match msg {
            Message::Binary(data) => {
                let data = Bytes::from(data);
                let rdows_msg = match RdowsMessage::decode(data) {
                    Ok(m) => m,
                    Err(e) => {
                        error!("decode error: {e}");
                        break;
                    }
                };

                if let Err(e) = handle_message(&mut session, rdows_msg, &mut sink).await {
                    error!("handler error: {e}");
                    break;
                }

                if matches!(session.state, SessionState::Closed) {
                    break;
                }
            }
            Message::Close(_) => {
                debug!("received websocket close");
                break;
            }
            _ => {} // ping/pong handled by tungstenite
        }
    }

    info!(session_id = session.session_id, "session ended");
}

async fn handle_message(
    session: &mut Session,
    msg: RdowsMessage,
    sink: &mut WsSink,
) -> Result<(), RdowsError> {
    // Payload size check (only in Ready state when max_msg_size is negotiated)
    if matches!(session.state, SessionState::Ready)
        && msg.header().payload_length > session.max_msg_size
    {
        return send_error(
            session,
            sink,
            ErrorCode::ErrPayloadSize,
            msg.header().sequence,
            "payload exceeds negotiated maximum message size",
        )
        .await;
    }

    match &session.state {
        SessionState::AwaitingConnect => handle_connect(session, msg, sink).await,
        SessionState::Ready => handle_ready(session, msg, sink).await,
        SessionState::Closed => Ok(()),
    }
}

async fn handle_connect(
    session: &mut Session,
    msg: RdowsMessage,
    sink: &mut WsSink,
) -> Result<(), RdowsError> {
    match msg {
        RdowsMessage::Connect(header, payload) => {
            if header.version != RDOWS_VERSION {
                send_error(
                    session,
                    sink,
                    ErrorCode::ErrProtoVersion,
                    header.sequence,
                    "unsupported protocol version",
                )
                .await?;
                session.state = SessionState::Closed;
                return Ok(());
            }

            session.session_id = header.session_id;
            session.pd = payload.pd;
            session.max_msg_size = std::cmp::min(payload.max_msg_size, DEFAULT_MAX_MSG_SIZE);

            let ack_header = session.next_header(Opcode::ConnectAck, 0);
            let ack_payload = ConnectPayload {
                pd: session.pd,
                capability_flags: 0,
                max_msg_size: session.max_msg_size,
                icc: DEFAULT_ICC,
            };
            let ack = RdowsMessage::ConnectAck(ack_header, ack_payload);
            send_message(sink, &ack).await?;

            session.state = SessionState::Ready;
            info!(
                session_id = session.session_id,
                max_msg_size = session.max_msg_size,
                "session established"
            );
            Ok(())
        }
        other => {
            warn!(
                opcode = ?other.header().opcode,
                "expected CONNECT, got something else"
            );
            Err(RdowsError::UnexpectedMessage {
                expected: "CONNECT",
                got: other.header().opcode,
            })
        }
    }
}

async fn handle_ready(
    session: &mut Session,
    msg: RdowsMessage,
    sink: &mut WsSink,
) -> Result<(), RdowsError> {
    match &msg {
        RdowsMessage::Disconnect(_) => {
            info!(session_id = session.session_id, "disconnect received");
            session.state = SessionState::Closed;
            Ok(())
        }
        RdowsMessage::CreditUpdate(_, _) => {
            // Accept and ignore per MVP
            debug!("ignoring CREDIT_UPDATE");
            Ok(())
        }
        _ => handler::dispatch(session, msg, sink).await,
    }
}

pub async fn send_message(sink: &mut WsSink, msg: &RdowsMessage) -> Result<(), RdowsError> {
    let data = msg.encode();
    sink.send(Message::Binary(data.to_vec()))
        .await
        .map_err(|e| RdowsError::WebSocket(e.to_string()))?;
    Ok(())
}

pub async fn send_error(
    session: &mut Session,
    sink: &mut WsSink,
    error_code: ErrorCode,
    failing_seq: u32,
    description: &str,
) -> Result<(), RdowsError> {
    let header = session.next_header(Opcode::Error, 0);
    let payload = ErrorPayload {
        error_code,
        failing_seq,
        description: description.to_string(),
    };
    let msg = RdowsMessage::Error(header, payload);
    send_message(sink, &msg).await
}
