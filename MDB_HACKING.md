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
| VMC → Pi | `13 00 <price> <item> <cks>` | Vend request |
| **Pi → VMC** | **`05 <price hi> <price lo> <cks>`** | **Vend approved** |
| Pi → VMC | `06 <cks>` | Vend denied |
| VMC → Pi | `13 02 ...` | Vend success (product dispensed) |
| VMC → Pi | `13 04 ...` | Session complete |
| Pi → VMC | `06 06` | Ack |

`05 <price>` is the single line that authorizes a vend. In the finished system
it is gated on BTCPay invoice settlement.

---

## Design note: payment must come BEFORE credit

MDB gives the reader only a few seconds to answer a vend request. A Lightning
payment takes far longer. Holding the approval while waiting for settlement will
time out and cancel the session.

**Invert the flow:**

1. Idle — display a Lightning QR
2. Customer scans and pays
3. BTCPay confirms settlement
4. **Then** send Begin Session with the credit amount
5. Machine displays credit; customer selects
6. Vend request arrives → approve **immediately**
7. Session complete

This is how commercial QR-based readers work and it avoids the timeout entirely.

**Open hardware question:** the AP 113's display is 16×1 characters — far too
small for a QR code. A separate customer-facing screen is required. `PROJECT.md`
specs a 2.13" e-ink for the earlier ESP32 design; it could be driven from the Pi
instead, though QR scannability at that size was already flagged as a risk.

---

## Scripts

In `mdb/`:

| Script | Purpose |
|---|---|
| `mdb_listen.py` | Passive capture with timestamps. Run across a machine power cycle. |
| `mdb_send_test.py` | Checksum probe — proves the send framing. Expect `9D`. |
| `mdb_vend.py` | Opens a session and auto-approves the vend request. **Vends for real.** |

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
