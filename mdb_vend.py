#!/usr/bin/env python3
"""Open an MDB cashless session and approve the resulting vend request.

WARNING: this vends for real. If a product is loaded in the selected slot, the
machine will dispense it.

Send format is raw binary bytes with a trailing checksum (sum of payload bytes
mod 256). Replies arrive as STX (0x02) + ASCII-hex + ETX (0x03).

This is the proven back half of the Lightning integration. To finish it, gate
the Begin Session call on BTCPay invoice settlement rather than sending it
immediately -- see the design note in MDB_HACKING.md about why payment must
happen before credit is offered.
"""

import datetime
import threading
import time
import serial

PORT = '/dev/ttyUSB0'
BAUD = 9600
CREDIT = 0x0064          # funds advertised in Begin Session

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

    # Vend request: 13 00 <price hi> <price lo> <item hi> <item lo> <cks>
    if len(b) >= 6 and b[0] == 0x13 and b[1] == 0x00:
        price = (b[2] << 8) | b[3]
        item = (b[4] << 8) | b[5]
        print(f"{ts()}  ==> VEND REQUEST item={item} price={price}", flush=True)
        # In the finished daemon, only reach this line once BTCPay reports the
        # invoice settled; otherwise send "06" to deny.
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
    send(f"03{CREDIT:04X}", "BEGIN SESSION - credit available")
    print("\n>>> Now press a selection on the machine. Ctrl+C to stop.\n",
          flush=True)
    while True:
        time.sleep(1)


if __name__ == '__main__':
    main()
