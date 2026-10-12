#!/bin/sh
# Host gate: format, lints, tests, and no_std builds of the device crate for
# the RISC-V ESP targets (C3; C6/H2). S3 (Xtensa) is checked in CI.
set -eu
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# esp-hal enables portable-atomic's single-core fallback in real C3 apps.
RUSTFLAGS="--cfg portable_atomic_unsafe_assume_single_core" \
  cargo check -p esp-lan-ota --features rs-matter-0-2 --target riscv32imc-unknown-none-elf
cargo check -p esp-lan-ota --features rs-matter-0-2 --target riscv32imac-unknown-none-elf
cargo check -p esp-lan-ota --target riscv32imc-unknown-none-elf
