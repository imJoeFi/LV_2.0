#!/usr/bin/env python3
"""Open an MDB cashless session with unknown funds, learn the selected item's
price from VEND REQUEST, then manually approve the vend.

WARNING: this vends for real. If a product is loaded in the selected slot, the
machine will dispense it.

Send format is raw binary bytes with a trailing checksum (sum of payload bytes
mod 256). Replies arrive as STX (0x02) + ASCII-hex + ETX (0x03).

The intended Lightning flow is:

    BEGIN SESSION (funds = FFFF / unknown)
        -> customer selects item
        -> VEND REQUEST reveals exact item + price
        -> create exact-amount Lightning hold invoice
        -> wait for invoice ACCEPTED
        -> VEND APPROVED

For now, pressing Enter manually stands in for invoice acceptance.
"""

import datetime
import threading
import time
import serial

PORT = '/dev/ttyUSB0'
BAUD = 9600

# MDB special value: available funds are not yet determined.
CREDIT = 0xFFFF

s = serial.Serial(PORT, BAUD, timeout=0.2)


def ts():
    return datetime.datetime.now().strftime('%H:%M:%S.%f')[:-3]


def cks(b):
    return sum(b) & 0xFF


def send(hexstr, label=""):
    """Send a payload as raw bytes with the checksum appended."""
    payload = bytes.fromhex(hexstr)
    frame = payload + bytes([cks(payload)])
    s.write(frame)
    s.flush()
    print(f"{ts()}  TX  {frame.hex(' ')}   {label}", flush=True)


def handle(payload):
    try:
        b = bytes.fromhex(payload.decode('ascii'))
    except (ValueError, UnicodeDecodeError):
        print(f"{ts()}  RX  {payload!r}   (text)", flush=True)
        return

    print(f"{ts()}  RX  {b.hex(' ')}", flush=True)

    # Vend request:
    #   13 00 <price hi> <price lo> <item hi> <item lo> <cks>
    if len(b) >= 6 and b[0] == 0x13 and b[1] == 0x00:
        price = (b[2] << 8) | b[3]
        item = (b[4] << 8) | b[5]

        print(
            f"{ts()}  ==> VEND REQUEST item={item} price={price}",
            flush=True,
        )

        print(
            "\n"
            ">>> Exact vend price is now known.\n"
            ">>> This is where you would create the Lightning hold invoice.\n"
            ">>> Press Enter to simulate invoice ACCEPTED and approve the vend.\n",
            flush=True,
        )

        input()

        send(f"05{price:04X}", "VEND APPROVED")

    elif len(b) >= 2 and b[0] == 0x13 and b[1] == 0x04:
        print(f"{ts()}  ==> SESSION COMPLETE", flush=True)
        send("0606", "ack session complete")


def rx_loop():
    buf = b''

    while True:
        d = s.read(256)
        if not d:
            continue

        buf += d

        while b'\x02' in buf:
            i = buf.index(b'\x02')

            if b'\x03' not in buf[i:]:
                break

            j = buf.index(b'\x03', i)
            handle(buf[i + 1:j])
            buf = buf[j + 1:]


def main():
    threading.Thread(target=rx_loop, daemon=True).start()

    time.sleep(1)

    send(
        f"03{CREDIT:04X}",
        "BEGIN SESSION - funds not yet determined",
    )

    print(
        "\n"
        ">>> Now press a selection on the machine.\n"
        ">>> The VEND REQUEST should reveal its exact price.\n"
        ">>> Ctrl+C to stop.\n",
        flush=True,
    )

    while True:
        time.sleep(1)


if __name__ == '__main__':
    main()
