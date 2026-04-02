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
|           Application (ibverbs-style API)        |
+--------------------------------------------------+
|              RDoWS Protocol Layer                |
|  (Memory Regions, Queue Pairs, Completion Qs)    |
+--------------------------------------------------+
|           RDoWS Message Framing Layer            |
+--------------------------------------------------+
|    WebSocket (RFC 6455) Binary Message Layer     |
+--------------------------------------------------+
|              TLS 1.3 (REQUIRED)                  |
+--------------------------------------------------+
|                      TCP                         |
+--------------------------------------------------+
```

The implementation consists of four crates:

- **`rdows-core`** — Wire types, 24-byte frame header codec, opcode
  definitions, and message payload encode/decode. No async runtime
  dependency.
- **`rdows-server`** — TLS-enabled WebSocket server with session state
  machine, memory region store, and opcode dispatch.
- **`rdows-client`** — Client library exposing an ibverbs-compatible API:
  `reg_mr`, `dereg_mr`, `post_send`, `rdma_write`, `rdma_read`,
  `atomic_cas`, `atomic_faa`, `poll_cq`.
- **`rdows-kv`** — Key-value store demo with web UI. Uses RDMA Read/Write
  against a 64 KiB memory region organized as a hash table. Embeds its own
  server for single-host use, or connects to a remote server.

## Quick Start (Single Host)

Every example and the KV store demo embed their own RDoWS server with
ephemeral TLS certificates — no cert generation or separate server process
needed.

```sh
# Two-sided SEND/RECV
cargo run -p rdows-client --example echo_send_recv

# One-sided RDMA Write + Read
cargo run -p rdows-client --example one_sided_write

# Random-access RDMA Read
cargo run -p rdows-client --example one_sided_read

# Atomic FAA counter
cargo run -p rdows-client --example atomic_counter

# ERR_RNR: receive queue exhaustion and recovery
cargo run -p rdows-client --example err_rnr_demo
```

### KV Store Demo

A key-value store where GET and PUT are implemented as RDMA Read and Write
into a remote memory region. Includes a web UI with a live memory region
visualizer.

```sh
cargo run -p rdows-kv
# Open http://localhost:8080 in your browser
```

The backing store is a 64-slot hash table (64 KiB) laid out in a single
memory region. The web UI shows the slot grid, hex view of raw RDMA reads,
and an operation log.

## Two-Host Deployment

RDoWS runs across separate hosts over the network. Both hosts need Rust
installed (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`).

### Server Setup

Generate a cert with the server's IP in the SAN and start the server:

```sh
openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout server.key -out server.crt -days 365 \
    -subj "/CN=SERVER_IP" \
    -addext "subjectAltName=IP:SERVER_IP" \
    -addext "basicConstraints=CA:FALSE"

cargo run -p rdows-server -- --bind 0.0.0.0:9443 --cert server.crt --key server.key
```

For the ERR_RNR demo, limit the receive queue depth so it exhausts quickly:

```sh
cargo run -p rdows-server -- --bind 0.0.0.0:9443 --cert server.crt --key server.key --recv-queue-depth 3
```

### Client Setup

Copy `server.crt` from the server host:

```sh
scp SERVER_IP:~/path/to/server.crt .
```

### Client Demos

All demos use `--url` and `--cert` to connect to the remote server. If
`--cert` is omitted, the system trust store is used (for CA-signed certs).

```sh
# RDMA Write + Read (default) — writes data into remote memory, reads it back
cargo run -p rdows-client -- --url wss://SERVER_IP:9443/rdows --cert server.crt

# Random-access RDMA Read — populates remote MR, reads records in reverse order
cargo run -p rdows-client -- --url wss://SERVER_IP:9443/rdows --cert server.crt --mode read

# Two-sided SEND/RECV — posts a SEND message to the server
cargo run -p rdows-client -- --url wss://SERVER_IP:9443/rdows --cert server.crt --mode echo

# Atomic FAA counter — increments a remote counter 5 times via Fetch-and-Add
cargo run -p rdows-client -- --url wss://SERVER_IP:9443/rdows --cert server.crt --mode atomic

# ERR_RNR — exhaust the server's receive queue (use --recv-queue-depth on server)
cargo run -p rdows-client -- --url wss://SERVER_IP:9443/rdows --cert server.crt --mode err_rnr

# Control how many SENDs to attempt
cargo run -p rdows-client -- --url wss://SERVER_IP:9443/rdows --cert server.crt --mode err_rnr --sends 10
```

### KV Store (Remote)

Run the KV store web UI against a remote RDoWS server:

```sh
cargo run -p rdows-kv -- --remote wss://SERVER_IP:9443/rdows --cert server.crt

# Or skip TLS verification for quick testing:
cargo run -p rdows-kv -- --remote wss://SERVER_IP:9443/rdows --insecure
```

Open `http://localhost:8080` — every PUT/GET/DELETE in the browser becomes an
RDMA Write/Read over WebSocket to the remote host's memory.


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


### Client side showing atomics
```
 cargo run -p rdows-client -- --url wss://10.1.0.22:9443/rdows --cert server.crt --mode atomic
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.02s
     Running `target/debug/rdows-client --url 'wss://10.1.0.22:9443/rdows' --cert server.crt --mode atomic`
Loaded trust anchor from server.crt
Connecting to wss://10.1.0.22:9443/rdows...
Session established (id: 0xFD03D7CA)
Remote MR registered: R_Key=0x5B196DF8, size=64
Counter initialized to 0
FAA #1: previous value = 0
FAA #2: previous value = 1
FAA #3: previous value = 2
FAA #4: previous value = 3
FAA #5: previous value = 4
Final counter value: 5
Verification passed.
Disconnected.
```

### ERR_RNR: Receive Queue Exhaustion

Per RFC Section 7.1, the server maintains a posted receive queue for SEND
operations. Each incoming SEND consumes one receive. When the queue is
exhausted, subsequent SENDs are rejected with ERR_RNR (0x0010). This is a
recoverable error — the connection remains alive and RDMA Write, Read, and
Atomic operations continue to work.

The `--recv-queue-depth <N>` server flag controls the initial queue depth
(default 128). The queue drains without replenishment. The `err_rnr_demo`
example demonstrates this with a depth of 3:

```
$ cargo run -p rdows-client --example err_rnr_demo
Session established (id: 0x312945CA)

SEND #1: OK (CQE: wrid=1, status=0x0000, bytes=15)
SEND #2: OK (CQE: wrid=2, status=0x0000, bytes=15)
SEND #3: OK (CQE: wrid=3, status=0x0000, bytes=15)
SEND #4: ERR_RNR (CQE: wrid=4, status=0x0010)

Connection still alive — performing RDMA Write...
RDMA Write: OK (CQE: wrid=100, status=0x0000)

Server receive queue exhausted after 3 SENDs. Connection remains usable for RDMA operations.
```

### Server side showing ERR_RNR

```
hyposcaler@vm-builder:~/src/rdows$ cargo run -p rdows-server -- --bind 0.0.0.0:9443 --cert server.crt --key server.key --recv-queue-depth 3
   Compiling rdows-server v0.1.0 (/storage/src/rdows/crates/rdows-server)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.68s
     Running `target/debug/rdows-server --bind '0.0.0.0:9443' --cert server.crt --key server.key --recv-queue-depth 3`
2026-04-01T21:57:52.831256Z  INFO rdows_server: starting RDoWS server bind=0.0.0.0:9443
2026-04-01T21:57:52.831281Z  INFO rdows_server: RDoWS server listening addr=0.0.0.0:9443
2026-04-01T21:58:09.644930Z  INFO rdows_server: WebSocket upgrade complete peer=10.0.0.158:46386
2026-04-01T21:58:09.645648Z  INFO rdows_server::session: session established session_id=1242882738 max_msg_size=16777216
2026-04-01T21:58:09.868345Z  INFO rdows_server::session: disconnect received session_id=1242882738
2026-04-01T21:58:09.868376Z  INFO rdows_server::session: session ended session_id=1242882738
```

### Client side showing ERR_RNR 

```
❯  cargo run -p rdows-client -- --url wss://10.1.0.22:9443/rdows --cert server.crt --mode err_rnr
   Compiling rdows-client v0.1.0 (/home/hyposcaler/RDoWS/crates/rdows-client)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.11s
     Running `target/debug/rdows-client --url 'wss://10.1.0.22:9443/rdows' --cert server.crt --mode err_rnr`
Loaded trust anchor from server.crt
Connecting to wss://10.1.0.22:9443/rdows...
Session established (id: 0x4A14E2B2)
SEND #1: OK (CQE: wrid=1, status=0x0000, bytes=15)
SEND #2: OK (CQE: wrid=2, status=0x0000, bytes=15)
SEND #3: OK (CQE: wrid=3, status=0x0000, bytes=15)
SEND #4: ERR_RNR (CQE: wrid=4, status=0x0010)
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

// Atomic Compare-and-Swap (returns original value)
let original = conn.atomic_cas(wrid, mr.rkey, remote_va, compare, swap).await?;

// Atomic Fetch-and-Add (returns original value)
let original = conn.atomic_faa(wrid, mr.rkey, remote_va, addend).await?;

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
| Atomic | ATOMIC_REQ, ATOMIC_RESP | 64-bit CAS and Fetch-and-Add |
| Control | ACK, CREDIT_UPDATE, ERROR | Flow and error control |

Remote Keys (R_Keys) are generated using a cryptographically secure PRNG
and are scoped to the session's Protection Domain. R_Keys are never reused
within a session, even after deregistration.

## Testing

```sh
cargo test --workspace
cargo clippy --workspace
```

### Wireshark Capture

![Wireshark capture showing decrypted RDoWS WRITE_DATA frame with full protocol dissection](docs/wireshark-capture.png)

## Wireshark Dissector

A Lua dissector for Wireshark is included in `wireshark/rdows.lua`. It parses
the 24-byte RDoWS header and all opcode payloads inside decrypted WebSocket
binary frames. See [wireshark/README.md](wireshark/README.md) for setup.

## References

- [RDoWS Protocol Specification](docs/rdows-rfc.txt) (RFC XXXX)
- [InfiniBand Architecture Specification](https://www.infinibandta.org/)
- [RFC 6455 — The WebSocket Protocol](https://www.rfc-editor.org/rfc/rfc6455)
- [RFC 8446 — TLS 1.3](https://www.rfc-editor.org/rfc/rfc8446)

## License

This project is provided as-is for educational and research purposes.
