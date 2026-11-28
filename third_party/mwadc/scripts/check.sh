#!/usr/bin/env bash
# Off-Windows development check: type-check the Windows build and run the portable unit tests.
# Linking for Windows needs the MSVC linker, so this stops at `cargo check`.
# Needs rustup with the target:  rustup target add x86_64-pc-windows-msvc
set -euo pipefail
cd "$(dirname "$0")/.."
cargo check --workspace --target x86_64-pc-windows-msvc
cargo test --workspace
echo "ok: Windows type-check and portable tests pass (export also runs here)"
