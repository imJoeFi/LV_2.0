# MDB actor follow-up work

This file records the known gaps in the Tokio-based MDB cashless-device actor.
The current implementation is a useful development checkpoint, but it is not
yet a claim of production readiness or complete MDB Level 1 conformance.

The original synchronous harness was exercised on the physical AP 113. The new
actor currently has simulated-adapter tests only; the vending-machine hardware
was not connected during this review.

## Must fix before handling real payments

### 1. Correlate every response with the adapter/VMC acknowledgement

`Link::send` currently succeeds after writing and flushing the serial frame.
Most operations then advance their state immediately, while incoming `ACK`,
`NAK`, and `RET` messages are not correlated with the command that produced
them. `NAK` and `RET` are currently only traced. Session teardown is the one
implemented exception: after `SESSION COMPLETE`, the actor retains the active
session until `END SESSION` is ACKed so a new `BEGIN SESSION` cannot overwrite
the adapter's pending response.

This is especially dangerous for `VEND APPROVED`: a later `RESET` can be
reported as `VendSucceeded::AssumedAfterReset` even if the approval was never
accepted by the VMC.

`DISPLAY REQUEST` has the same acknowledgement limitation. The harness waits a
second after `BEGIN SESSION` before its first display request to avoid writing
two responses back-to-back, but that delay is only a best-effort workaround and
does not prove that the adapter or VMC accepted either response.

Before release:

- qualify the WAFER adapter's exact host-side ACK/NAK/RET contract;
- keep at most one response in flight and retain its frame until it is settled;
- complete public operations only after the relevant acknowledgement;
- retransmit or fail deterministically on `RET`, `NAK`, silence, and disconnect;
- add tests for approval/reset without ACK, NAK, RET, and lost connections.

### 2. Preserve physical MDB ordering ahead of application commands

The actor uses an unbiased `tokio::select!` between serial input and application
commands. If `VEND CANCEL` is already buffered while an application approval is
queued, the approval can win the selection. The cancel is then treated as out
of sequence, and the reset path can incorrectly imply a successful vend.

The serial/VMC ordering needs to be authoritative. Possible implementations
include draining already-ready adapter input before accepting a vend decision,
or routing both sources through an ordering layer with explicit precedence.
Add a deterministic cancel-versus-approve regression test.

### 3. Resynchronize after startup or process restart

The adapter emits its initialization sequence only at machine power-on, and a
session left open can survive the controlling process. The actor nevertheless
starts in `Enabled` and allows `BEGIN SESSION` immediately.

Define and hardware-qualify a startup recovery strategy that can distinguish an
enabled reader from a stale active session. Do not rely on an unconditional
`END SESSION` until its behavior in every VMC state has been qualified. Add
tests for reconnecting while enabled, disabled, and in a stale session.

### 4. Serialize denial and session teardown

Explicit denial, decision timeout, and dropped `PendingVend` currently move the
actor directly back to `Idle` after the serial write. A caller can consequently
request session cancellation before `VEND DENIED` has been acknowledged.

Use an explicit awaiting-denial-acknowledgement state for every denial path,
then permit `SESSION CANCEL` only after the denial is settled. Test an immediate
`finish()` after explicit, timed-out, and drop-triggered denial.

## Other conformance and qualification work

### Reader disable during a transaction

MDB says `READER DISABLE` must not interrupt a transaction already in progress.
The actor currently returns `COMMAND OUT OF SEQUENCE` whenever a session is
active, and normal session completion republishes `Enabled`. Track a deferred
disabled state, finish the current transaction, and remain disabled afterward.

### Commands received during the uninterruptible vend sequence

Unknown commands are currently ignored. MDB's command-out-of-sequence example
includes an expansion request during a pending vend: the command is
acknowledged, then `COMMAND OUT OF SEQUENCE` is returned on a poll and the VMC
resets the reader. Parse enough command identity and state to implement this
behavior, then test representative unexpected commands.

### Application response time

The captured adapter configuration advertises a seven-second application
maximum response time (`Z7 = 0x07`). The interactive harness configures the
actor for a 46-second application response time around the kiosk's 40-second
payment window, but that setter does not reprogram the adapter. Reconfigure and
recapture the adapter, or constrain the
actor to the value actually advertised. The configured and enforced deadlines
must have a single source of truth.

### Refund capability and payment durability

The stored miscellaneous-options byte is `0x0D`, which includes the capability
to restore funds. Version one instead records a durable staff-assistance case
for a paid Lightning vend failure and uses an out-of-band operator resolution.
Clear this unsupported capability in the adapter configuration before real
money is enabled. If protocol-level refunds are added later, durably coordinate
refund-pending, refund-complete, and refund-failed states with the payment
backend. The adapter's automatic acknowledgement behavior must also be
qualified so the VMC is not told refund handling is complete too early.

### Session-complete response on real hardware

The actor now uses the specification's `END SESSION` response (`07 07`) after
`SESSION COMPLETE`. An AP 113 capture confirmed that immediately queueing the
next `BEGIN SESSION` can provoke a one-byte `10` RESET before teardown settles;
the actor now waits for the `END SESSION` ACK before returning session
ownership. The physically proven older harness sent `06 06`, so repeated-round
qualification of the spec-correct response is still required on the AP 113.

## Validation required before closing this document

- Formatting, Clippy, unit tests, and documentation checks remain clean.
- Simulated tests cover every ACK/NAK/RET and ordering edge described above.
- A power-cycle capture proves initialization and advertised configuration.
- Restart recovery is tested with a deliberately stale session.
- Approve, deny, timeout, VMC cancel, vend success, vend failure, reader disable,
  display request, and unexpected-command paths are exercised on the physical
  machine.
- Payment/refund outcomes remain correct across process termination and restart.

Relevant specification sections are MDB/ICP 4.3 sections 2.4, 7.3, 7.4, 7.5,
and the vend-session examples in 7.7.
