use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use rdows_client::rdows_core::error::ErrorCode;
use rdows_client::rdows_core::memory::AccessFlags;
use rdows_client::rdows_core::queue::ScatterGatherEntry;
use rdows_client::RdowsConnection;
use rdows_server::ServerConfig;

struct TestServer {
    addr: SocketAddr,
    client_tls: rustls::ClientConfig,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_config(ServerConfig::default()).await
    }

    async fn start_with_config(config: ServerConfig) -> Self {
        let subject_alt_names = vec!["localhost".to_string()];
        let cert = rcgen::generate_simple_self_signed(subject_alt_names).unwrap();

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
            rdows_server::run_server(listener, acceptor, config).await;
        });

        TestServer {
            addr,
            client_tls: client_config,
        }
    }

    fn url(&self) -> String {
        format!("wss://localhost:{}/rdows", self.addr.port())
    }

    async fn connect(&self) -> RdowsConnection {
        RdowsConnection::connect(&self.url(), self.client_tls.clone())
            .await
            .unwrap()
    }
}

// ===========================================================================
// Phase 2: Connection
// ===========================================================================

#[tokio::test]
async fn connect_and_disconnect() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;

    let conn = server.connect().await;
    assert_ne!(conn.session_id(), 0);
    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn reject_ws_url() {
    let err = RdowsConnection::connect(
        "ws://localhost:1234/rdows",
        rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth(),
    )
    .await;

    assert!(err.is_err());
    let msg = format!("{}", err.as_ref().err().unwrap());
    assert!(msg.contains("TLS required"), "got: {msg}");
}

// ===========================================================================
// Phase 3: Memory Regions
// ===========================================================================

#[tokio::test]
async fn register_and_deregister_mr() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ, 4096)
        .await
        .unwrap();

    assert_ne!(mr.rkey.0, 0);
    assert_ne!(mr.lkey.0, 0);
    assert_eq!(mr.len, 4096);

    let lkey = mr.lkey;
    conn.dereg_mr(lkey).await.unwrap();
    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn unique_rkeys() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr1 = conn.reg_mr(AccessFlags::REMOTE_READ, 64).await.unwrap();
    let mr2 = conn.reg_mr(AccessFlags::REMOTE_READ, 64).await.unwrap();
    let mr3 = conn.reg_mr(AccessFlags::REMOTE_READ, 64).await.unwrap();

    assert_ne!(mr1.rkey, mr2.rkey);
    assert_ne!(mr2.rkey, mr3.rkey);
    assert_ne!(mr1.rkey, mr3.rkey);

    conn.disconnect().await.unwrap();
}

// ===========================================================================
// Phase 4: SEND/RECV
// ===========================================================================

#[tokio::test]
async fn post_send() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();
    let lkey = mr.lkey;

    // Write data into local MR
    conn.write_local_mr(lkey, 0, b"Hello, RDoWS!").unwrap();

    // Post send
    conn.post_send(
        1,
        &[ScatterGatherEntry {
            lkey,
            offset: 0,
            length: 13,
        }],
    )
    .await
    .unwrap();

    // Check CQ
    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].wrid.0, 1);
    assert_eq!(cqes[0].status, 0);
    assert_eq!(cqes[0].byte_count, 13);

    conn.disconnect().await.unwrap();
}

// ===========================================================================
// Phase 5: RDMA Write + Read
// ===========================================================================

#[tokio::test]
async fn rdma_write_and_read() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    // Register remote MR (server side) with write+read access
    let remote_mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ, 1024)
        .await
        .unwrap();
    let remote_rkey = remote_mr.rkey;

    // Register local MR for source data
    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 1024).await.unwrap();
    let local_lkey = local_mr.lkey;

    // Write test data into local MR
    let test_data = b"The CPU never saw this coming";
    conn.write_local_mr(local_lkey, 0, test_data).unwrap();

    // RDMA Write: local MR -> remote MR at offset 100
    conn.rdma_write(
        10,
        remote_rkey,
        100,
        &[ScatterGatherEntry {
            lkey: local_lkey,
            offset: 0,
            length: test_data.len() as u32,
        }],
    )
    .await
    .unwrap();

    // Check write CQE
    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].wrid.0, 10);
    assert_eq!(cqes[0].status, 0);

    // Register a local MR for read sink
    let read_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 1024).await.unwrap();
    let read_lkey = read_mr.lkey;

    // RDMA Read: remote MR at offset 100 -> local read MR at offset 0
    conn.rdma_read(
        20,
        remote_rkey,
        100,
        test_data.len() as u64,
        read_lkey,
        0,
    )
    .await
    .unwrap();

    // Check read CQE
    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].wrid.0, 20);
    assert_eq!(cqes[0].status, 0);

    // Verify data read back matches
    let read_back = conn
        .read_local_mr(read_lkey, 0, test_data.len())
        .unwrap();
    assert_eq!(&read_back, test_data);

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn rdma_write_access_denied() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    // Register MR with only REMOTE_READ (no REMOTE_WRITE)
    let mr = conn.reg_mr(AccessFlags::REMOTE_READ, 256).await.unwrap();

    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();
    conn.write_local_mr(local_mr.lkey, 0, b"data").unwrap();

    let err = conn
        .rdma_write(
            1,
            mr.rkey,
            0,
            &[ScatterGatherEntry {
                lkey: local_mr.lkey,
                offset: 0,
                length: 4,
            }],
        )
        .await;

    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrAccessDenied);
        }
        other => panic!("expected Protocol error, got: {other}"),
    }

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn rdma_write_bounds_error() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE, 64)
        .await
        .unwrap();

    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();
    conn.write_local_mr(local_mr.lkey, 0, &[0xAA; 128]).unwrap();

    // Try to write 128 bytes into a 64-byte region
    let err = conn
        .rdma_write(
            1,
            mr.rkey,
            0,
            &[ScatterGatherEntry {
                lkey: local_mr.lkey,
                offset: 0,
                length: 128,
            }],
        )
        .await;

    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrBounds);
        }
        other => panic!("expected Protocol error, got: {other}"),
    }

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn rdma_read_invalid_rkey() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();

    // Use a bogus R_Key
    let err = conn
        .rdma_read(
            1,
            rdows_client::rdows_core::memory::RKey(0xDEADBEEF),
            0,
            64,
            local_mr.lkey,
            0,
        )
        .await;

    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrInvalidMkey);
        }
        other => panic!("expected Protocol error, got: {other}"),
    }

    conn.disconnect().await.unwrap();
}

// ===========================================================================
// Phase 7: Atomic CAS / FAA
// ===========================================================================

/// Helper: write a big-endian u64 into remote MR at the given offset via RDMA Write.
async fn write_remote_u64(
    conn: &mut RdowsConnection,
    rkey: rdows_client::rdows_core::memory::RKey,
    offset: u64,
    value: u64,
) {
    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 8).await.unwrap();
    conn.write_local_mr(local_mr.lkey, 0, &value.to_be_bytes())
        .unwrap();
    conn.rdma_write(
        0,
        rkey,
        offset,
        &[ScatterGatherEntry {
            lkey: local_mr.lkey,
            offset: 0,
            length: 8,
        }],
    )
    .await
    .unwrap();
    conn.poll_cq(10);
}

/// Helper: read a big-endian u64 from remote MR at the given offset via RDMA Read.
async fn read_remote_u64(
    conn: &mut RdowsConnection,
    rkey: rdows_client::rdows_core::memory::RKey,
    offset: u64,
) -> u64 {
    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 8).await.unwrap();
    conn.rdma_read(0, rkey, offset, 8, local_mr.lkey, 0)
        .await
        .unwrap();
    conn.poll_cq(10);
    let data = conn.read_local_mr(local_mr.lkey, 0, 8).unwrap();
    u64::from_be_bytes(data.try_into().unwrap())
}

#[tokio::test]
async fn atomic_cas_success() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn
        .reg_mr(
            AccessFlags::REMOTE_ATOMIC | AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ,
            64,
        )
        .await
        .unwrap();

    write_remote_u64(&mut conn, mr.rkey, 0, 0xDEADBEEF).await;

    let original = conn
        .atomic_cas(1, mr.rkey, 0, 0xDEADBEEF, 0xCAFEBABE)
        .await
        .unwrap();
    assert_eq!(original, 0xDEADBEEF);

    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].wrid.0, 1);
    assert_eq!(cqes[0].status, 0);
    assert_eq!(cqes[0].byte_count, 8);

    let readback = read_remote_u64(&mut conn, mr.rkey, 0).await;
    assert_eq!(readback, 0xCAFEBABE);

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn atomic_cas_no_match() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn
        .reg_mr(
            AccessFlags::REMOTE_ATOMIC | AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ,
            64,
        )
        .await
        .unwrap();

    write_remote_u64(&mut conn, mr.rkey, 0, 0xDEADBEEF).await;

    let original = conn
        .atomic_cas(1, mr.rkey, 0, 0x1111, 0xCAFE)
        .await
        .unwrap();
    assert_eq!(original, 0xDEADBEEF);

    let readback = read_remote_u64(&mut conn, mr.rkey, 0).await;
    assert_eq!(readback, 0xDEADBEEF); // unchanged

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn atomic_faa() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn
        .reg_mr(
            AccessFlags::REMOTE_ATOMIC | AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ,
            64,
        )
        .await
        .unwrap();

    write_remote_u64(&mut conn, mr.rkey, 0, 100).await;

    let original = conn.atomic_faa(1, mr.rkey, 0, 50).await.unwrap();
    assert_eq!(original, 100);

    let readback = read_remote_u64(&mut conn, mr.rkey, 0).await;
    assert_eq!(readback, 150);

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn atomic_alignment_error() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn
        .reg_mr(AccessFlags::REMOTE_ATOMIC, 64)
        .await
        .unwrap();

    let err = conn.atomic_cas(1, mr.rkey, 3, 0, 0).await;
    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrAlignment);
        }
        other => panic!("expected ErrAlignment, got: {other}"),
    }

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn atomic_access_denied() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE, 64)
        .await
        .unwrap();

    let err = conn.atomic_cas(1, mr.rkey, 0, 0, 0).await;
    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrAccessDenied);
        }
        other => panic!("expected ErrAccessDenied, got: {other}"),
    }

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn atomic_bounds_error() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start().await;
    let mut conn = server.connect().await;

    let mr = conn
        .reg_mr(AccessFlags::REMOTE_ATOMIC, 16)
        .await
        .unwrap();

    // Offset 16 needs bytes 16..24, but MR is only 16 bytes
    let err = conn.atomic_cas(1, mr.rkey, 16, 0, 0).await;
    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrBounds);
        }
        other => panic!("expected ErrBounds, got: {other}"),
    }

    conn.disconnect().await.unwrap();
}

// ===========================================================================
// Phase 8: ERR_RNR — Posted Receive Queue
// ===========================================================================

#[tokio::test]
async fn send_exhausts_recv_queue() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start_with_config(ServerConfig { recv_queue_depth: 3 }).await;
    let mut conn = server.connect().await;

    let mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();
    conn.write_local_mr(mr.lkey, 0, b"hello").unwrap();
    let sg = vec![ScatterGatherEntry {
        lkey: mr.lkey,
        offset: 0,
        length: 5,
    }];

    // First 3 sends succeed
    for i in 1..=3u64 {
        conn.post_send(i, &sg).await.unwrap();
        let cqes = conn.poll_cq(10);
        assert_eq!(cqes.len(), 1);
        assert_eq!(cqes[0].status, 0);
    }

    // 4th send gets ERR_RNR
    let err = conn.post_send(4, &sg).await;
    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrRnr);
        }
        other => panic!("expected ErrRnr, got: {other}"),
    }
    // Drain the ERR_RNR CQE
    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].status, u16::from(ErrorCode::ErrRnr));

    // Connection still alive — RDMA Write works
    let remote_mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE, 64)
        .await
        .unwrap();
    conn.write_local_mr(mr.lkey, 0, b"alive").unwrap();
    conn.rdma_write(
        10,
        remote_mr.rkey,
        0,
        &[ScatterGatherEntry {
            lkey: mr.lkey,
            offset: 0,
            length: 5,
        }],
    )
    .await
    .unwrap();
    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].status, 0);

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn send_recv_queue_depth_one() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start_with_config(ServerConfig { recv_queue_depth: 1 }).await;
    let mut conn = server.connect().await;

    let mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();
    conn.write_local_mr(mr.lkey, 0, b"data").unwrap();
    let sg = vec![ScatterGatherEntry {
        lkey: mr.lkey,
        offset: 0,
        length: 4,
    }];

    // First send succeeds
    conn.post_send(1, &sg).await.unwrap();
    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].status, 0);

    // Second send gets ERR_RNR
    let err = conn.post_send(2, &sg).await;
    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrRnr);
        }
        other => panic!("expected ErrRnr, got: {other}"),
    }

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn send_recv_queue_depth_zero() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start_with_config(ServerConfig { recv_queue_depth: 0 }).await;
    let mut conn = server.connect().await;

    let mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();
    conn.write_local_mr(mr.lkey, 0, b"data").unwrap();
    let sg = vec![ScatterGatherEntry {
        lkey: mr.lkey,
        offset: 0,
        length: 4,
    }];

    // Very first send gets ERR_RNR
    let err = conn.post_send(1, &sg).await;
    assert!(err.is_err());
    match err.unwrap_err() {
        rdows_client::rdows_core::error::RdowsError::Protocol(code) => {
            assert_eq!(code, ErrorCode::ErrRnr);
        }
        other => panic!("expected ErrRnr, got: {other}"),
    }

    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn err_rnr_does_not_close_connection() {
    let _ = tracing_subscriber::fmt::try_init();
    let server = TestServer::start_with_config(ServerConfig { recv_queue_depth: 0 }).await;
    let mut conn = server.connect().await;

    let mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();
    conn.write_local_mr(mr.lkey, 0, b"data").unwrap();
    let sg = vec![ScatterGatherEntry {
        lkey: mr.lkey,
        offset: 0,
        length: 4,
    }];

    // Trigger ERR_RNR
    let err = conn.post_send(1, &sg).await;
    assert!(err.is_err());
    // Drain the ERR_RNR CQE
    conn.poll_cq(10);

    // RDMA Write still works
    let remote_mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ, 256)
        .await
        .unwrap();
    conn.write_local_mr(mr.lkey, 0, b"test").unwrap();
    conn.rdma_write(
        10,
        remote_mr.rkey,
        0,
        &[ScatterGatherEntry {
            lkey: mr.lkey,
            offset: 0,
            length: 4,
        }],
    )
    .await
    .unwrap();
    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].status, 0);

    // RDMA Read still works
    let read_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await.unwrap();
    conn.rdma_read(20, remote_mr.rkey, 0, 4, read_mr.lkey, 0)
        .await
        .unwrap();
    let cqes = conn.poll_cq(10);
    assert_eq!(cqes.len(), 1);
    assert_eq!(cqes[0].status, 0);
    let read_back = conn.read_local_mr(read_mr.lkey, 0, 4).unwrap();
    assert_eq!(&read_back, b"test");

    conn.disconnect().await.unwrap();
}
