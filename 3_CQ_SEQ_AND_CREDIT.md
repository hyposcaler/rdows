# Implement CQ Overflow, Sequence Validation, and Credit-Based Flow Control

## Context

RDoWS is an RDMA-over-WebSockets implementation. Three protocol features from `docs/rdows-rfc.txt` are currently stubbed or missing. This prompt covers all three since they're small and non-disruptive.

Read `docs/rdows-rfc.txt` Sections 8, 9, and 11. The existing code is in `crates/`.

---

## Feature 1: CQ Overflow Enforcement (RFC Section 8)

The spec says ERR_CQ_OVERFLOW must be returned **synchronously** when the application posts a WR that would overflow the CQ. Currently `CompletionQueue::push()` returns `false` when full but all callers ignore it.

### Changes

**`crates/rdows-client/src/verbs.rs`**: In every method that calls `self.cq.push()` (`post_send`, `rdma_write`, `rdma_read`, `atomic_cas`/`atomic_faa` or shared atomic helper), check the return value. If `push()` returns `false`, return `Err(RdowsError::Protocol(ErrorCode::ErrCqOverflow))`.

This is a pre-check -- the CQE push happens after the operation succeeds, so the WR has already completed on the wire. To make this truly synchronous per spec, add a `cq_has_space()` or check `self.cq.len() < capacity` **before** sending the WR on the wire. If no space, return ERR_CQ_OVERFLOW without sending anything.

Add a `pub fn is_full(&self) -> bool` method to `CompletionQueue`.

### Tests

- `cq_overflow`: Create an `RdowsConnection`, set CQ capacity low (expose a way to configure this, or use the existing `DEFAULT_CQ_CAPACITY` and just fill it). Post enough successful operations to fill the CQ without polling, then attempt one more. Expect ERR_CQ_OVERFLOW. Verify that polling the CQ drains entries and subsequent operations succeed again.

Note: The default CQ capacity is 65536 which is too large to fill in a test. Add a way to configure CQ capacity on `RdowsConnection`. Options:
- A `connect_with_config()` method that takes a config struct including cq_capacity
- Or just add a `set_cq_capacity()` method that can be called right after connect (simpler, test-only in practice)

Go with a `ConnectConfig` struct with `cq_capacity: usize` and a `connect_with_config()` method. Keep `connect()` as-is using defaults.

---

## Feature 2: Sequence Number Validation (RFC Section 5.1, 10, 11)

Both sides increment sequence numbers but neither validates incoming sequences. Per spec, sequence numbers are monotonically increasing, used for duplicate detection and ordered delivery validation. A gap should produce ERR_SEQ_GAP (0x0030).

### Changes

**`crates/rdows-server/src/session.rs`**: Add `expected_remote_seq: Option<u32>` to `Session`. Initialize to `None`. On every received message in `handle_message()`:
- If `expected_remote_seq` is `None`, set it to `msg.header().sequence + 1` (bootstrapping from first message, which is CONNECT at seq 0)
- If `expected_remote_seq` is `Some(expected)` and `msg.header().sequence != expected`, send ERROR with ERR_SEQ_GAP and close
- Otherwise, set `expected_remote_seq = Some(msg.header().sequence.wrapping_add(1))`

**`crates/rdows-client/src/connection.rs`** or **`lib.rs`**: Add `expected_remote_seq: Option<u32>` to `RdowsConnection`. Same validation logic in `send_and_recv()` when processing the response. On mismatch, return `Err(RdowsError::Protocol(ErrorCode::ErrSeqGap))`.

### Tests

- `sequence_validation_normal`: Connect, do several operations, verify all succeed (sequences are correct by default). This is really just verifying existing tests still pass.
- `sequence_gap_detected`: This is hard to test in integration without a way to inject bad sequences. Skip integration test for this, rely on unit tests. Add a unit test in `rdows-core` that creates a scenario with mismatched sequences if feasible, or just verify the tracking logic with a small state-machine test in the server.

Actually the simplest integration-level test: after connecting, verify that `session.expected_remote_seq` has been set correctly. This is more of a "didn't break anything" validation. The real protection is against bugs or malicious clients, not something easily triggered in a well-behaved test.

---

## Feature 3: Credit-Based Flow Control (RFC Section 9)

ICC (Initial Credit Count) is advertised as 65535 in CONNECT but CREDIT_UPDATE is accepted and ignored. No actual enforcement. Per spec, each SEND consumes one credit, and when credits hit zero the client must stop sending. Credits only apply to SEND, not to RDMA Write/Read/Atomic.

### Changes

**Client side (`crates/rdows-client/src/lib.rs` and `verbs.rs`)**:

Add `send_credits: u32` to `RdowsConnection`. Initialize from the server's ICC value in the CONNECT_ACK payload during `connect()`.

In `post_send()`: before sending, check `self.send_credits > 0`. If zero, return a new error variant. Add `RdowsError::SendCreditsExhausted` to `rdows-core/src/error.rs` (or reuse an existing error -- but a dedicated variant is cleaner since this is a local enforcement, not a wire error).

On successful send, decrement `self.send_credits`.

Handle incoming CREDIT_UPDATE: the tricky part is that CREDIT_UPDATE can arrive at any time. Since the client's `send_and_recv` is synchronous, handle it inline. In `send_and_recv()`, when waiting for a response, if a CREDIT_UPDATE arrives instead of the expected message, process it (increment `send_credits`) and continue waiting for the expected response. This means modifying `send_and_recv` to loop:

```rust
loop {
    let msg = connection::recv_message(&mut self.stream).await?;
    match &msg {
        RdowsMessage::CreditUpdate(_, payload) => {
            self.send_credits += payload.credit_increment;
            continue;
        }
        _ => return Ok(msg),
    }
}
```

**Server side (`crates/rdows-server/src/session.rs` and `handler.rs`)**:

Add `sends_since_last_credit: u32` and `icc: u32` to `Session`.

After successfully processing a SEND (in `handle_send_data` after sending RECV_COMP), increment `sends_since_last_credit`. If `sends_since_last_credit >= icc / 4`, send a CREDIT_UPDATE with `credit_increment = sends_since_last_credit` and reset the counter to zero.

The server's ICC is already `DEFAULT_ICC` (65535). Use that.

### Tests

- `send_credits_enforced`: Start server with default config. Create client with a manually reduced `send_credits` (set it to 2 after connect). Send twice successfully. Third send should fail with SendCreditsExhausted. Verify it's a local error, not a wire error.
- `credit_update_replenishes`: This is the full flow test. Start server with small `recv_queue_depth` (say 8) and set ICC to 4 in the CONNECT_ACK. Client sends 4 (exhausting credits), then the server should have sent a CREDIT_UPDATE after processing ~1 send (ICC/4 = 1). Verify client can send more after receiving the credit update.

Actually, testing the full async credit flow is complex with the current synchronous model. Simpler approach: just test the enforcement side (credits decrement and error when zero) and test CREDIT_UPDATE parsing in isolation. The CREDIT_UPDATE handling in the `send_and_recv` loop will get exercised naturally once the server starts sending them.

Simplify tests to:
- `send_credits_exhausted`: Connect, manually set `send_credits = 2` on the connection, send twice (succeed), third send fails with SendCreditsExhausted before hitting the wire.
- `credit_update_received`: This can be verified indirectly -- if the server sends CREDIT_UPDATE and the client's recv loop handles it, existing send tests should still pass without hanging. The `send_and_recv` loop change is what matters.

To make `send_credits` settable for testing, add a `pub fn set_send_credits(&mut self, credits: u32)` method on `RdowsConnection`. It's test-only in practice but doesn't need to be behind cfg(test).

---

## What NOT to do

- Don't change any wire formats or message types
- Don't add RNR retry logic (already handled in the previous feature)
- Don't implement fragmentation (separate feature)
- Don't block the send path waiting for CREDIT_UPDATE -- just error when credits are zero, let the caller decide to retry

## Validation

Run `cargo test --workspace && cargo clippy --workspace` after implementation. All existing tests must continue to pass. The send_credits default (from ICC=65535) is high enough that existing tests won't be affected.