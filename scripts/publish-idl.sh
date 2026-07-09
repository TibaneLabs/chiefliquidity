#!/bin/bash
# Publish or update the on-chain IDL so explorers (Solscan, etc.) can decode
# this program's instructions and accounts.
#
# Usage: ./scripts/publish-idl.sh [--rpc <url>] [--keypair <path>] [--idl <path>]
#
# Defaults:
#   RPC:     from `solana config` (falls back to mainnet-beta) — keeps a private
#            Helius URL out of source.
#   Keypair: ~/.config/solana/id.json (must be the program's upgrade authority)
#   IDL:     ./idl.json
#
# The IDL is minified before upload to reduce on-chain size and transaction
# count. A higher priority fee is used for reliability on public RPCs.

set -e

PROGRAM_ID="ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw"
IDL_FILE="$(cd "$(dirname "$0")/.." && pwd)/idl.json"
KEYPAIR="$HOME/.config/solana/id.json"

AGAVE_LOCAL="$HOME/.local/share/solana/install/active_release/bin"
if [ -x "/pkg/main/net-p2p.agave.core/bin/solana" ]; then
    SOLANA_CLI="/pkg/main/net-p2p.agave.core/bin/solana"
elif [ -x "$AGAVE_LOCAL/solana" ]; then
    SOLANA_CLI="$AGAVE_LOCAL/solana"
else
    SOLANA_CLI="solana"
fi

# Default RPC from the CLI config (so the private Helius URL is never in source).
RPC_URL="$("$SOLANA_CLI" config get 2>/dev/null | awk '/^RPC URL:/ {print $3}')"
RPC_URL="${RPC_URL:-https://api.mainnet-beta.solana.com}"
PRIORITY_FEES=1000000

while [[ $# -gt 0 ]]; do
    case $1 in
        --rpc)     RPC_URL="$2";  shift 2 ;;
        --keypair) KEYPAIR="$2";  shift 2 ;;
        --idl)     IDL_FILE="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--rpc <url>] [--keypair <path>] [--idl <path>]"
            exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [ ! -f "$IDL_FILE" ]; then
    echo "Error: IDL file not found: $IDL_FILE"
    exit 1
fi
if [ ! -f "$KEYPAIR" ]; then
    echo "Error: Keypair not found: $KEYPAIR"
    exit 1
fi

# Minify the IDL to reduce on-chain size and transaction count.
MINIFIED=$(mktemp /tmp/idl-min-XXXXXX.json)
trap "rm -f $MINIFIED" EXIT
node -e "process.stdout.write(JSON.stringify(JSON.parse(require('fs').readFileSync('$IDL_FILE','utf8'))))" > "$MINIFIED"

ORIG_SIZE=$(wc -c < "$IDL_FILE")
MIN_SIZE=$(wc -c < "$MINIFIED")

echo "=== Publish IDL ==="
echo "Program:  $PROGRAM_ID"
echo "IDL file: $IDL_FILE ($ORIG_SIZE bytes, minified to $MIN_SIZE bytes)"
echo "RPC:      $RPC_URL"
echo "Keypair:  $KEYPAIR"
if [ -x "$SOLANA_CLI" ]; then
    BALANCE=$("$SOLANA_CLI" balance --keypair "$KEYPAIR" --url "$RPC_URL" 2>/dev/null || echo "unknown")
    echo "Balance:  $BALANCE"
fi
echo ""

npx @solana-program/program-metadata@latest write idl \
    "$PROGRAM_ID" \
    "$MINIFIED" \
    --keypair "$KEYPAIR" \
    --rpc "$RPC_URL"

echo ""
echo "IDL published successfully!"
echo "View on Solscan: https://solscan.io/account/$PROGRAM_ID"
