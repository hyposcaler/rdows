//! Demonstrates one-sided RDMA Read over RDoWS.
//!
//! Writes data into a remote memory region via RDMA Write, then performs
//! multiple RDMA Reads at different offsets to demonstrate random-access
//! read capabilities over WebSocket transport.

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

    let mut conn = RdowsConnection::connect(&url, client_tls).await?;
    println!("Session 0x{:08X} established", conn.session_id());

    // Register remote MR
    let remote_mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ, 256)
        .await?;

    // Local MR for writing source data
    let write_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await?;

    // Populate the remote MR with structured data
    let records = [
        "record_0: alpha",
        "record_1: bravo",
        "record_2: charlie",
        "record_3: delta",
    ];

    for (i, record) in records.iter().enumerate() {
        let offset = i * 64;
        conn.write_local_mr(write_mr.lkey, 0, record.as_bytes())?;
        conn.rdma_write(
            i as u64,
            remote_mr.rkey,
            offset as u64,
            &[ScatterGatherEntry {
                lkey: write_mr.lkey,
                offset: 0,
                length: record.len() as u32,
            }],
        )
        .await?;
    }
    // drain CQEs from writes
    conn.poll_cq(10);
    println!("Populated remote MR with {} records", records.len());

    // Read back records in reverse order to demonstrate random access
    let read_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await?;

    for i in (0..records.len()).rev() {
        let offset = i * 64;
        let len = records[i].len();

        conn.rdma_read(
            100 + i as u64,
            remote_mr.rkey,
            offset as u64,
            len as u64,
            read_mr.lkey,
            0,
        )
        .await?;

        let data = conn.read_local_mr(read_mr.lkey, 0, len)?;
        println!(
            "  Read VA 0x{:04X}: {:?}",
            offset,
            std::str::from_utf8(&data).unwrap()
        );
    }
    conn.poll_cq(10);

    conn.disconnect().await?;
    println!("Done.");
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
