//! Demonstrates posted receive queue exhaustion and ERR_RNR recovery.
//!
//! The embedded server starts with recv_queue_depth=3. After 3 successful
//! SENDs the queue is exhausted and the 4th SEND returns ERR_RNR.
//! An RDMA Write afterwards proves the connection is still alive.

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use rdows_client::rdows_core::error::ErrorCode;
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
    println!("Session established (id: 0x{:08X})\n", conn.session_id());

    // Register a local MR for send data
    let mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await?;
    let message = b"Hello from SEND";
    conn.write_local_mr(mr.lkey, 0, message)?;
    let sg = vec![ScatterGatherEntry {
        lkey: mr.lkey,
        offset: 0,
        length: message.len() as u32,
    }];

    // Send 4 messages — first 3 succeed, 4th hits ERR_RNR
    for i in 1..=4u64 {
        print!("SEND #{i}: ");
        match conn.post_send(i, &sg).await {
            Ok(()) => {
                let cqes = conn.poll_cq(10);
                println!(
                    "OK (CQE: wrid={}, status=0x{:04X}, bytes={})",
                    cqes[0].wrid.0, cqes[0].status, cqes[0].byte_count
                );
            }
            Err(rdows_client::rdows_core::error::RdowsError::Protocol(code))
                if code == ErrorCode::ErrRnr =>
            {
                // Drain the ERR_RNR CQE
                let cqes = conn.poll_cq(10);
                println!(
                    "ERR_RNR (CQE: wrid={}, status=0x{:04X})",
                    cqes[0].wrid.0, cqes[0].status
                );
            }
            Err(e) => return Err(e.into()),
        }
    }

    // Prove connection is still alive with an RDMA Write
    println!("\nConnection still alive — performing RDMA Write...");
    let remote_mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE, 64)
        .await?;
    conn.write_local_mr(mr.lkey, 0, b"still alive")?;
    conn.rdma_write(
        100,
        remote_mr.rkey,
        0,
        &[ScatterGatherEntry {
            lkey: mr.lkey,
            offset: 0,
            length: 11,
        }],
    )
    .await?;
    let cqes = conn.poll_cq(10);
    println!(
        "RDMA Write: OK (CQE: wrid={}, status=0x{:04X})",
        cqes[0].wrid.0, cqes[0].status
    );

    println!("\nServer receive queue exhausted after 3 SENDs. Connection remains usable for RDMA operations.");

    conn.disconnect().await?;
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

    let config = rdows_server::ServerConfig { recv_queue_depth: 3 };
    tokio::spawn(async move {
        rdows_server::run_server(listener, acceptor, config).await;
    });

    (addr, client_config)
}
