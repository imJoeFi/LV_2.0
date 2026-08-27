# LightningVEND kiosk GUI

The `lv-kiosk` binary is the fixed 720×720 customer touchscreen application. MDB and
Lightning are configured independently: `--port` enables the live vending
machine, while `--vendimint-data` enables the persistent mainnet payment
machine. Omitting either option keeps that side simulated, so the command below
does not create a real invoice or touch vending hardware.

## Run the simulator

From the repository root:

```sh
cargo run --bin lv-kiosk -- --seed-demo
```

This creates `lv-kiosk.redb`, imports `config/promo_codes.csv`, and explicitly
stocks every configured slot with three simulated items. Without `--seed-demo`,
new slots correctly start with zero inventory.

The sample promo codes are:

- `123456`: two Sparkling Waters and one Event Enamel Pin
- `654321`: one Sparkling Water

The simulator admin PIN is `2468`. Override it with `--admin-pin`; do not use the
sample PIN in a deployed kiosk. Add `--fullscreen` for the borderless kiosk
window. Development mode is a fixed logical 720×720 window matching the
Waveshare display documented in `DISPLAY.md`.

The panel labeled **SIMULATED VENDING KEYPAD** stands in for selections that the
VMC delivers through `MdbSession` events in hardware mode.

## Run with the MDB machine

Only one process may own the PC2MDB serial port. Stop `mdb-flow-test` before
starting the GUI, then run:

```sh
cargo run --release --bin lv-kiosk -- \
  --port /dev/ttyUSB0 \
  --fullscreen
```

The baud rate defaults to 9600 and can be changed with `--baud`. For an empty
development database, add `--seed-demo` once to import the sample promo codes
and stock each configured slot with three items. That flag replaces the promo
codes and resets demo inventory every time it is used, so do not leave it in a
production startup command.

Hardware mode:

- opens an MDB session with unknown funds and automatically re-arms after every
  completed or cancelled session;
- maps the AP113 item bytes to catalog slots such as `A1` and `B2`;
- ignores the VMC price for catalog, payment, and authorization decisions;
- echoes the VMC's requested amount only in the MDB approval frame, while the
  Lightning amount comes exclusively from `config/kiosk.toml`;
- approves promo, free, and maintenance vends at zero MDB monetary units;
- saves the transaction or promo reservation before sending `VEND APPROVED`;
- denies unknown, unavailable, out-of-stock, and unauthorized selections;
- hides all simulated machine controls;
- marks ambiguous disconnects after approval as uncertain for administrator
  reconciliation; and
- fails closed, displays **Vending machine unavailable**, and retries the serial
  connection automatically.

The terminal running the GUI prints the raw MDB TX/RX trace for hardware
debugging. The actor uses a 46-second application response window to support the
40-second temporary Lightning screen. The adapter's stored MDB configuration
must advertise a compatible response time; changing the application setting
does not reprogram the adapter.

## Enable real Vendimint payments

Pass a dedicated persistent directory to enable Vendimint on Bitcoin mainnet:

```sh
cargo run --release --bin lv-kiosk -- \
  --vendimint-data data/vendimint-machine
```

On a new directory the kiosk displays a pairing QR. The manager initiates a
Vendimint claim, then a physically present operator compares the six-digit PIN
on both screens and presses **Confirm** on the kiosk. The kiosk remains in setup
until the claimed manager supplies a federation configuration. Its Vendimint
identity and wallet files must be treated as persistent application data and
must never be shared by two running kiosk processes.

With real payments enabled, the kiosk:

- runs Vendimint on a dedicated Tokio thread and installs the authenticated
  `lightningvend/manager/2` Iroh protocol on the same endpoint;
- durably reserves inventory before asking Vendimint for an invoice;
- durably stores the BOLT11, operation ID, payment hash, and expiration before
  displaying the QR;
- waits for Vendimint's funded/expired result instead of showing the simulator
  vend button;
- continues observing an abandoned invoice after releasing its inventory;
- never vends for a late payment to an abandoned invoice; and
- reattaches payment watchers for unresolved abandoned invoices after restart.

To use real MDB and real Lightning together on the Pi, supply both options:

```sh
./lv-kiosk \
  --port /dev/ttyUSB0 \
  --vendimint-data /var/lib/lightningvend/vendimint \
  --database /var/lib/lightningvend/lv-kiosk.redb \
  --catalog /etc/lightningvend/kiosk.toml \
  --fullscreen
```

Run the resizable, landscape manager application on the operator's MacBook:

```sh
cargo run --release --bin lv-manager -- --data data/vendimint-manager
```

The manager scans the kiosk QR with the MacBook camera (manual paste remains a
fallback through the kiosk's **Copy pairing payload** action), performs the
physical claim-PIN confirmation, configures the federation, and exposes remote
inventory, health, vend authorization, and assistance controls. macOS may ask
the terminal or packaged application for camera access the first time the
scanner opens.

Pairing, invoice, and ecash-export QRs use the shared `lv-ui` raster renderer.
It always produces a standard black-on-white square with integer-sized modules
and a four-module quiet zone, independent of the Iced theme, display scale, or
Tiny-Skia canvas transforms.

The manager also shows its exact millisatoshi wallet balance. **Export funds**
creates self-contained bearer ecash as QR and copyable text after an explicit
confirmation. Vendimint supports both mint generations and chooses mint v2 for
a newly joined federation that advertises both; wallets created by earlier
Vendimint versions remain pinned to mint v1. The manager displays the selected
version. Unclaimed mint-v1 exports are reclaimed after 24 hours. Mint-v2
exports do not support automatic reclaim, so their bearer tokens must be kept
safe until they have been claimed.

### Promo exercise

1. Enter code `123456`.
2. Press simulated `A1` or `A2`; both contain the same granted product.
3. Choose `VEND SUCCESS`.
4. Observe that the code claims one product and only the selected slot loses
   inventory.
5. Try `VEND FAILURE` to see the reservation released and the slot enter
   **Needs attention**.

### Simulated Lightning exercise

1. Press simulated `B1` or `B2` before entering a promo code.
2. A 40-second simulated invoice QR appears.
3. **Vend** simulates a paid Lightning invoice; the back arrow abandons it.
4. Choose the simulated VMC result. A failure preserves inventory and marks the
   slot for attention.

### Admin exercise

1. Press the gear and enter `2468`.
2. Adjust expected inventory independently of the catalog.
3. Arm one free vend; the next configured, stocked, healthy selection is
   approved and the arm clears.
4. Arm a slot-specific maintenance test. A successful test consumes one item
   and marks the slot ready; a failure leaves it needing attention.
5. Reconcile an uncertain transaction as dispensed or not dispensed.

## Domain model

The catalog and mutable machine state are intentionally separate.

`config/kiosk.toml` owns:

- stable product IDs, display names, and optional image paths;
- physical slot-to-product assignments;
- the payment policy for each slot;
- Lightning prices as exact integer millisatoshis. The current mainnet testing
  floor is 100,000 msats (100 sats).

`redb` owns:

- expected per-slot inventory, which defaults to zero;
- independent slot health (`Ready` or `NeedsAttention`);
- product-level promo grants, claims, and reservations;
- transaction history and uncertain results;
- Lightning invoice identity, purchase state, and per-slot inventory
  reservations;
- a salted Argon2id admin-PIN verifier (never the plaintext PIN); and
- a schema version around the disposable JSON state document.

`config/promo_codes.csv` grants products rather than slots:

```csv
code,product_id,quantity
123456,sparkling-water,2
123456,event-pin,1
```

A promo customer authenticates first and may then use any configured, stocked,
healthy promo slot containing a granted product. The entitlement is reserved
durably before approval, claimed only on `VEND SUCCESS`, and released on
failure. An uncertain outcome keeps the entitlement reserved until an
administrator reconciles it.

Production promo imports are separate from demo seeding. Validate a CSV without
changing the database:

```sh
lv-kiosk \
  --catalog /etc/lightningvend/kiosk.toml \
  --database /var/lib/lightningvend/lv-kiosk.redb \
  import-promo-codes --file codes.csv --dry-run
```

Replace every existing promo code and entitlement atomically with `--yes` in
place of `--dry-run`. Catalog and inventory are never modified by this command.

## Application and MDB sessions

The controller continuously re-arms short-lived MDB sessions while the GUI
maintains a longer customer session:

```text
customer promo session
    ├── MDB session: vend first product, complete, re-arm
    ├── MDB session: vend second product, complete, re-arm
    └── Done / 20 seconds idle: end customer session
```

Lightning remains machine-first and has a 40-second decision window because the
AP113 VMC ends an unanswered vend at about 60 seconds. Promo remains
screen-first, so authentication and entitlement display happen before a
physical selection.

Promo entry, browsing, and result screens return home after 20 seconds without
a meaningful action. A displayed Lightning invoice always follows its fixed
40-second payment window, dispensing is never interrupted by UI inactivity,
and a local admin session closes and disarms after two idle minutes.

Invoice creation is limited to six attempts per rolling minute. The kiosk also
allows at most three still-payable abandoned invoices per product and ten
kiosk-wide. These limits are derived from durable purchase history, survive a
restart, do not hold inventory, and show a retry countdown when reached.

## Persistence boundary

The current store serializes the complete small mutable state as one JSON value
inside a single ACID `redb` write transaction. This keeps related changes—such
as inventory, a promo claim, and transaction status—atomic without prematurely
committing to a granular database schema.

On restart:

- an invoice which was displayed but not funded becomes abandoned and its
  inventory reservation is released;
- that abandoned invoice remains tracked to a final funded/expired state and
  can never authorize a vend;
- a late payment on an abandoned invoice becomes a staff-assistance case;
- a paid transaction interrupted around vend approval becomes uncertain;
- its slot becomes `NeedsAttention`;
- a promo reservation remains held for administrator review.

## MDB integration boundary

A dedicated Tokio thread exclusively owns `MdbDevice`, each `MdbSession`, and
the current single-use `PendingVend`. Iced receives typed events and can only
send an approve or deny decision for the matching vend ID. Machine events are
prioritized over queued touchscreen decisions so that a buffered VMC
cancellation is handled before a nearly simultaneous approval.

`SessionEvent::VendRequested` is mapped to a `SlotId` and passed through the same
`KioskEngine::machine_selected` path used by the simulator. Successful and
failed vend reports then update inventory, entitlements, slot health, and
transaction history through the existing durable domain transitions.

Before real money is enabled, the outstanding ACK/retransmission and adapter
deadline issues in `MDB_ACTOR_FOLLOW_UPS.md` still need resolving.

## Intentionally deferred or pending physical qualification

- automatic Raspberry Pi startup and display configuration;
- product/catalog management in the admin UI;
- automatic refunds (version one uses operator-assisted resolution);
- manager identity backup and safe re-pairing; and
- the physical MDB reliability and power-cycle work in
  `MDB_ACTOR_FOLLOW_UPS.md`.
