#!/bin/bash
# Run full E2E tests for ChiefLiquidity
#
# This script:
# 1. Builds the program (cargo build-sbf)
# 2. Starts a test validator with the program pre-deployed
# 3. Runs the TypeScript E2E suite against it
# 4. Cleans up

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
PROGRAM_ID="D8K39AXioKew7kLfKEjsBtW3BuDXnYqntk2z4PWxzPAW"

AGAVE_LOCAL="$HOME/.local/share/solana/install/active_release/bin"
if [ -x "/pkg/main/net-p2p.agave.core/bin/solana" ]; then
    SOLANA_BIN="/pkg/main/net-p2p.agave.core/bin"
elif [ -x "$AGAVE_LOCAL/solana" ]; then
    SOLANA_BIN="$AGAVE_LOCAL"
else
    SOLANA_BIN=""
fi
VALIDATOR_BIN="${SOLANA_BIN:+$SOLANA_BIN/}solana-test-validator"

cd "$PROJECT_DIR"

echo "=== ChiefLiquidity E2E Test Runner ==="
echo ""

echo "Step 1: Building program..."
./scripts/build-sbf.sh
echo ""

PROGRAM_SO="$PROJECT_DIR/target/deploy/chiefliquidity.so"
if [ ! -f "$PROGRAM_SO" ]; then
    echo "ERROR: Program not found at $PROGRAM_SO"
    exit 1
fi

if [ ! -f "$HOME/.config/solana/id.json" ]; then
    echo "Generating default keypair..."
    "${SOLANA_BIN:+$SOLANA_BIN/}solana-keygen" new --no-bip39-passphrase
fi

echo "Step 2: Starting test validator with program deployed..."
LEDGER_DIR="$PROJECT_DIR/test-ledger"
rm -rf "$LEDGER_DIR"

$VALIDATOR_BIN \
    --ledger "$LEDGER_DIR" \
    --rpc-port 8899 \
    --faucet-port 9900 \
    --slots-per-epoch 32 \
    --upgradeable-program "$PROGRAM_ID" "$PROGRAM_SO" "$HOME/.config/solana/id.json" \
    --log &
VALIDATOR_PID=$!

cleanup() {
    echo ""
    echo "Cleaning up..."
    if [ -n "$VALIDATOR_PID" ]; then
        kill $VALIDATOR_PID 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "Waiting for validator to start..."
sleep 5
if ! kill -0 $VALIDATOR_PID 2>/dev/null; then
    echo "ERROR: Validator failed to start"
    exit 1
fi

echo ""
echo "Step 3: Running E2E tests..."
cd "$PROJECT_DIR/tests/typescript"
if [ ! -d "node_modules" ]; then
    echo "Installing test dependencies..."
    npm install
fi
npm test

echo ""
echo "=== E2E Tests Complete ==="
