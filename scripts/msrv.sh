#!/usr/bin/env bash
# The minimum supported Rust versions, checked the same way by CI (job `msrv` in
# .github/workflows/ci.yml) and by scripts/preflight.sh: change them here only.
#
#   scripts/msrv.sh            run the checks (needs both toolchains installed)
#   scripts/msrv.sh versions   print the toolchains it needs, one per line
#
# Libraries only: the tests generate certificates with rcgen, whose dependencies
# need a newer compiler. giong-quic, and giong-http's unstable-http3 feature that
# uses it, need Rust 1.85 (quinn-proto's minimum since its 0.11.15 security fix,
# and its rand 0.10 dependency is edition 2024, which cargo 1.80 cannot parse):
# every other crate and feature is checked with 1.80, those two with 1.85.
set -euo pipefail
cd "$(dirname "$0")/.."
MSRV=1.80.0
MSRV_QUIC=1.85.0

if [ "${1:-}" = versions ]; then
    echo "$MSRV"
    echo "$MSRV_QUIC"
    exit 0
fi

echo "== Rust $MSRV: the workspace without giong-quic and giong-http"
cargo +"$MSRV" check --quiet --workspace --all-features --exclude giong-quic --exclude giong-http
echo "== Rust $MSRV: giong-http without unstable-http3"
cargo +"$MSRV" check --quiet -p giong-http --features tls,client
echo "== Rust $MSRV_QUIC: giong-quic and giong-http with every feature"
cargo +"$MSRV_QUIC" check --quiet -p giong-quic -p giong-http --all-features
