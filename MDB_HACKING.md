# MDB Cashless Integration — AP 113 Vending Machine

Working notes for driving an Automatic Products 113 snack machine from a
Raspberry Pi over MDB, so a Bitcoin Lightning payment can authorize a vend.

**Status: end-to-end vend confirmed.** The Pi successfully authorized a real
vend on the machine.

---

## Hardware chain

```
Raspberry Pi 5
   └─ USB
      └─ Prolific USB-to-RS232 adapter (067b:23a3)   → /dev/ttyUSB0
         └─ DB9
            └─ WAFER RS232-MDB (PC2MDB) box, V42
               │  MDB address switch set to 10H (Cashless Device #1)
               │  Powered FROM the MDB bus — no separate supply
               └─ 6-pin Molex MDB harness
                  └─ VE Solutions VE5801 Universal Control Board (the VMC)
                     └─ Automatic Products 113 snack machine
```

### Machine identification

The VMC identifies itself over MDB (decoded from its `1700` response):

| Field | Value |
|---|---|
| Manufacturer | `VEI` (Vendors Exchange International) |
| Model | `AP113` |
| Serial | `F1B83EE7` |
| Software version | `32` (3.2) |

Controller boards: `VE5801-RSR641/E1` (UCB) and `VE5856-RSR654/F`, firmware
`SW 3.2.1 / BL 0.0.8`, manufactured late 2017. The UCB is a retrofit board that
adds MDB + DEX to the AP 111/112/113 family and is explicitly sold as a
credit-card/DEX upgrade kit.

The machine's coin mech and bill validator have been physically removed. The
UCB runs happily without them and sits at a normal `SELECT` idle prompt.

---

## Critical gotcha: two mirror-image products

WAFER/waferstar sells two boxes whose names differ only in word order. They do
opposite jobs, and buying the wrong one costs days.

| Product | Direction | Role |
|---|---|---|
| **MDB-RS232** (MDB2PC), incl. the `MDB-RPI` board | MDB → PC | Your PC becomes the **VMC/master**. It polls coin mechs, bill validators, and card readers. |
| **RS232-MDB** (PC2MDB) | PC → MDB | Your PC becomes a **cashless peripheral**. The vending machine's VMC polls *you*. **This is the one this project needs.** |

We initially had an `MDB-RPI` board (silkscreen `MDB-RPI` / `PCB20200610U3.0`)
wired to the machine. It is a **master**. With the UCB also being a master, two
masters sat on one bus, neither answering the other. Symptoms:

- Board powered and responsive over UART, but every input returned `FF \r\n`
  (`0xFF` = MDB NAK — nothing on its bus ever answered)
- Zero passive traffic at every baud rate, even with the machine powered on
- All documented VMC commands (`09`, `0A`, `08`, `1700`) also NAK'd

Nothing was broken. The architecture was simply inverted.

Note also: on the `MDB-RPI` board, the silkscreen labels `CASL` / `BILL` /
`COIN` / `PC` are **LED labels, not a jumper header**. It is the five-LED
variant, which boots with payment devices disabled.

---

## Serial protocol (RS232-MDB / PC2MDB)

**Port settings: 9600 baud, 8 data bits, no parity, 1 stop bit.**

The framing is **asymmetric**:

| Direction | Format |
|---|---|
| **PC → box** | Raw binary bytes, checksum appended |
| **box → PC** | `STX` (`0x02`) + ASCII-hex string + `ETX` (`0x03`) |

**Checksum** is a simple sum of all payload bytes, mod 256.
Example: `03 + 00 + 64 = 0x67`, so Begin Session is sent as `03 00 64 67`.

### Checksum helper

The box will compute checksums for you. Send a payload *without* its checksum
and it replies with the correct value. This was used to prove the send framing:

```
TX  01 01 09 72 0A 02 07 0D        (raw bytes, no checksum)
RX  \x02 9D \x03                    (box returns 0x9D)
```

Sending the same payload as ASCII text, or with CRLF, returns a *different*
checksum — the box sums whatever literal bytes it receives. Getting `9D` back
confirms raw-byte framing is correct.

### What the box handles for you

- Replies to the VMC's continuous `POLL` automatically
- Sends its stored config data to the VMC at power-on
- All 9-bit MDB framing, bus timing, and electrical layer

Everything except polls is forwarded to the serial port. You only implement the
transaction messages below.

---

## Captured power-on sequence

Real capture from the AP 113, machine power-cycled while listening:

```
CN0999-WFV-Jul  6 2021,17:31:43     box firmware self-ID
01 01 09 72 0A 02 07 0D  9D         box's stored cashless config + checksum
11 00 03 10 01 01  26               VMC SETUP: feature level 3, 16x1 display
17 00 "VEI" "F1B83EE7" "AP113" "32" VMC ID (see table above)
11 01 05 41 00 32  8A               MAX/MIN PRICE: max 0x0541, min 0x0032
14 01 15                            READER ENABLE   ← success marker
```

Per the vendor's quick-start guide, receiving **`140115`** means the PC2MDB has
successfully connected to the VMC.

The `MDB Status` LED on the box flashes during this exchange. If it never
flashes, check the address switch (`10H` first, then `60H`), power-cycle the
VMC, and confirm the machine supports MDB cashless at all.

**The self-ID fires once, at power-on.** A listener started after the machine is
already up will see nothing. Always capture across a power cycle.

---

## Transaction flow

| Direction | Message | Meaning |
|---|---|---|
| VMC → Pi | `11 00 ...` | Setup / config data |
| VMC → Pi | `11 01 ...` | Max/min price |
| VMC → Pi | `17 00 ...` | Peripheral ID request |
| VMC → Pi | `14 01 15` | Reader enable |
| **Pi → VMC** | **`03 <funds hi> <funds lo> <cks>`** | **Begin Session — credit available** |
| VMC → Pi | `13 00 <price hi> <price lo> <row> <col> <cks>` | Vend request |
| **Pi → VMC** | **`05 <price hi> <price lo> <cks>`** | **Vend approved** |
| Pi → VMC | `06 <cks>` | Vend denied |
| Pi → VMC | `07 <cks>` | End session |
| VMC → Pi | `13 02 <row> <col> <cks>` | Vend success (product dispensed) |
| VMC → Pi | `13 03 <cks>` | Vend failure / gave up waiting |
| VMC → Pi | `13 04 <cks>` | Session complete |
| Pi → VMC | `06 06` | Ack session complete |

`05 <price>` is the single line that authorizes a vend. In the finished system
it is gated on confirmation from the payment side — see
[`PAYMENT_INTERFACE.md`](PAYMENT_INTERFACE.md).

---

## Bench results, 2026-08-08

All measured on the real machine with `mdb/mdb_flow_test.py`. These supersede
any earlier guesses in this file.

### The flow works: select first, pay the exact price

```
  07        END SESSION            clear any stale session first -- see below
  03 0541   BEGIN SESSION          credit >= the machine's max price
              v  customer presses a selection
  13 00 04 e3 00 01                A1, price 1251
              v  payment happens here, in our own time
  05 04 e3  VEND APPROVED
  13 02 00 01                      VEND SUCCESS, echoes the item vended
  13 04     SESSION COMPLETE
  06 06     ack, then re-arm
```

Confirmed end to end: the item dispensed.

### The vend-request window is 60 seconds

Measured three times by staying silent after `13 00`: **60.066 s, 59.865 s,
60.065 s.** The VMC then sends `13 03`, followed ~200 ms later by `13 04`.

Answer by **45 s**, not 60. At 60 we raced the machine by 217 ms and sent
`06 VEND DENIED` into a session it had already closed.

### The item field is row/column, not a number

`13 00 <price hi> <price lo> <row> <col>`. The last two bytes are *not* a 16-bit
integer — they are the tray and the position within it.

| Bytes | Selection |
|---|---|
| `00 01` | A1 |
| `01 02` | B2 |
| `02 03` | C3 |
| `04 05` | E5 |

`13 02` echoes the same two bytes, so the machine confirms *which* item it vended.

### Prices are scaled by 10 — they are not cents

    dollars = raw * scale / 10^decimals = raw * 10 / 100 = raw / 10

Proved by sending funds `1345` and watching the machine display **$134.50**.
This matches the `0A` (scale factor 10) and `02` (decimal places) bytes in the
reader's stored config, `01 01 09 72 0A 02 07 0D`.

So the power-on MAX/MIN PRICE of `0x0541` / `0x0032` means **$134.50 / $5.00**,
and A1's `1251` is **$125.10**. Those are clearly not snack prices — the
machine's own prices need setting via `SET PRICE` in the service menu.

### `0xFFFF` is NOT honoured as "funds unknown"

The VMC displays it literally, as **$655.35**. Selections still work, because
everything is affordable at that figure, but a machine advertising a fake
$655.35 balance is not shippable.

Send a real credit figure instead, at least the machine's max price (`0x0541`).
Anything lower makes the dearest selections unaffordable.

### Sessions must be explicitly closed

A session left open parks the machine on its credit display and it stops
accepting selections. It survives the controlling script exiting. **Always send
`07 END SESSION` before arming a new one** — `mdb_flow_test.py` does this at the
top of every round, and it is the difference between a machine that recovers and
one that looks bricked.

### Every branch, and what the machine does

| Branch | Machine's response | Outcome |
|---|---|---|
| `05` approve | `13 02 <row> <col>` → `13 04` | Vends. Clean |
| `06` decline, session still live | `00` ack → `13 04` | No vend, re-arms clean |
| No answer for 60 s | `13 03` → `13 04` | No vend, re-arms clean |
| Selection on an empty slot | Machine shows `NOT AVAILABLE`, **no `13 00` at all** | Cashless never consulted |

That last row matters: the VMC blocks known-empty selections itself, so the
"customer pays and nothing drops" case cannot arise from an empty slot. The
remaining exposure is a slot the machine believes is stocked that jams anyway.

---

## Open problem: an armed session shows a fake credit

To let a customer select *before* paying, the machine must first be told they
have credit. So an idle, armed machine permanently displays a balance nobody
has paid — `$134.50` in testing.

It is not exploitable, since nothing vends without our `05`. But it invites
button-pressing, and each press locks the session for the full window.

Three ways out, none yet tested:

1. **Does the VMC report a selection with no session open?** If it does, stay
   disarmed at idle and arm only once we know what they picked. Unlikely under
   MDB, but cheap to check and it would remove the problem entirely.
2. **Display Request (`02 <duration> <32 chars>`).** MDB lets the reader push
   text to the VMC's 16×1. If this VMC honours it, the screen can read
   `SCAN TO PAY` instead of a balance.
3. **A presence trigger.** Arm only when someone is actually there — a button or
   sensor — so the fake credit shows for seconds rather than permanently.

---

**Customer-facing screen:** the AP 113's display is 16×1 characters — far too
small for a QR code, so a separate screen is required. The 1.0 ESP32 design
specced a 2.13" e-ink (`1.0/Code/PROJECT.md`), but QR scannability at that size
was flagged as a risk.

Resolved for 2.0 with a **Waveshare 4inch HDMI LCD (C)**, 720×720 — square, so
the whole panel is usable QR. It mounts in the door opening left by the removed
bill mech. See [`DISPLAY.md`](DISPLAY.md).

---

## Scripts

In `mdb/`:

| Script | Purpose |
|---|---|
| `mdb_listen.py` | Passive capture with timestamps. Run across a machine power cycle. |
| `mdb_send_test.py` | Checksum probe — proves the send framing. Expect `9D`. |
| `mdb_vend.py` | Opens a session and auto-approves the vend request. **Vends for real.** |
| `mdb_vend_unknown_amount.py` | Early select-first bench script. Superseded by `mdb_flow_test.py`; note it uses funds `FFFF`, which this VMC mishandles. **Vends for real.** |
| **`mdb_flow_test.py`** | **The current harness.** Full flow with the payment step faked as a y/n prompt. Loops, closes sessions properly, decodes row/column selections. **Vends for real on `y`.** |
| **`mdb-flow-test` (Rust)** | Rust port of `mdb_flow_test.py` with the same serial framing, interactive payment prompt, session cleanup, and result logging. Entry point: `src/bin/mdb_flow_test.rs`; flow and protocol modules live under `src/`. **Vends for real on `y`.** |

Typical run:

```bash
python3 mdb/mdb_flow_test.py --port /dev/serial/by-id/usb-Prolific* --funds 1345
```

Rust equivalent:

```bash
cargo run --release --bin mdb-flow-test -- \
  --port /dev/serial/by-id/usb-Prolific* --funds 1345
```

`--funds 1345` is the machine's max price. `--clear` sends `07 END SESSION` and
exits, which unsticks a machine parked on a credit display.

---

## Troubleshooting log

Things that cost time, recorded so they don't cost it again:

- **`FF \r\n` to every input** is MDB NAK, not a parse error. It means nothing on
  the bus answered.
- **Passive silence proves nothing** unless the vending machine is confirmed
  powered on *and* the capture spans a power cycle.
- **The box draws power from the MDB bus.** No lights when connected to the Pi
  alone; lights when connected to MDB. That is itself a useful test — it proves
  the harness reaches a live bus at 24–34V.
- **`RS232 Communication` LED dark** while `MDB Status` blinks means the bus side
  works and the fault is on the serial link.
- Vendor documentation (waferstar, Upstate Networks, VE Solutions) blocks
  automated fetching. Download PDFs manually.
- VE Solutions support: **800-321-2311**. The `ADVANCED` service menu is
  password protected and the code is not published.

### Service menu (VE UCB)

Reached via the `KEY` button on the VE5856 board. Top-level items:

```
AUDIT DATA
SERVICE
ADVANCED        ← password protected
CONFIGURE       ← all drop sensor, tube fill, asset number,
                  set force vend, set bill escrow, set clock
MESSAGE SET-UP
SET PRICE
ACCESS CODE
```

No cashless enable setting was found in `CONFIGURE`, and none turned out to be
needed — the UCB probes for a cashless device at `10H` on its own.

---

## References

- WAFER RS232-MDB quick start: `http://www.waferstar.com/downloads/Quick Start of RS232-MDB.pdf`
- WAFER MDB-RS232 quick start: `http://www.waferlife.com/downloads/Quick_Start_of_RS232_MDB.pdf`
- MDB protocol spec v4.2 — cashless device section begins at §7.1
- VE Solutions UCB manuals: https://www.vesolutions.co/support/manuals/ucb
- Walkthrough video: https://www.youtube.com/watch?v=afq4uCf59Ac
