//! Demonstrates Fetch-and-Add (FAA) as a remote atomic counter over RDoWS.

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

    // Register remote MR with atomic + read + write access
    let remote_mr = conn
        .reg_mr(
            AccessFlags::REMOTE_ATOMIC | AccessFlags::REMOTE_READ | AccessFlags::REMOTE_WRITE,
            64,
        )
        .await?;
    println!(
        "Remote MR registered: R_Key=0x{:08X}, size=64",
        remote_mr.rkey.0
    );

    // Register local MR for write/read buffers
    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 64).await?;

    // Initialize counter to 0 via RDMA Write
    conn.write_local_mr(local_mr.lkey, 0, &0u64.to_be_bytes())?;
    conn.rdma_write(
        0,
        remote_mr.rkey,
        0,
        &[ScatterGatherEntry {
            lkey: local_mr.lkey,
            offset: 0,
            length: 8,
        }],
    )
    .await?;
    conn.poll_cq(10);
    println!("Counter initialized to 0");

    // FAA +1 five times
    for i in 1..=5 {
        let prev = conn.atomic_faa(i, remote_mr.rkey, 0, 1).await?;
        conn.poll_cq(10);
        println!("FAA #{i}: previous value = {prev}");
    }

    // RDMA Read the final value
    let read_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 64).await?;
    conn.rdma_read(100, remote_mr.rkey, 0, 8, read_mr.lkey, 0)
        .await?;
    conn.poll_cq(10);
    let final_bytes = conn.read_local_mr(read_mr.lkey, 0, 8)?;
    let final_value = u64::from_be_bytes(final_bytes.try_into().unwrap());
    println!("Final counter value: {final_value}");
    assert_eq!(final_value, 5);
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
