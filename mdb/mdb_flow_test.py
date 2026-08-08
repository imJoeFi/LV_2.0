#!/usr/bin/env python3
"""Bench harness for the full machine-interaction flow, payment faked.

Exercises the select-first ordering end to end against the real AP 113, with a
human standing in for the payment backend:

    1. BEGIN SESSION 03 FFFF        funds not yet determined
    2. customer presses a selection
    3. VEND REQUEST 13 00 reveals the selection and its exact price
    4. a QR would be shown here     (faked -- no display fitted yet)
    5. "Payment received? [y/N]"    60 s countdown, y/n stands in for settlement
    6. y  -> VEND APPROVED 05 <price>     the machine vends
       n  -> VEND DENIED    06            the machine should reset
       timeout -> same as n
    7. whatever the machine says next is logged verbatim, the session is closed,
       and we re-arm for the next round

Loops until Ctrl+C, so repeat runs need no restart.

WARNING: answering "y" vends for real. Use an empty slot until you trust it.

Measured on this machine 2026-08-08, worth knowing before reading the output:
  - the VMC allows 60 s between VEND REQUEST and our answer
  - on timeout it sends 13 03 (VEND FAILURE) and then nothing -- no 13 04
  - the item field is NOT a 16-bit integer. It is two bytes: row, then column.
    00 01 = A1, 01 02 = B2, 02 03 = C3, 04 05 = E5

Nothing here talks to a payment backend. Step 5 is the seam Tommy's side plugs
into: replace the prompt with "wait for settlement, or time out".
"""

import argparse
import contextlib
import datetime
import os
import queue
import select
import sys
import termios
import threading
import time
import tty

import serial

# Reader -> VMC
BEGIN_SESSION = 0x03
VEND_APPROVED = 0x05
VEND_DENIED = 0x06
END_SESSION = 0x07

FUNDS_UNKNOWN = 0xFFFF

# How long to keep listening after we answer, before declaring the round over.
SETTLE_WATCH_S = 10.0


# --- console -------------------------------------------------------------
# The RX thread prints while the countdown is redrawing on the same line, so
# every write goes through here to avoid interleaved garbage.

_out_lock = threading.Lock()
_countdown = {'text': ''}


def _erase():
    if _countdown['text']:
        sys.stdout.write('\r' + ' ' * (len(_countdown['text']) + 2) + '\r')


def _redraw():
    if _countdown['text']:
        sys.stdout.write(_countdown['text'])
        sys.stdout.flush()


def log(msg):
    with _out_lock:
        _erase()
        print(msg, flush=True)
        _redraw()


def set_countdown(text):
    with _out_lock:
        _erase()
        _countdown['text'] = text
        _redraw()


@contextlib.contextmanager
def single_keypress():
    """Read one key at a time, with no echo and no Enter needed.

    In normal line mode the countdown redraw blanks the line the user is typing
    on, so their keystroke vanishes from the screen while still sitting in the
    buffer -- and select() cannot see it at all until they press Enter. cbreak
    turns off both ICANON and ECHO, so we own the line completely and a single
    y/n registers instantly. ISIG stays on, so Ctrl+C still works.
    """
    fd = sys.stdin.fileno()
    try:
        saved = termios.tcgetattr(fd)
    except termios.error:
        yield False                     # not a tty; caller falls back
        return
    try:
        tty.setcbreak(fd)
        yield True
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


def ts():
    return datetime.datetime.now().strftime('%H:%M:%S.%f')[:-3]


def cks(payload):
    return sum(payload) & 0xFF


# MDB price units. Measured 2026-08-08: we sent funds 1345 and the machine
# displayed $134.50, so value = raw * scale / 10**decimals with scale 10 and
# decimals 2 -- i.e. raw / 10. That matches the reader config byte 0x0A in the
# power-on capture. It is NOT cents.
MONEY = {'scale': 10, 'decimals': 2}


def money(raw):
    return f"${raw * MONEY['scale'] / 10 ** MONEY['decimals']:.2f}"


def decode_selection(row, col):
    """The item field is row/column, not a number. 01 02 -> B2."""
    if row < 26:
        return f'{chr(ord("A") + row)}{col}'
    return f'?{row}/{col}'


# --- link ----------------------------------------------------------------

class Link:
    def __init__(self, port, baud):
        self.serial = serial.Serial(port, baud, timeout=0.2)
        self.events = queue.Queue()
        self._stop = threading.Event()

    def start(self):
        threading.Thread(target=self._rx_loop, daemon=True).start()

    def stop(self):
        self._stop.set()

    def send(self, hexstr, label=''):
        payload = bytes.fromhex(hexstr)
        frame = payload + bytes([cks(payload)])
        self.serial.write(frame)
        self.serial.flush()
        log(f"{ts()}  TX  {frame.hex(' ')}   {label}")

    def begin_session(self, funds=FUNDS_UNKNOWN):
        # This VMC does NOT honour 0xFFFF as "funds unknown" -- it displays it
        # literally, as $655.35. Selections still work (everything is
        # affordable), but the machine advertises a nonsense balance on its own
        # 16x1 display while it waits. --funds lets you try something saner,
        # e.g. 1345 = the machine's max price = $13.45.
        note = ' (0xFFFF -- displayed literally, NOT "funds unknown")' \
            if funds == FUNDS_UNKNOWN else f' ({money(funds)} credit)'
        self.send(f'{BEGIN_SESSION:02X}{funds:04X}', f'BEGIN SESSION{note}')

    def approve(self, price):
        self.send(f'{VEND_APPROVED:02X}{price:04X}',
                  f'VEND APPROVED price={price}')

    def deny(self):
        self.send(f'{VEND_DENIED:02X}', 'VEND DENIED')

    def end_session(self):
        self.send(f'{END_SESSION:02X}', 'END SESSION')

    def ack_session_complete(self):
        # What mdb_vend.py proved this machine accepts. Spec-wise 0x06 is VEND
        # DENIED, which reads oddly, but do not "fix" it without re-testing.
        self.send('0606', 'ack SESSION COMPLETE')

    def _rx_loop(self):
        buf = b''
        while not self._stop.is_set():
            try:
                chunk = self.serial.read(256)
            except Exception as exc:                        # noqa: BLE001
                log(f'{ts()}  read failed: {exc}')
                return
            if not chunk:
                continue
            buf += chunk
            while b'\x02' in buf:
                i = buf.index(b'\x02')
                if b'\x03' not in buf[i:]:
                    break
                j = buf.index(b'\x03', i)
                self._handle(buf[i + 1:j])
                buf = buf[j + 1:]
            if len(buf) > 4096:
                buf = b''

    def _handle(self, payload_ascii):
        try:
            b = bytes.fromhex(payload_ascii.decode('ascii'))
        except (ValueError, UnicodeDecodeError):
            log(f'{ts()}  RX  {payload_ascii!r}   (text)')
            return

        note = ''
        if b == b'\x00':
            note = '   (ack)'
        elif len(b) >= 2:
            note = {
                (0x13, 0x01): '   <== VEND CANCEL',
                (0x13, 0x02): '   <== VEND SUCCESS',
                (0x13, 0x03): '   <== VEND FAILURE',
                (0x13, 0x04): '   <== SESSION COMPLETE',
                (0x14, 0x00): '   <== READER DISABLE',
                (0x14, 0x01): '   <== READER ENABLE',
                (0x14, 0x02): '   <== READER CANCEL',
            }.get((b[0], b[1]), '')

        log(f"{ts()}  RX  {b.hex(' ')}{note}")

        if len(b) >= 6 and b[0] == 0x13 and b[1] == 0x00:
            self.events.put({
                'kind': 'vend_request',
                'price': (b[2] << 8) | b[3],
                'row': b[4], 'col': b[5],
            })
        elif len(b) >= 2 and b[0] == 0x13:
            self.events.put({'kind': {
                0x01: 'vend_cancel', 0x02: 'vend_success',
                0x03: 'vend_failure', 0x04: 'session_complete',
            }.get(b[1], 'other')})
        elif len(b) >= 2 and b[0] == 0x14:
            self.events.put({'kind': 'reader'})


# --- the fake display ----------------------------------------------------

def show_fake_qr(selection, price):
    """Stands in for the customer screen. No display fitted yet."""
    dollars = money(price)
    payload = f'lnbc-FAKE-{selection}-{price}'
    lines = [
        '',
        '        +-------------------------------------+',
        '        |                                     |',
        '        |     [ QR CODE WOULD APPEAR HERE ]   |',
        '        |                                     |',
        f'        |{selection.center(37)}|',
        f'        |{dollars.center(37)}|',
        '        |                                     |',
        '        +-------------------------------------+',
        f'        payload: {payload}',
        '',
    ]
    log('\n'.join(lines))


# --- the faked payment step ----------------------------------------------

def wait_for_payment(link, timeout_s):
    """Stand-in for settlement. Returns 'paid', 'declined', 'timeout' or
    'cancelled' (the machine gave up or the customer backed out first)."""
    deadline = time.time() + timeout_s

    # Discard anything typed before the prompt appeared. Without this, a stray
    # keystroke from while we were waiting for a selection gets consumed the
    # instant we start reading -- which is how round 2 approved a vend in 0.0s.
    try:
        termios.tcflush(sys.stdin, termios.TCIFLUSH)
    except (termios.error, ValueError):
        pass

    log('    >>> Payment received?   press  y = vend   n = decline'
        '   (no Enter needed, no answer = timeout)')

    with single_keypress() as raw:
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                set_countdown('')
                return 'timeout'

            set_countdown(
                f'    waiting for payment... {remaining:4.0f}s   press y or n ')

            # The machine can end the round out from under us -- notice that
            # rather than sending an answer into a dead session.
            try:
                ev = link.events.get_nowait()
            except queue.Empty:
                pass
            else:
                if ev['kind'] in ('vend_cancel', 'session_complete',
                                  'vend_failure'):
                    set_countdown('')
                    return 'cancelled'

            r, _, _ = select.select([sys.stdin], [], [], 0.25)
            if not r:
                continue

            if raw:
                answer = os.read(sys.stdin.fileno(), 1).decode(
                    'utf-8', 'ignore').lower()
                if answer == '\x03':
                    raise KeyboardInterrupt
            else:
                answer = sys.stdin.readline().strip().lower()

            if answer.startswith('y'):
                set_countdown('')
                log(f'{ts()}  >>> you pressed Y')
                return 'paid'
            if answer.startswith('n'):
                set_countdown('')
                log(f'{ts()}  >>> you pressed N')
                return 'declined'


# --- one round -----------------------------------------------------------

def drain(link):
    while True:
        try:
            link.events.get_nowait()
        except queue.Empty:
            return


def watch(link, seconds):
    """Listen after answering and report what the machine did."""
    seen = set()
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            ev = link.events.get(timeout=0.2)
        except queue.Empty:
            continue
        seen.add(ev['kind'])
        if ev['kind'] == 'session_complete':
            link.ack_session_complete()
            break
    return seen


def run_round(link, timeout_s, round_no, funds):
    log('')
    log('=' * 68)
    log(f'  ROUND {round_no}  --  press a selection on the machine')
    log('=' * 68)

    drain(link)

    # Clear any session the VMC still thinks is open. Without this, a previous
    # run that died mid-session leaves the machine parked on a stale credit
    # display and ignoring new selections.
    link.end_session()
    time.sleep(0.5)
    drain(link)

    link.begin_session(funds)

    # 1-2. wait for the customer to choose. Never block silently -- an empty
    # queue here looks identical to a crashed script otherwise.
    waited = 0.0
    while True:
        try:
            ev = link.events.get(timeout=1.0)
        except queue.Empty:
            waited += 1.0
            if waited % 15 == 0:
                log(f'{ts()}  still waiting for a selection ({waited:.0f}s) '
                    f'-- Ctrl+C to stop')
            continue
        if ev['kind'] == 'vend_request':
            break
        if ev['kind'] == 'session_complete':
            log(f'{ts()}  machine closed the session before a selection '
                f'-- re-arming')
            link.ack_session_complete()
            time.sleep(0.5)
            link.begin_session(funds)

    selection = decode_selection(ev['row'], ev['col'])
    price = ev['price']
    log(f"{ts()}  ==> SELECTION {selection}  price={price} "
        f"({money(price)})  raw item bytes "
        f"{ev['row']:02X} {ev['col']:02X}")

    # 3-4. the screen the customer would see
    show_fake_qr(selection, price)

    # 5. payment, faked
    t0 = time.time()
    result = wait_for_payment(link, timeout_s)
    elapsed = time.time() - t0

    if result == 'cancelled':
        log(f'{ts()}  round ended by the machine after {elapsed:.1f}s')
        return

    # 6. answer the VMC
    if result == 'paid':
        log(f'{ts()}  payment confirmed after {elapsed:.1f}s -- approving')
        link.approve(price)
        expect = 'expect VEND SUCCESS 13 02, or VEND FAILURE 13 03 if the ' \
                 'slot is empty'
    else:
        why = 'declined' if result == 'declined' else f'timed out at {elapsed:.0f}s'
        log(f'{ts()}  payment {why} -- denying so the machine resets')
        link.deny()
        expect = 'expect the machine to reset and release the selection'

    # 7. observe
    log(f'    watching {SETTLE_WATCH_S:.0f}s -- {expect}')
    seen = watch(link, SETTLE_WATCH_S)

    # 13 03 only means "the item did not drop" if we actually approved. After a
    # decline or a timeout it just means the machine gave up waiting, and
    # calling that a vend failure would tell a customer they had been charged.
    if 'vend_success' in seen:
        log(f'{ts()}  RESULT: vended')
    elif result == 'paid':
        if 'vend_failure' in seen:
            log(f'{ts()}  RESULT: approved but DID NOT VEND '
                f'(empty slot, or motor/sensor fault)')
        else:
            log(f'{ts()}  RESULT: approved, machine said '
                f"{sorted(seen) if seen else 'nothing'}")
    else:
        why = 'declined' if result == 'declined' else 'timed out'
        log(f'{ts()}  RESULT: {why}, machine said '
            f"{sorted(seen) if seen else 'nothing'} -- no vend, nothing charged")

    if 'session_complete' not in seen:
        log(f'{ts()}  no SESSION COMPLETE -- closing the session ourselves')
        link.end_session()
        time.sleep(1.0)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', default='/dev/ttyUSB0')
    parser.add_argument('--baud', type=int, default=9600)
    # 45, not 60. The machine's own limit is 60 s, and at 60 we raced it by
    # 217 ms and sent VEND DENIED into an already-closed session.
    parser.add_argument('--timeout', type=float, default=45.0,
                        help='payment window in seconds (default 45, must stay '
                             'under the machine\'s own 60 s)')
    parser.add_argument('--funds', type=lambda v: int(v, 0),
                        default=FUNDS_UNKNOWN,
                        help='Begin Session funds, in cents. Default 0xFFFF, '
                             'which this VMC displays literally as $655.35. '
                             'Try 1345 (the machine max price) instead.')
    parser.add_argument('--clear', action='store_true',
                        help='just send END SESSION to unstick the machine, '
                             'then exit')
    args = parser.parse_args(argv)

    if not sys.stdin.isatty():
        print('This needs an interactive terminal for the y/n prompt.',
              file=sys.stderr)
        return 2

    link = Link(args.port, args.baud)
    link.start()
    time.sleep(0.5)

    if args.clear:
        print('clearing any open session...')
        link.end_session()
        time.sleep(1.5)
        link.stop()
        print('done -- if the display is still stuck, power-cycle the machine')
        return 0

    print(f'port     {args.port}')
    print(f'window   {args.timeout:.0f}s')
    print(f'funds    0x{args.funds:04X} ({money(args.funds)})')
    print(f'scale    raw x {MONEY["scale"]} / 10^{MONEY["decimals"]} '
          f'-- so raw 1345 = {money(1345)}')
    print('WARNING  answering "y" vends for real. B2 is your empty slot.')
    print('Ctrl+C to stop.')

    round_no = 0
    try:
        while True:
            round_no += 1
            run_round(link, args.timeout, round_no, args.funds)
    except KeyboardInterrupt:
        set_countdown('')
        print('\nstopping -- closing the session')
        try:
            link.end_session()
            time.sleep(0.5)
        except Exception:                                   # noqa: BLE001
            pass
    finally:
        link.stop()
    return 0


if __name__ == '__main__':
    sys.exit(main())
