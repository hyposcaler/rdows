//! Demonstrates two-sided SEND/RECV over RDoWS.
//!
//! Run the server first:
//!     cargo run -p rdows-server -- --bind 127.0.0.1:9443 --cert server.crt --key server.key
//!
//! Then run this example:
//!     cargo run -p rdows-client --example echo_send_recv

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use rdows_client::rdows_core::memory::AccessFlags;
use rdows_client::rdows_core::queue::ScatterGatherEntry;
use rdows_client::RdowsConnection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Start an embedded server for the demo
    let (addr, client_tls) = start_embedded_server().await;
    let url = format!("wss://localhost:{}/rdows", addr.port());

    println!("Connecting to {url}...");
    let mut conn = RdowsConnection::connect(&url, client_tls).await?;
    println!("Session established (id: 0x{:08X})", conn.session_id());

    // Register a memory region for our send buffer
    let mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await?;
    println!(
        "Registered MR: L_Key=0x{:08X}, R_Key=0x{:08X}, size=256",
        mr.lkey.0, mr.rkey.0
    );

    // Write a message into the local buffer
    let message = b"Hello, RDoWS! The CPU never saw this coming.";
    conn.write_local_mr(mr.lkey, 0, message)?;

    // Post a SEND operation
    println!("Posting SEND ({} bytes)...", message.len());
    conn.post_send(
        1,
        &[ScatterGatherEntry {
            lkey: mr.lkey,
            offset: 0,
            length: message.len() as u32,
        }],
    )
    .await?;

    // Poll the completion queue
    let cqes = conn.poll_cq(10);
    for cqe in &cqes {
        println!(
            "CQE: wrid={}, status=0x{:04X}, bytes={}",
            cqe.wrid.0, cqe.status, cqe.byte_count
        );
    }

    conn.disconnect().await?;
    println!("Disconnected.");
    Ok(())
}

async fn start_embedded_server() -> (std::net::SocketAddr, rustls::ClientConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_der = cert.cert.der().clone();
    let key_der = cert.key_pair.serialize_der();

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert_der.clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into()),
        )
        .unwrap();

    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(cert_der).unwrap();
    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    tokio::spawn(async move {
        rdows_server::run_server(listener, acceptor, rdows_server::ServerConfig::default()).await;
    });

    (addr, client_config)
}
