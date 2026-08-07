#!/usr/bin/env python3
"""Prove the send framing for the WAFER RS232-MDB box.

The box computes checksums on request: send a payload without its checksum and
it replies with the correct value. The box's own config data is known to be
01 01 09 72 0A 02 07 0D with checksum 0x9D, so the framing that returns 9D is
the correct one.

Result on this hardware: raw binary bytes. Replies come back as
STX + ASCII-hex + ETX regardless of what was sent.
"""

import time
import serial

PORT = '/dev/ttyUSB0'
BAUD = 9600
CONFIG = '010109720A02070D'   # expect the box to answer 9D


def main():
    s = serial.Serial(PORT, BAUD, timeout=0.5)
    time.sleep(0.3)

    candidates = [
        ("A: STX + ascii-hex + ETX", b'\x02' + CONFIG.encode() + b'\x03'),
        ("B: ascii-hex + CRLF",      CONFIG.encode() + b'\r\n'),
        ("C: raw bytes",             bytes.fromhex(CONFIG)),
        ("D: ascii-hex bare",        CONFIG.encode()),
    ]

    for label, payload in candidates:
        s.reset_input_buffer()
        s.write(payload)
        s.flush()
        time.sleep(0.8)
        reply = s.read(s.in_waiting or 0)
        hit = '   <== correct framing' if b'9D' in reply else ''
        print(f"{label:34} -> {reply!r}{hit}")


if __name__ == '__main__':
    main()
