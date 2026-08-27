# Lightning payment boundary

This document describes the durable seam between the kiosk state actor,
Vendimint, and MDB. The kiosk is authoritative for inventory and vend safety;
Vendimint creates and observes Lightning receives and moves completed funds to
the claimed manager.

## Money and price authority

Application money is always an exact integer number of millisatoshis (`Msats`).
There is no cents or floating-point money type. Catalog prices come only from
`config/kiosk.toml`, with a current mainnet testing floor of 100,000 msats (100
sats).

The VMC-provided price is deliberately ignored when deciding what the customer
owes or whether a selection is allowed. Once a vend is authorized, the kiosk
echoes the VMC's raw requested amount in `VEND APPROVED` solely to satisfy the
MDB protocol. That value never crosses into the payment domain.

## Payment window

The AP113 ends an unanswered vend request after roughly 60 seconds. The kiosk
shows a payable invoice for at most 40 seconds, leaving time to persist the
decision and deny MDB before its deadline. Network and payment observation run
asynchronously and may never block the MDB actor.

An invoice's payable lifetime must cover the entire interval during which it is
shown. The kiosk records the authoritative Vendimint operation ID, payment
hash, invoice, and expiration before displaying the QR.

## Durable ordering

The normal path is:

1. Validate the physical slot against local catalog, health, and inventory.
2. Durably create a purchase and reserve one unit of that slot.
3. Mark invoice creation in progress and request a Vendimint invoice.
4. Durably record the returned invoice and operation ID.
5. Display the QR.
6. Observe Vendimint's authoritative funded result.
7. Durably mark the purchase `AwaitingVend` before sending MDB approval.
8. Record success, known failure, or an uncertain vend outcome.

The state document is committed atomically to redb at every safety boundary.
The UI is a projection of durable state, not the owner of payment truth.

## Leaving an invoice

Once an invoice has been displayed, a user may leave after a warning. The kiosk
then marks the purchase abandoned and releases its inventory reservation so a
later customer is not blocked. This is a one-way transition: an abandoned
invoice can never authorize a vend, even if it is funded later.

The kiosk continues observing the abandoned Vendimint operation until it is
funded or expires. Expiration closes it normally. Late funding creates a
staff-assistance record; it does not vend. A small invoice-creation rate limit
and audit log discourage abuse without allowing abandoned invoices to pin
inventory.

## Vend outcomes after payment

Lightning is paid before vending; version one does not use hold invoices.

- `VEND SUCCESS`: decrement physical inventory and mark the purchase dispensed.
- Known `VEND FAILURE`: preserve inventory, mark the slot `NeedsAttention`, and
  tell the customer to find staff for out-of-band assistance/refund.
- Lost power or an unrecoverably ambiguous outcome after approval: mark the
  purchase uncertain, preserve inventory, block the slot, and require a manager
  to determine whether it dispensed.

The safety bias is to avoid a duplicate vend. A restart never repeats a vend
whose outcome is unknown.

## Vendimint and manager transport

The kiosk's Vendimint `Machine` owns payment creation/observation. The manager's
Vendimint `Manager` claims kiosks, sweeps completed receives, and supplies the
authenticated Iroh identity used by the LightningVEND manager ALPN.

`lv-core` owns the serializable purchase states, manager commands/events, and
versioned wire messages. `lv-vendimint` installs the claimed-manager-only Iroh
handler and length-delimited request/response framing. The kiosk state actor is
the only component allowed to apply a manager command or append a manager
event.

## Integration status

The kiosk can now opt into a persistent mainnet Vendimint machine. A dedicated
Tokio actor owns it, creates invoices, observes final funded/expired states,
forwards authenticated manager requests, and exposes physical claim
confirmation to Iced. The kiosk renders real invoice and pairing QR codes and
reattaches watchers for abandoned invoices recovered from redb.

The manager UI still needs claim initiation/scanning and command handling. The
agreed invoice creation rate limit and concurrent abandoned-invoice cap also
remain before public deployment. Real-money qualification must additionally
complete the MDB reliability items in `MDB_ACTOR_FOLLOW_UPS.md`.

Longer-term Vendimint maintenance and recovery work is tracked in
`VENDIMINT_FOLLOW_UPS.md`.
