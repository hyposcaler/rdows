# Implement Posted Receive WRs and ERR_RNR (RFC Section 7.1)

## Context

RDoWS is an RDMA-over-WebSockets implementation. The server currently auto-accepts all incoming SENDs without requiring a posted Receive Work Request. Per RFC Section 7.1, the remote endpoint must have a posted Receive WR before a SEND arrives, and if none is posted, the server must respond with ERR_RNR (0x0010). We need to implement this properly.

Read `docs/rdows-rfc.txt` Section 7.1 and Section 9 for the spec. The existing code is in `crates/`.

## Design

The server gets a receive queue with a configurable initial depth. At session start, the server auto-posts that many receives. Each incoming SEND consumes one. When the queue is exhausted, the server responds with ERR_RNR. No auto-replenishment -- the queue drains. This is intentional for demo purposes and makes ERR_RNR easy to demonstrate.

## What to implement

### 1. Server: `session.rs` — add receive queue to Session

Add a `recv_queue_depth: u32` field to `Session` representing how many receives are available. Initialize it from a configurable value (default 128). Each time a SEND is successfully processed, decrement by 1. When it hits 0, subsequent SENDs get ERR_RNR.

Add a `--recv-queue-depth <N>` CLI flag to `crates/rdows-server/src/main.rs`. Thread this value through to session creation. The server lib's `run_server` function will need to accept this parameter.

### 2. Server: `handler.rs` — enforce posted receives in SEND handling

Modify `handle_send()` to check `session.recv_queue_depth > 0` before accepting the SEND:
- If depth > 0: decrement depth, proceed as before (set pending op to AwaitingSendData)
- If depth == 0: send ERROR with ERR_RNR, do NOT set pending op, do NOT consume the SEND

The ERR_RNR error is recoverable per RFC Section 11 -- it does NOT close the connection. The client can retry or give up.

### 3. Server: `lib.rs` — update `run_server` signature

`run_server` currently takes `(TcpListener, TlsAcceptor)`. Add a config struct or parameter for recv_queue_depth so it flows from main.rs through to session creation. Keep it simple -- a struct like:

```rust
pub struct ServerConfig {
    pub recv_queue_depth: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { recv_queue_depth: 128 }
    }
}
```

Update `run_server` to accept this. Update `handle_connection` and `run_session` to pass it through. Update all call sites including the embedded servers in tests and examples.

### 4. Client: `verbs.rs` — handle ERR_RNR response to SEND

Currently `post_send()` sends SEND + SEND_DATA, then awaits RECV_COMP. It needs to also handle the case where the server responds with an ERROR containing ERR_RNR instead of RECV_COMP.

When ERR_RNR is received:
- Push a CQE with status set to the ERR_RNR error code
- Return `Err(RdowsError::Protocol(ErrorCode::ErrRnr))`

Note: the client will have already sent the SEND_DATA by the time it gets ERR_RNR back. That's fine -- the server just discards the SEND_DATA since it never set the pending op. But we need to handle the case where the server sends ERROR before the client sends SEND_DATA. The simplest approach: keep the current flow (send SEND, send SEND_DATA, then check response), since the server won't read the SEND_DATA payload when it rejected the SEND with ERR_RNR.

Actually, there's a subtlety here. Currently handle_send on the server sets `pending_op = AwaitingSendData` and returns Ok. The SEND_DATA arrives next and handle_send_data checks the pending op. If we reject at the SEND stage with ERR_RNR and don't set the pending op, the subsequent SEND_DATA will hit the "unexpected SEND_DATA without preceding SEND" error path. 

Fix: when the server rejects SEND with ERR_RNR, it needs to still consume and discard the following SEND_DATA. Options:
- Set a `PendingOp::DiscardingSendData` state that silently drops the next SEND_DATA
- Have the client not send SEND_DATA if SEND was rejected (requires the client to wait for a response between SEND and SEND_DATA)

Option A (DiscardingSendData) is simpler and doesn't change the client's send flow. Go with that.

Add a `DiscardingSendData` variant to `PendingOp` in `handler.rs`. When ERR_RNR fires:
1. Send ERROR with ERR_RNR
2. Set `pending_op = PendingOp::DiscardingSendData`

In `handle_send_data`, add a match arm for `DiscardingSendData` that silently drops the data and returns Ok without sending any response.

The client's `post_send` will then receive the ERR_RNR error on the recv side after it has already sent both messages.

### 5. Integration tests in `crates/rdows-client/tests/integration.rs`

The `TestServer` helper needs updating to accept a recv_queue_depth config. Add a `TestServer::start_with_config(config)` method alongside the existing `start()` (which uses defaults).

Add these tests:

- `send_exhausts_recv_queue`: Start server with `recv_queue_depth: 3`. Send 3 messages successfully (verify CQEs). Send a 4th, expect ERR_RNR error. Verify connection is still alive by checking that we can do an RDMA Write after the ERR_RNR (since ERR_RNR is recoverable).
- `send_recv_queue_depth_one`: Start server with `recv_queue_depth: 1`. First send succeeds. Second send gets ERR_RNR.
- `send_recv_queue_depth_zero`: Start server with `recv_queue_depth: 0`. Very first send gets ERR_RNR immediately.
- `err_rnr_does_not_close_connection`: After getting ERR_RNR, verify RDMA Write and RDMA Read still work. This proves ERR_RNR is recoverable per spec.

### 6. Example: `crates/rdows-client/examples/err_rnr_demo.rs`

Demonstrate the receive queue mechanics:
- Start embedded server with `recv_queue_depth: 3`
- Register a local MR, write message data into it
- Send 4 messages in a loop, printing success/failure for each
- Show that sends 1-3 succeed and send 4 returns ERR_RNR
- After ERR_RNR, do an RDMA Write to prove the connection is still alive
- Print a summary: "Server receive queue exhausted after 3 SENDs. Connection remains usable for RDMA operations."
- Use the same embedded server pattern as the other examples

## What NOT to do

- Don't implement receive queue replenishment or auto-repost
- Don't implement CREDIT_UPDATE flow control (still accept and ignore per current behavior)
- Don't make the client post receives -- only the server side needs this for now
- Don't change any wire formats or message types
- Don't add RNR retry logic on the client side -- just return the error

## Validation

Run `cargo test --workspace && cargo clippy --workspace` after implementation. All existing tests must continue to pass. The existing `post_send` test uses the default config (recv_queue_depth: 128) so it should be unaffected.