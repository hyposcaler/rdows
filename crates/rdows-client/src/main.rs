use std::env;

use rdows_client::rdows_core::error::ErrorCode;
use rdows_client::rdows_core::memory::AccessFlags;
use rdows_client::rdows_core::queue::ScatterGatherEntry;
use rdows_client::RdowsConnection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = env::args().collect();
    let url = get_arg(&args, "--url")
        .unwrap_or_else(|| "wss://localhost:9443/rdows".to_string());
    let cert_path = get_arg(&args, "--cert");

    let mode = get_arg(&args, "--mode").unwrap_or_else(|| "write".to_string());
    let tls_config = build_tls_config(cert_path.as_deref())?;

    println!("Connecting to {url}...");
    let mut conn = RdowsConnection::connect(&url, tls_config).await?;
    println!("Session established (id: 0x{:08X})", conn.session_id());

    let sends: u64 = get_arg(&args, "--sends")
        .map(|s| s.parse().expect("--sends must be a number"))
        .unwrap_or(4);

    match mode.as_str() {
        "write" => demo_write(&mut conn).await?,
        "atomic" => demo_atomic(&mut conn).await?,
        "err_rnr" => demo_err_rnr(&mut conn, sends).await?,
        other => {
            eprintln!("unknown mode: {other} (valid: write, atomic, err_rnr)");
            std::process::exit(1);
        }
    }

    conn.disconnect().await?;
    println!("Disconnected.");
    Ok(())
}

async fn demo_write(conn: &mut RdowsConnection) -> Result<(), Box<dyn std::error::Error>> {
    let remote_mr = conn
        .reg_mr(AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ, 4096)
        .await?;
    println!(
        "Remote MR registered: R_Key=0x{:08X}, size=4096",
        remote_mr.rkey.0
    );

    let local_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 4096).await?;

    let payload = b"RDMA over WebSockets: because InfiniBand was too easy.";
    conn.write_local_mr(local_mr.lkey, 0, payload)?;

    println!("RDMA Write: {} bytes -> remote VA 0x0000", payload.len());
    conn.rdma_write(
        1,
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
        "Write complete: status=0x{:04X}",
        cqes.first().map(|c| c.status).unwrap_or(0xFFFF)
    );

    let read_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 4096).await?;

    println!("RDMA Read: {} bytes <- remote VA 0x0000", payload.len());
    conn.rdma_read(2, remote_mr.rkey, 0, payload.len() as u64, read_mr.lkey, 0).await?;

    let cqes = conn.poll_cq(10);
    println!(
        "Read complete: status=0x{:04X}",
        cqes.first().map(|c| c.status).unwrap_or(0xFFFF)
    );

    let read_back = conn.read_local_mr(read_mr.lkey, 0, payload.len())?;
    println!("Data: {:?}", std::str::from_utf8(&read_back)?);
    assert_eq!(&read_back, payload);
    println!("Verification passed.");
    Ok(())
}

async fn demo_atomic(conn: &mut RdowsConnection) -> Result<(), Box<dyn std::error::Error>> {
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
    for i in 1..=5u64 {
        let prev = conn.atomic_faa(i, remote_mr.rkey, 0, 1).await?;
        conn.poll_cq(10);
        println!("FAA #{i}: previous value = {prev}");
    }

    // Read back final value
    let read_mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 64).await?;
    conn.rdma_read(100, remote_mr.rkey, 0, 8, read_mr.lkey, 0).await?;
    conn.poll_cq(10);
    let final_bytes = conn.read_local_mr(read_mr.lkey, 0, 8)?;
    let final_value = u64::from_be_bytes(final_bytes.try_into().unwrap());
    println!("Final counter value: {final_value}");
    assert_eq!(final_value, 5);
    println!("Verification passed.");
    Ok(())
}

async fn demo_err_rnr(conn: &mut RdowsConnection, sends: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mr = conn.reg_mr(AccessFlags::LOCAL_WRITE, 256).await?;
    let message = b"Hello from SEND";
    conn.write_local_mr(mr.lkey, 0, message)?;
    let sg = vec![ScatterGatherEntry {
        lkey: mr.lkey,
        offset: 0,
        length: message.len() as u32,
    }];

    let mut succeeded = 0u64;
    for i in 1..=sends {
        print!("SEND #{i}: ");
        match conn.post_send(i, &sg).await {
            Ok(()) => {
                let cqes = conn.poll_cq(10);
                println!(
                    "OK (CQE: wrid={}, status=0x{:04X}, bytes={})",
                    cqes[0].wrid.0, cqes[0].status, cqes[0].byte_count
                );
                succeeded += 1;
            }
            Err(rdows_client::rdows_core::error::RdowsError::Protocol(ErrorCode::ErrRnr)) =>
            {
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

    println!(
        "\nServer receive queue exhausted after {succeeded} SENDs. \
         Connection remains usable for RDMA operations."
    );
    Ok(())
}

fn build_tls_config(cert_path: Option<&str>) -> Result<rustls::ClientConfig, Box<dyn std::error::Error>> {
    let mut root_store = rustls::RootCertStore::empty();

    if let Some(path) = cert_path {
        // Load a PEM cert file (self-signed or custom CA)
        let pem = std::fs::read(path)?;
        let certs: Vec<_> = rustls_pemfile::certs(&mut &*pem).collect::<Result<Vec<_>, _>>()?;
        for cert in certs {
            root_store.add(cert)?;
        }
        println!("Loaded trust anchor from {path}");
    } else {
        // Use system roots
        let native = rustls_native_certs::load_native_certs();
        for cert in native.certs {
            let _ = root_store.add(cert);
        }
        println!("Using system trust store ({} roots)", root_store.len());
    }

    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth())
}

fn get_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}
