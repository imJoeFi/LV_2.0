# Customer Display — Waveshare 4inch HDMI LCD (C)

The customer-facing screen that shows the Lightning invoice QR, mounted in the
AP 113 door where the dollar bill mech used to be.

**Status: ordered, not yet fitted.** Everything below is from the vendor
drawing and wiki. Verify against the physical part on arrival — items marked
**[VERIFY]** are the ones most likely to bite.

Resolves the open hardware question in [`MDB_HACKING.md`](MDB_HACKING.md) —
the AP 113's own 16×1 character display cannot render a QR code.

---

## Part

| | |
|---|---|
| Product | 4inch HDMI Capacitive Touch IPS LCD Display (C), 720×720, Optical Bonding |
| Waveshare part no. | `4inch HDMI LCD (C)` |
| SKU | `21433` |
| Price | $66.99 |
| Wiki | https://www.waveshare.com/wiki/4inch_HDMI_LCD_(C) |

### Why this panel

- **720×720 square.** QR codes are square. A 16:9 panel wastes ~40% of its
  glass on letterbox; here the whole screen is usable QR. This directly
  addresses the scannability risk that killed the 2.13" e-ink idea from the
  1.0 ESP32 design.
- **Optical bonding.** No air gap between glass and LCD. Matters for a machine
  the public leans on — the glass can't be flexed into the panel, and it won't
  fog or delaminate.
- **6H toughened glass**, IPS, 170° viewing angle.

---

## Physical fit — AP 113 bill mech opening

Door opening: **3-3/8" × 4-3/8" = 85.725 × 111.125 mm**

| Part of the display | Dimension |
|---|---|
| Glass outline | 84.00 × 84.00 mm |
| **Active (lit) area** | **72.53 × 72.53 mm** |
| Glass border around active area | 5.74 mm top/left, 5.73 mm bottom/right (centered) |
| PCB | 79.50 × 77.80 mm |
| Mounting holes | 58.00 × 49.20 mm — the standard Raspberry Pi HAT pattern |

### Clearances

- **Across the 85.725 mm dimension:** glass is 84.00 mm → **0.86 mm per side.**
  The glass technically passes through the opening, but that is a slip fit with
  no tolerance. Do not plan to drop it in from the front.
- **Along the 111.125 mm dimension:** 27.1 mm of opening is left over and must
  be covered.

### Mounting approach

Mount **behind** the door skin on an adapter plate, not through the opening:

1. Adapter plate — 1/8" aluminum or 16ga steel — covering the full
   85.725 × 111.125 opening, bolted to the existing bill-mech mounting studs.
2. Square window cut ~74–76 mm, centered on the active area. This leaves a few
   mm of overlap onto the glass border and hides the PCB edge, the OSD buttons,
   and the 27 mm of leftover opening.
3. Closed-cell foam gasket between plate and glass. **Never clamp metal
   directly onto the glass face.**
4. The 58.00 × 49.20 mm hole pattern is the Pi HAT pattern, so the plate can
   reuse standoffs from any Pi mount.

Because the active area is only 72.53 mm square inside an 85.7 mm opening,
there is ~6.5 mm of alignment slop per side. Much more forgiving than the 5"
panels considered earlier, which had ~1 mm.

**Do not add a separate polycarbonate cover sheet.** An air gap over a
projected-capacitive panel kills touch sensitivity. The bonded 6H glass *is*
the protective layer.

---

## Wiring

```
Raspberry Pi 5
   ├─ micro-HDMI  ──(micro-HDMI → HDMI-A cable)──►  LCD "Display" port (HDMI-A female)
   └─ USB-A       ──(USB-A → USB-C cable)────────►  LCD "Power&Touch" port (USB-C)
```

Pi 5 has micro-HDMI, the board has full-size HDMI-A. **A micro-HDMI → HDMI-A
cable or adapter is required.** [VERIFY] whether one is in the box.

### Touch: use USB-C, not I2C

The board has a physical `USB` / `I2C` slide switch selecting the touch path.

| Mode | How it works | Verdict |
|---|---|---|
| **USB-C** | Standard HID over USB. Driver-free on every OS. | **Use this.** |
| I2C | Via the 40-pin header pogo pins. Requires `dtoverlay=waveshare-4dpic-3b/4b/5b` plus three `.dtbo` files copied into `/boot/overlays/` — they are not in stock Raspberry Pi OS. | Avoid. |

USB-C also carries the display's power, so it is one cable for both.

**Power budget note:** the LCD is powered from a Pi USB port. The MDB chain
already occupies a USB port with the Prolific RS232 adapter. Pi 5 has four
ports so there is no contention, but the Pi's PSU now carries the display too —
use the official 5V/5A supply, not a phone charger. If the display browns out
or the Pi throttles, power the LCD from a separate USB supply instead.

---

## Display configuration

### What the wiki says (legacy path)

The wiki gives this `config.txt` block:

```
dtparam=i2c_arm=on
dtoverlay=waveshare-4dpic-3b
dtoverlay=waveshare-4dpic-4b
dtoverlay=waveshare-4dpic-5b
hdmi_force_hotplug=1
config_hdmi_boost=10
hdmi_group=2
hdmi_mode=87
hdmi_timings=720 0 100 20 100 720 0 20 8 20 0 0 0 60 0 48000000 6
start_x=0
gpu_mem=128
```

### [VERIFY] This block will not work as-is on a Pi 5

That is legacy firmware-KMS syntax. **Raspberry Pi 5 has no legacy display
path** — it runs `vc4-kms-v3d` exclusively, and the `hdmi_group`, `hdmi_mode`,
`hdmi_timings`, `config_hdmi_boost`, `start_x` and `gpu_mem` keys are ignored.
The `waveshare-4dpic-*` overlays are only for I2C touch, which we are not using.

Try in this order:

1. **Nothing.** The panel advertises 720×720 over EDID — that is why Waveshare
   says Windows needs no configuration. KMS on Bookworm should also
   auto-detect it. Plug it in first and see.
2. If the mode is wrong, force it in `/boot/firmware/cmdline.txt`:
   ```
   video=HDMI-A-1:720x720M@60
   ```
3. If EDID is not being read at all, add to `config.txt`:
   ```
   hdmi_force_hotplug=1
   ```
   (still honoured on Pi 5 as a KMS hint)

Record what actually worked here once the panel is in hand.

---

## Trade-offs accepted

- **No software backlight control.** This is an HDMI panel, so there is no
  `/sys/class/backlight/*/brightness` entry the way a DSI panel would have.
  Brightness and contrast are set through the **OSD menu buttons on the left
  edge of the board** (Power / Menu / Up-Right / Down-Left / Exit).

  **Set brightness before final assembly.** Once the adapter plate is on, those
  buttons are unreachable. That is desirable for a public machine — a customer
  cannot power the screen off — but it means no scheduled overnight dimming
  without the hardware mod below.

- **Optional PWM backlight mod.** Remove the soldered resistor on the board and
  wire the exposed pad to Pi GPIO18 (P1). 5–30 kHz. Then
  `gpio -g pwm 18 <0–1024>`. Only worth doing if overnight dimming or burn-in
  turns out to matter.

- **Audio available if wanted.** 3.5 mm jack plus a 4-pin P2.0 header
  (`* + - - + *`) driving an 8Ω / 5W speaker, fed from HDMI audio. A short chime
  on invoice settlement would be good vend UX — the customer is looking at their
  phone, not the screen, at that moment.

---

## UI implications

The Lightning QR UI should be built for a **720×720 square viewport**, not
scaled down from a widescreen layout.

### Density

The 33-module / ~1.9 mm-per-module estimate originally written here was
optimistic — it assumed a much shorter payload than a real payment request.

Budget for a 560 px QR card with a 44 px quiet zone, leaving 472 px of QR:

| Payload length | Modules | mm/module |
|---|---|---|
| ~270 chars | 49 | **0.97** |
| ~360 chars | 57 | **0.83** |

Still above the ~0.5 mm floor where phone cameras start failing, but roughly
half the original figure. Consequences:

- The QR should get the entire vertical budget the layout can spare.
- If the payload is case-insensitive, **uppercase it before encoding** — that
  unlocks QR alphanumeric mode instead of byte mode, about 30% denser.
- ECC level L. At this payload length anything higher costs modules the panel
  does not have, and a customer can simply rescan.

Payload length is the payment side's to control — see
[`PAYMENT_INTERFACE.md`](PAYMENT_INTERFACE.md).

### Screen states

These follow the **select-first** flow confirmed on the bench in
`MDB_HACKING.md`, so the MDB session leads and the screen follows — the opposite
of what this section originally described.

| State | Screen |
|---|---|
| Idle | Branding, "Make your selection" (MDB session armed) |
| Vend request | Selection + price + QR + countdown |
| Settled | "Payment received", then "Dispensing" |
| Vend complete | "Enjoy", return to idle |
| Deadline hit | "Payment timed out — nothing was charged" |
| Paid but not dispensed | Apology + support contact |
| Link down | "Temporarily out of service" |

Note the open problem in `MDB_HACKING.md`: while the MDB session is armed, the
machine's **own** 16×1 display advertises a credit nobody has paid. This panel
cannot fix that — it needs solving on the MDB side.

---

## Open items

- [ ] Confirm box contents — is a micro-HDMI → HDMI-A cable included?
- [ ] Confirm 720×720 auto-detects on Pi 5 / Bookworm with no config
- [ ] Measure assembled depth behind the door skin; confirm it clears the
      VE5801 UCB and the existing wiring loom
- [ ] Decide whether to keep touch at all — leaning **no**, since the flow needs
      no customer input (the machine's own buttons are the only control
      surface) and disabling touch removes a failure mode on a public machine.
      Held open because one candidate fix for the fake-credit problem in
      `MDB_HACKING.md` is a presence trigger, which might want a tap.

---

## References

- Wiki: https://www.waveshare.com/wiki/4inch_HDMI_LCD_(C)
- Product page: https://www.waveshare.com/4inch-hdmi-lcd-c.htm
- Dimension drawing: https://www.waveshare.com/img/devkit/LCD/4inch-HDMI-LCD-C/Exterior-Size.jpg
