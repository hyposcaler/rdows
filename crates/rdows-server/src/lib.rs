pub mod handler;
pub mod memory_store;
pub mod session;

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tracing::{debug, error, info};

use rdows_core::SUBPROTOCOL;

pub struct ServerConfig {
    pub recv_queue_depth: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            recv_queue_depth: 128,
        }
    }
}

pub async fn run_server(listener: TcpListener, tls_acceptor: TlsAcceptor, config: ServerConfig) {
    info!(
        addr = %listener.local_addr().unwrap(),
        "RDoWS server listening"
    );

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("accept error: {e}");
                continue;
            }
        };

        let acceptor = tls_acceptor.clone();
        let recv_queue_depth = config.recv_queue_depth;
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, acceptor, peer, recv_queue_depth).await {
                debug!(peer = %peer, "connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    acceptor: TlsAcceptor,
    peer: SocketAddr,
    recv_queue_depth: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!(peer = %peer, "new TCP connection");

    let tls_stream = acceptor.accept(stream).await?;
    debug!(peer = %peer, "TLS handshake complete");

    let ws_stream = tokio_tungstenite::accept_hdr_async(tls_stream, check_subprotocol).await?;

    info!(peer = %peer, "WebSocket upgrade complete");
    session::run_session(ws_stream, recv_queue_depth).await;
    Ok(())
}

#[allow(clippy::result_large_err)]
fn check_subprotocol(req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
    let has_rdows = req
        .headers()
        .get_all("Sec-WebSocket-Protocol")
        .iter()
        .any(|v| {
            v.to_str()
                .map(|s| s.split(',').any(|p| p.trim() == SUBPROTOCOL))
                .unwrap_or(false)
        });

    if !has_rdows {
        let mut err_resp = ErrorResponse::new(Some("missing rdows.v1 subprotocol".into()));
        *err_resp.status_mut() = http::StatusCode::BAD_REQUEST;
        return Err(err_resp);
    }

    resp.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        http::HeaderValue::from_static(SUBPROTOCOL),
    );
    Ok(resp)
}

pub fn build_server_tls_config(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<rustls::ServerConfig>, Box<dyn std::error::Error>> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut &*cert_pem).collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut &*key_pem)?
        .ok_or("no private key found")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(Arc::new(config))
}
