# Payment interface

The seam between the machine side of LightningVEND and the payment side.

The machine side is proven working — see the bench results in
[`MDB_HACKING.md`](MDB_HACKING.md). It handles the vending machine, the MDB
session, the timing, and the customer display. It knows nothing about how
payment happens.

The payment side owns everything else.

---

## The contract

Two calls. That is the whole surface.

### 1. Create a payment request

```
create(selection, amount) -> { payload, display_amount, id }
```

| In | |
|---|---|
| `selection` | Selection code, e.g. `"A1"` — for display and bookkeeping |
| `amount` | Price in **dollars**, already converted from MDB units by our side |

| Out | |
|---|---|
| `payload` | The string to render as a QR on the customer screen |
| `display_amount` | Optional secondary figure to show, e.g. a sats amount |
| `id` | Whatever handle `status()` needs |

Called the instant the customer presses a selection. **Return fast** — every
millisecond here comes out of the payment window below.

### 2. Ask whether it settled

```
status(id) -> "pending" | "settled" | "dead"
```

Polled, non-blocking, roughly every 400 ms.

- `settled` → we send `05 VEND APPROVED` and the machine dispenses
- `dead` → expired, cancelled, or otherwise unpayable; we release the machine
- `pending` → keep waiting

A cancel hook (`cancel(id)`) is useful but optional — we call it when a customer
walks away or picks something else.

---

## The one hard constraint

**The whole payment must complete within 45 seconds** of the customer pressing
their selection.

The vending machine gives us 60 seconds between announcing the selection and
needing an answer — measured three times, dead consistent. We answer at 45 to
leave margin; at 60 we raced it and lost by 217 ms.

Inside that budget sits everything: creating the request, the customer getting
their phone out, scanning, confirming, and settlement being *observed* by
`status()`. So the real user-facing budget is more like 40 seconds.

If nothing has settled by then we send `06 VEND DENIED`, the machine resets, and
the customer is told nothing was charged. That path is tested and clean — the
machine re-arms for the next person with no intervention.

**Nothing may block.** `create()` and `status()` run on a worker; the MDB
deadline timer must never be held up by a slow or hanging network call.

---

## Money and rounding

Prices come off the machine as integers that are **not cents**. Our side does
the conversion and hands you dollars:

    dollars = raw * 10 / 100 = raw / 10

Verified on the bench: we sent `1345` and the machine displayed `$134.50`.

The amount must be honoured **exactly**. The machine is told to vend at the
price it quoted, so an underpayment cannot be absorbed. If a partial payment is
possible in your backend, treat it as `pending` until whole, then `dead`.

Note the machine's current prices are unconfigured — a $5.00 minimum and
$134.50 maximum. They need setting via `SET PRICE` in the service menu before
any real use. That is a machine configuration job, not a software one.

---

## QR constraints

The customer screen is a 720×720 panel over **72.53 mm** of glass, and the QR
gets a 472 px window inside it. That works out to:

| Payload length | Modules | mm per module |
|---|---|---|
| ~270 chars | 49 | 0.97 |
| ~360 chars | 57 | 0.83 |

Phone cameras start failing around 0.5 mm per module, so there is headroom — but
not a lot. **Shorter payloads scan better.** If the payload format lets you trim
a description field or drop an optional parameter, do.

If the payload is case-insensitive, tell us: encoding it uppercase lets the QR
use alphanumeric mode instead of byte mode, which is roughly 30% denser. That
alone is worth several modules.

---

## What our side guarantees

- A selection is announced exactly once per session
- `create()` is called at most once per selection, unless the customer changes
  their mind, in which case the previous one is cancelled first
- `05 VEND APPROVED` is sent **only** on `settled`, never speculatively
- The machine is always released, on every path — settled, dead, or timed out

## What our side does not do

- Refunds. MDB has no mechanism for it, and there is no change to give
- Partial payments or credit carried between sessions
- Retries after a vend has been approved

---

## Status

Machine side: **working on real hardware.** Full flow confirmed 2026-08-08 —
selection, price, approval, dispense, session teardown, re-arm.

Payment side: not started. `mdb/mdb_flow_test.py` stands in for it with a
`y`/`n` prompt, which is exactly where `status()` will slot in.
