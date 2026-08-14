# LightningVEND kiosk GUI

The `lv-kiosk` binary is the first simulator-backed shell for the 480×800
portrait touchscreen. It deliberately does not open the MDB serial port yet.
Its job is to make the customer, inventory, promo, maintenance, and persistence
state machines testable before they can authorize physical vends.

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
window. Development mode is a fixed logical 480×800 window, matching an
800×480 panel rotated to portrait.

The panel labeled **SIMULATED VENDING KEYPAD** stands in for selections that the
VMC will eventually deliver through `MdbSession` events.

### Promo exercise

1. Enter code `123456`.
2. Press simulated `A1` or `A2`; both contain the same granted product.
3. Choose `VEND SUCCESS`.
4. Observe that the code claims one product and only the selected slot loses
   inventory.
5. Try `VEND FAILURE` to see the reservation released and the slot enter
   **Needs attention**.

### Lightning exercise

1. Press simulated `B1` or `B2` before entering a promo code.
2. The 45-second placeholder payment page appears.
3. **Vend** simulates an accepted Lightning hold invoice; **Cancel** denies it.
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
- Lightning prices, in cents and currently restricted to ten-cent increments.

`redb` owns:

- expected per-slot inventory, which defaults to zero;
- independent slot health (`Ready` or `NeedsAttention`);
- product-level promo grants, claims, and reservations;
- transaction history and uncertain results.

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

## Application and MDB sessions

The eventual controller will continuously re-arm short-lived MDB sessions while
the GUI maintains a longer customer session:

```text
customer promo session
    ├── MDB session: vend first product, complete, re-arm
    ├── MDB session: vend second product, complete, re-arm
    └── Done / 20 seconds idle: end customer session
```

Lightning remains machine-first and has a 45-second decision window because the
AP113 VMC ends an unanswered vend at about 60 seconds. Promo remains
screen-first, so authentication and entitlement display happen before a
physical selection.

## Persistence boundary

The current store serializes the complete small mutable state as one JSON value
inside a single ACID `redb` write transaction. This keeps related changes—such
as inventory, a promo claim, and transaction status—atomic without prematurely
committing to a granular database schema.

On restart:

- an invoice that never reached vend approval becomes cancelled;
- a transaction that may have been approved becomes uncertain;
- its slot becomes `NeedsAttention`;
- a promo reservation remains held for administrator review.

## Integration boundary

The simulator currently calls `KioskEngine::machine_selected` directly. The MDB
integration should replace only that source of machine events and the three
simulated result buttons:

- `SessionEvent::VendRequested` → map AP113 item bytes to `SlotId`, then call
  `machine_selected`;
- an approved domain outcome → persist first, then call `PendingVend::approve_for`;
- `VendSucceeded`, `VendFailed`, cancellation, or uncertain disconnect → the
  corresponding engine transition.

Before real money is enabled, the outstanding ACK/retransmission and adapter
deadline issues in `MDB_ACTOR_FOLLOW_UPS.md` still need resolving.

## Not implemented yet

- live MDB controller integration;
- actual Lightning hold invoices and QR generation;
- production secret handling for the admin PIN;
- automatic Raspberry Pi startup and display rotation configuration;
- a promo CSV generation/import utility beyond the parser and sample file;
- product management in the admin UI.
