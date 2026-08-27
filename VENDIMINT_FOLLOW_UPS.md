# Vendimint follow-up work

This file records Vendimint integration work that is intentionally outside the
first kiosk and manager implementation. None of these items should be mistaken
for permission to release an inventory reservation while a Lightning receive
operation can still become funded.

## Prune finalized receive operations

Vendimint and the underlying Fedimint client persist receive-operation history
after an invoice reaches its final `Funded` or `Expired` state. The current
Vendimint API does not expose an operation-retention or pruning facility.

This is not an immediate correctness problem: invoices have short expirations,
their state machines terminate, and the kiosk can release inventory after an
authoritative `Expired` result. Over a long-running, high-volume deployment,
however, finalized operation metadata may cause the local database to grow
without a bound.

Before treating the kiosk as maintenance-free for long deployments:

- measure finalized-operation storage growth under realistic transaction load;
- determine which Fedimint operation records must remain for recovery and
  payment sweeping;
- add an upstream or Vendimint API for safely pruning only finalized operations;
- define a conservative retention period; and
- test pruning across process termination and restart.

Pending or otherwise unresolved operations must never be pruned.

## Safe unpairing and re-pairing

Vendimint machines are persistently bound to the manager identity that claims
them. Version one will not expose an ordinary unpair action. A future physical
administrator flow should provide an intentionally destructive way to erase
only the payment identity and pair with a replacement manager, with clear
warnings about consequences for unresolved payments.

## Manager identity backup and recovery

Version one assumes one authoritative manager installation and does not provide
a backup workflow. Losing that installation's Vendimint identity prevents a
fresh manager identity from authenticating as the manager that originally
claimed each kiosk, regardless of whether the kiosk admin PIN is known.

A future recovery design should identify the exact manager identity and wallet
material that must be backed up, protect it appropriately, and test restoring a
manager that can reconnect to already-claimed kiosks and continue sweeping
payments.
