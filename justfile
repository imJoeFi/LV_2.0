set shell := ["bash", "-euo", "pipefail", "-c"]

pi-rust-target := "aarch64-unknown-linux-gnu"
pi-zig-target := pi-rust-target + ".2.17"
pi-binary := "target/" + pi-rust-target + "/release/lv-kiosk"

# List the available project commands.
default:
    @just --list

# Cross-compile the kiosk release binary for a 64-bit Raspberry Pi 5.
build-pi:
    cargo zigbuild --locked --release -p lv-kiosk --bin lv-kiosk --target {{ pi-zig-target }}
    @echo "Built {{ pi-binary }}"

# Check, lint, and test every crate in the workspace.
check:
    cargo fmt --all --check
    cargo clippy --workspace --exclude lv-e2e-tests --all-targets --all-features -- -D warnings
    cargo test --workspace --exclude lv-e2e-tests --all-targets --all-features

# Start an ephemeral regtest federation and run the system integration test.
test-e2e:
    nix develop .#e2e --command just _test-e2e

[private]
_test-e2e:
    FM_ENABLE_MODULE_MINTV2=1 cargo run --locked -p lv-e2e-tests
