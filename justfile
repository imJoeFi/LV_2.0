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
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-targets --all-features
