# RDoWS: RDMA over WebSockets

RDoWS is a protocol implementation that enables Remote Direct Memory Access
(RDMA) semantics over WebSocket transport. It provides memory region
registration, one-sided read/write operations, queue pairs, and completion
queues for environments where InfiniBand, RoCE, or iWARP hardware is
unavailable or where port constraints mandate WebSocket connectivity.

The protocol is specified in [RFC XXXX](docs/rdows-rfc.txt), which defines
the wire format, session lifecycle, memory key management, and error handling
in full. This implementation follows the RFC faithfully.

RDoWS operates entirely in user space. It does not require kernel bypass,
specialized network interface cards, or any hardware beyond a standard TCP
stack. Latency and throughput characteristics are bounded by the underlying
TCP and WebSocket layers.

## Architecture

```
+--------------------------------------------------+
|           Application (ibverbs-style API)         |
+--------------------------------------------------+
|              RDoWS Protocol Layer                 |
|  (Memory Regions, Queue Pairs, Completion Qs)    |
+--------------------------------------------------+
|           RDoWS Message Framing Layer             |
+--------------------------------------------------+
|    WebSocket (RFC 6455) Binary Message Layer      |
+--------------------------------------------------+
|              TLS 1.3 (REQUIRED)                   |
+--------------------------------------------------+
|                      TCP                          |
+--------------------------------------------------+
```

The implementation consists of three crates:

- **`rdows-core`** — Wire types, 24-byte frame header codec, opcode
  definitions, and message payload encode/decode. No async runtime
  dependency.
- **`rdows-server`** — TLS-enabled WebSocket server with session state
  machine, memory region store, and opcode dispatch.
- **`rdows-client`** — Client library exposing an ibverbs-compatible API:
  `reg_mr`, `dereg_mr`, `post_send`, `rdma_write`, `rdma_read`, `poll_cq`.

## Quick Start (Single Host)

Generate self-signed TLS certificates for development:

```sh
./gen-certs.sh
```

Start the server:

```sh
cargo run -p rdows-server -- --bind 127.0.0.1:9443 --cert server.crt --key server.key
```

Run the examples (each starts an embedded server with ephemeral certificates):

```sh
# Two-sided SEND/RECV
cargo run -p rdows-client --example echo_send_recv

# One-sided RDMA Write + Read
cargo run -p rdows-client --example one_sided_write

# Random-access RDMA Read
cargo run -p rdows-client --example one_sided_read
```

## Two-Host Deployment

RDoWS runs across separate hosts over the network. Both hosts need Rust
installed (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`).

**Server host** — generate a cert with the server's IP in the SAN and start
the server:

```sh
openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout server.key -out server.crt -days 365 \
    -subj "/CN=SERVER_IP" \
    -addext "subjectAltName=IP:SERVER_IP" \
    -addext "basicConstraints=CA:FALSE"

cargo run -p rdows-server -- --bind 0.0.0.0:9443 --cert server.crt --key server.key
```

**Client host** — copy `server.crt` from the server and run the client:

```sh
scp SERVER_IP:~/path/to/server.crt .
cargo run -p rdows-client -- --url wss://SERVER_IP:9443/rdows --cert server.crt
```

The client performs an RDMA Write into the server's memory region, reads it
back, and verifies the round-trip. If `--cert` is omitted, the system trust
store is used (suitable for CA-signed certificates).


## Example 

### Server started

```
hyposcaler@vm-builder:~/src/rdows$ cargo run -p rdows-server -- --bind 0.0.0.0:9443 --cert server.crt --key server.key
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.03s
     Running `target/debug/rdows-server --bind '0.0.0.0:9443' --cert server.crt --key server.key`
2026-04-01T18:11:02.346786Z  INFO rdows_server: starting RDoWS server bind=0.0.0.0:9443
2026-04-01T18:11:02.346809Z  INFO rdows_server: RDoWS server listening addr=0.0.0.0:9443
```

### Client run

```
❯  cargo run -p rdows-client -- --url wss://10.1.0.22:9443/rdows --cert server.crt
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.02s
     Running `target/debug/rdows-client --url 'wss://10.1.0.22:9443/rdows' --cert server.crt`
Loaded trust anchor from server.crt
Connecting to wss://10.1.0.22:9443/rdows...
Session established (id: 0xFB3F6624)
Remote MR registered: R_Key=0x3D6135CE, size=4096
RDMA Write: 54 bytes -> remote VA 0x0000
Write complete: status=0x0000
RDMA Read: 54 bytes <- remote VA 0x0000
Read complete: status=0x0000
Data: "RDMA over WebSockets: because InfiniBand was too easy."
Verification passed.
Disconnected.
```

### Observed on Server side
```
2026-04-01T18:11:05.647986Z  INFO rdows_server: WebSocket upgrade complete peer=10.0.0.158:44810
2026-04-01T18:11:05.690227Z  INFO rdows_server::session: session established session_id=4215236132 max_msg_size=16777216
2026-04-01T18:11:05.736776Z  INFO rdows_server::session: disconnect received session_id=4215236132
2026-04-01T18:11:05.736807Z  INFO rdows_server::session: session ended session_id=4215236132
```

## API

The client API mirrors the ibverbs verb model:

```rust
use rdows_client::RdowsConnection;
use rdows_client::rdows_core::memory::AccessFlags;
use rdows_client::rdows_core::queue::ScatterGatherEntry;

let mut conn = RdowsConnection::connect("wss://host:9443/rdows", tls_config).await?;

// Register a remote memory region (4 KiB, read+write)
let mr = conn.reg_mr(
    AccessFlags::REMOTE_WRITE | AccessFlags::REMOTE_READ,
    4096,
).await?;

// RDMA Write into remote memory
conn.rdma_write(wrid, mr.rkey, remote_va, &sg_list).await?;

// RDMA Read from remote memory
conn.rdma_read(wrid, mr.rkey, remote_va, len, local_lkey, local_va).await?;

// Poll completions
let cqes = conn.poll_cq(16);

conn.disconnect().await?;
```

## Protocol

Every RDoWS message is a single WebSocket binary frame containing a fixed
24-byte header followed by an opcode-specific payload. The header carries
the protocol version, opcode, flags, session ID, sequence number, work
request ID, and payload length — all in network byte order.

The protocol supports:

| Operation | Opcodes | Description |
|-----------|---------|-------------|
| Connection | CONNECT, CONNECT_ACK, DISCONNECT | Session lifecycle |
| Memory | MR_REG, MR_REG_ACK, MR_DEREG, MR_DEREG_ACK | Region management |
| Two-sided | SEND, SEND_DATA, RECV_COMP | Posted receive model |
| RDMA Write | WRITE, WRITE_DATA, WRITE_COMP | One-sided write |
| RDMA Read | READ_REQ, READ_RESP | One-sided read |
| Control | ACK, CREDIT_UPDATE, ERROR | Flow and error control |

Remote Keys (R_Keys) are generated using a cryptographically secure PRNG
and are scoped to the session's Protection Domain. R_Keys are never reused
within a session, even after deregistration.

## Testing

```sh
cargo test --workspace
cargo clippy --workspace
```

## References

- [RDoWS Protocol Specification](docs/rdows-rfc.txt) (RFC XXXX)
- [InfiniBand Architecture Specification](https://www.infinibandta.org/)
- [RFC 6455 — The WebSocket Protocol](https://www.rfc-editor.org/rfc/rfc6455)
- [RFC 8446 — TLS 1.3](https://www.rfc-editor.org/rfc/rfc8446)

## License

This project is provided as-is for educational and research purposes.
