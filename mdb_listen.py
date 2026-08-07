#!/usr/bin/env python3
"""Passive MDB capture from the WAFER RS232-MDB (PC2MDB) box.

The box's self-ID and the VMC's setup sequence are only sent once, when the
vending machine powers on. Start this first, then power-cycle the machine.

Frames arrive as STX (0x02) + ASCII-hex + ETX (0x03).
"""

import datetime
import serial

PORT = '/dev/ttyUSB0'
BAUD = 9600
LOG = '/home/joefi/mdb_log.txt'


def ts():
    return datetime.datetime.now().strftime('%H:%M:%S.%f')[:-3]


def main():
    s = serial.Serial(PORT, BAUD, timeout=0.2)
    s.dtr = True
    s.rts = True
    print(f"Listening at {BAUD}. Power-cycle the machine now. Ctrl+C to stop.",
          flush=True)

    buf = b''
    with open(LOG, 'ab') as f:
        while True:
            d = s.read(256)
            if not d:
                continue
            f.write(d)
            f.flush()
            print(f"{ts()}  raw {d.hex(' ')}", flush=True)

            buf += d
            while b'\x02' in buf:
                i = buf.index(b'\x02')
                if b'\x03' not in buf[i:]:
                    break
                j = buf.index(b'\x03', i)
                payload = buf[i + 1:j]
                buf = buf[j + 1:]
                try:
                    decoded = bytes.fromhex(payload.decode('ascii'))
                    print(f"{ts()}  FRAME {decoded.hex(' ')}", flush=True)
                except ValueError:
                    # Firmware version banner is plain text, not hex
                    print(f"{ts()}  TEXT  {payload!r}", flush=True)


if __name__ == '__main__':
    main()
