//! Demonstrates one-sided RDMA Write and Read over RDoWS.
//!
//! This is the whole point of the project: writing directly into a remote
//! memory region over WebSockets, then reading it back, without the remote
//! CPU being involved in the data path.

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use rdows_client::rdows_core::memory::AccessFlags;
use rdows_client::rdows_core::queue::ScatterGatherEntry;
use rdows_client::RdowsConnection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (addr, client_tls) = start_embedded_server().await;
    let url = format!("wss://localhost:{}/rdows", addr.port());

    println!("Connecting to {url}...");
    let mut conn = RdowsConnection::connect(&url, client_tls).await?;
    println!("Session established (id: 0x{:08X})", conn.session_id());

    // Register a remote MR with RDMA Write + Read access
    let remote_mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ, 4096)
        .await?;
    println!(
        "Remote MR registered: R_Key=0x{:08X}, size=4096",
        remote_mr.rkey.0
    );

    // Register a local MR for our source/sink buffers
    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 4096).await?;

    // Write test data into local buffer
    let payload = b"RDMA over WebSockets: because InfiniBand was too easy.";
    conn.write_local_mr(local_mr.lkey, 0, payload)?;

    // RDMA Write: push data from local MR into remote MR at offset 0
    println!("RDMA Write: {} bytes -> remote VA 0x0000", payload.len());
    conn.rdma_write(
        100,
        remote_mr.rkey,
        0,
        &[ScatterGatherEntry {
            lkey: local_mr.lkey,
            offset: 0,
            length: payload.len() as u32,
        }],
    )
    .await?;

    let cqes = conn.poll_cq(10);
    println!(
        "Write complete: {} CQE(s), status=0x{:04X}",
        cqes.len(),
        cqes.first().map(|c| c.status).unwrap_or(0xFFFF)
    );

    // Register another local MR as a read sink
    let read_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 4096).await?;

    // RDMA Read: pull data back from remote MR into local read buffer
    println!(
        "RDMA Read: {} bytes <- remote VA 0x0000",
        payload.len()
    );
    conn.rdma_read(
        200,
        remote_mr.rkey,
        0,
        payload.len() as u64,
        read_mr.lkey,
        0,
    )
    .await?;

    let cqes = conn.poll_cq(10);
    println!(
        "Read complete: {} CQE(s), status=0x{:04X}",
        cqes.len(),
        cqes.first().map(|c| c.status).unwrap_or(0xFFFF)
    );

    // Verify the data
    let read_back = conn.read_local_mr(read_mr.lkey, 0, payload.len())?;
    println!("Readback: {:?}", std::str::from_utf8(&read_back).unwrap());
    assert_eq!(&read_back, payload);
    println!("Verification passed.");

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
