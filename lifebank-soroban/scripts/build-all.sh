#!/bin/bash

set -e

# Lifebank contracts build script.
#
# Builds every contract in dependency order so that `contractimport!` /
# `contractclient` consumers (notably the coordinator) always see freshly
# built WASMs:
#
#   1. interfaces   (shared cross-contract types + clients, no WASM)
#   2. domain       (inventory, requests, payments, identity, matching,
#                    temperature, delivery, reputation, analytics)
#   3. coordinator  (imports the domain WASMs)
#
# Usage:
#   ./scripts/build-all.sh                # build all contracts
#   ./scripts/build-all.sh --check-specs  # build + diff specs vs snapshots
#   ./scripts/build-all.sh --update-specs # build + rewrite spec snapshots
#
# The `--check-specs` mode is what CI runs: it fails if any contract's
# interface (spec XDR) differs from the committed snapshot under
# `lifebank-soroban/specs/`. Any interface change therefore requires an
# explicit snapshot update in the PR, making breaking changes (e.g. a
# reordered `contracttype` enum variant) visible at review time instead of
# surfacing as a runtime mis-decode.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
SPECS_DIR="${ROOT_DIR}/specs"
WASM_DIR="${ROOT_DIR}/target/wasm32-unknown-unknown/release"

MODE="build"
case "${1:-}" in
    "")
        MODE="build"
        ;;
    --check-specs)
        MODE="check-specs"
        ;;
    --update-specs)
        MODE="update-specs"
        ;;
    -h|--help)
        echo "Usage: $0 [--check-specs|--update-specs]"
        exit 0
        ;;
    *)
        echo "❌ Unknown argument: $1" >&2
        echo "Usage: $0 [--check-specs|--update-specs]" >&2
        exit 1
        ;;
esac

# Domain contracts, in dependency order. The coordinator is built last.
DOMAIN_CONTRACTS=(
    inventory
    requests
    payments
    identity
    matching
    temperature
    delivery
    reputation
    analytics
)
ALL_CONTRACTS=("${DOMAIN_CONTRACTS[@]}" coordinator)

cd "${ROOT_DIR}"

echo "🔨 Building all Lifebank contracts (mode: ${MODE})..."
echo ""

# ---------------------------------------------------------------------------
# 1. Shared interface crate
# ---------------------------------------------------------------------------
echo "🧩 Building shared interfaces crate..."
cargo build --release -p interfaces
echo "✅ interfaces built"
echo ""

# ---------------------------------------------------------------------------
# 2. Domain contracts (dependency order)
# ---------------------------------------------------------------------------
echo "🏗️  Building domain contracts..."
for contract in "${DOMAIN_CONTRACTS[@]}"; do
    echo "  → ${contract}"
    cargo build --release --target wasm32-unknown-unknown -p "${contract}"
done
echo "✅ Domain contracts built"
echo ""

# ---------------------------------------------------------------------------
# 3. Coordinator (imports domain WASMs via contractimport!)
# ---------------------------------------------------------------------------
echo "🔗 Building coordinator (imports domain WASMs)..."
cargo build --release --target wasm32-unknown-unknown -p coordinator
echo "✅ Coordinator built"
echo ""

echo "✅ Build complete!"
echo ""
echo "📦 Contract artifacts:"
for contract in "${ALL_CONTRACTS[@]}"; do
    printf "  - %-12s %s\n" "${contract}:" "${WASM_DIR}/${contract}_contract.wasm"
done
echo ""

# ---------------------------------------------------------------------------
# Optional: Optimize WASM files
# ---------------------------------------------------------------------------
if command -v soroban &> /dev/null; then
    echo "🔧 Optimizing WASM files..."

    for contract in "${ALL_CONTRACTS[@]}"; do
        wasm="${WASM_DIR}/${contract}_contract.wasm"
        if [ -f "${wasm}" ]; then
            soroban contract optimize --wasm "${wasm}"
        fi
    done

    echo "✅ Optimization complete!"
    echo ""
fi

# ---------------------------------------------------------------------------
# Spec snapshot handling
# ---------------------------------------------------------------------------
if [ "${MODE}" = "build" ]; then
    exit 0
fi

# Extract a contract's spec XDR. Prefers `stellar contract inspect` and falls
# back to the legacy `soroban contract inspect` binary.
extract_spec() {
    local wasm="$1"
    if command -v stellar &> /dev/null; then
        stellar contract inspect --wasm "${wasm}" --output xdr-base64 2>/dev/null \
            || stellar contract inspect --wasm "${wasm}" 2>/dev/null
    elif command -v soroban &> /dev/null; then
        soroban contract inspect --wasm "${wasm}" 2>/dev/null
    else
        echo "❌ Neither 'stellar' nor 'soroban' CLI found; cannot extract specs." >&2
        return 1
    fi
}

mkdir -p "${SPECS_DIR}"

drift=0
for contract in "${ALL_CONTRACTS[@]}"; do
    wasm="${WASM_DIR}/${contract}_contract.wasm"
    snapshot="${SPECS_DIR}/${contract}.spec.xdr"

    if [ ! -f "${wasm}" ]; then
        echo "❌ Missing WASM for ${contract}: ${wasm}" >&2
        drift=1
        continue
    fi

    current="$(extract_spec "${wasm}")" || {
        echo "❌ Failed to extract spec for ${contract}" >&2
        drift=1
        continue
    }

    if [ "${MODE}" = "update-specs" ]; then
        printf '%s\n' "${current}" > "${snapshot}"
        echo "📝 Updated spec snapshot: specs/${contract}.spec.xdr"
        continue
    fi

    # --check-specs
    if [ ! -f "${snapshot}" ]; then
        echo "❌ Missing spec snapshot for ${contract}: specs/${contract}.spec.xdr" >&2
        echo "   Run ./scripts/build-all.sh --update-specs and commit the result." >&2
        drift=1
        continue
    fi

    if ! diff -u "${snapshot}" <(printf '%s\n' "${current}") > /tmp/lifebank-spec-diff.$$ 2>&1; then
        echo "❌ Spec drift detected for ${contract}:" >&2
        cat /tmp/lifebank-spec-diff.$$ >&2
        echo "" >&2
        drift=1
    else
        echo "✅ ${contract}: spec matches snapshot"
    fi
    rm -f /tmp/lifebank-spec-diff.$$
done

if [ "${MODE}" = "update-specs" ]; then
    echo ""
    echo "✅ Spec snapshots updated. Review and commit the changes under specs/."
    exit 0
fi

if [ "${drift}" -ne 0 ]; then
    echo "" >&2
    echo "❌ Contract spec drift detected." >&2
    echo "   If this interface change is intentional, run:" >&2
    echo "     ./scripts/build-all.sh --update-specs" >&2
    echo "   and commit the updated snapshots under lifebank-soroban/specs/." >&2
    echo "   NOTE: contracttype enums used across contract boundaries are" >&2
    echo "   append-only; removing/reordering variants requires a schema" >&2
    echo "   version bump (see #31)." >&2
    exit 1
fi

echo ""
echo "✅ All contract specs match committed snapshots."