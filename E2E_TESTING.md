# End-to-end testing

Run the system smoke test from macOS or Linux with:

```console
nix develop .#default --command just test-e2e
```

When already inside the default development shell, use `just test-e2e`.

The `devimint` Cargo dependency is intentionally pinned exactly to `0.11.1`,
matching the Fedimint daemon bundle selected by `flake.nix`. Upgrade both sides
together: a library/daemon patch-version mismatch activates Devimint's
backward-compatibility path instead of starting an ordinary federation.

The command enters a pinned Fedimint Nix environment, starts a disposable
regtest federation and its supporting processes, and runs the `lv-e2e-tests`
binary. All identities, databases, ports, and federation state are temporary.
The first run can take substantially longer while Nix and Cargo build the
Fedimint native dependencies.

The current scenario verifies that:

1. a Vendimint kiosk and manager can start with independent identities;
2. the manager can claim the kiosk and both sides see the same confirmation
   PIN;
3. the manager can send the regtest federation configuration to the kiosk;
4. the claim remains visible through the manager API; and
5. idempotent commands and ordered events cross LightningVEND's authenticated
   `lightningvend/manager/2` Iroh ALPN;
6. a Lightning slot is stocked remotely and durably reserved;
7. a real 100-sat BOLT11 is created and paid through the regtest gateway;
8. the funded purchase advances through a simulated successful MDB vend;
9. inventory and purchase completion reach the manager event log; and
10. the manager sweeps the completed payment.

This test is deliberately opt-in. `just check` excludes `lv-e2e-tests`, so
ordinary formatting, linting, and unit-test runs do not start daemons or pay the
native Fedimint build cost.

The scenario runs the real payment and manager transports with the headless
`KioskEngine`; it deliberately substitutes a typed simulated success for the
physical MDB adapter. Physical hardware acceptance and power-cycle cases remain
in `MDB_ACTOR_FOLLOW_UPS.md`. Future automated coverage should add explicit
restart scenarios, especially abandonment, funding during shutdown, and a
crash after vend authorization.

Rendered Iced screenshot tests should remain a separate deterministic suite.
They are valuable for stable screens and components, but federation timing,
fonts, animation, and asynchronous progress would make screenshots from this
system test noisy and difficult to diagnose.
