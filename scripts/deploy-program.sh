#!/bin/bash
# Deploy (or upgrade) the chiefliquidity program on the configured Solana cluster.
#
# By default this pulls the reproducible .so artifact from the latest successful
# CI run on master and deploys it to the canonical program ID. The RPC endpoint
# is taken from your `solana config` (so a private Helius URL never lives in
# source); just point your CLI at the right cluster before running.
#
#   --program-id is the vanity address GoZx…cHieF. On the FIRST deploy the
#   program account does not exist yet, so the program *keypair*
#   (~/.config/solana/chiefliquidity-program.json) is required to create it at
#   that address. Afterwards the program exists and only the upgrade authority
#   (your configured wallet) is needed; the script auto-detects which case applies.
#
# Usage:
#   ./scripts/deploy-program.sh                       # latest master CI artifact
#   ./scripts/deploy-program.sh --run-id 27322921869  # specific CI run
#   ./scripts/deploy-program.sh --so /tmp/chiefliquidity.so
#   ./scripts/deploy-program.sh --priority-fee 100000

set -euo pipefail

PROGRAM_ID="ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw"
PROGRAM_KEYPAIR="$HOME/.config/solana/chiefliquidity-program.json"
REPO="KarpelesLab/chiefliquidity"
WORKFLOW="CI"
ARTIFACT_NAME="chiefliquidity-verifiable"
ARTIFACT_FILE="chiefliquidity.so"
DEFAULT_PRIORITY_FEE=50000

AGAVE_LOCAL="$HOME/.local/share/solana/install/active_release/bin"
if [ -x "/pkg/main/net-p2p.agave.core/bin/solana" ]; then
    SOLANA_CLI="/pkg/main/net-p2p.agave.core/bin/solana"
elif [ -x "$AGAVE_LOCAL/solana" ]; then
    SOLANA_CLI="$AGAVE_LOCAL/solana"
else
    SOLANA_CLI="solana"
fi

LOCAL_SO=""
RUN_ID=""
PRIORITY_FEE="$DEFAULT_PRIORITY_FEE"

usage() {
    cat <<EOF
Usage: $0 [options]
  --so <path>           Deploy from a local .so file (skips CI download)
  --run-id <id>         Use a specific GitHub Actions run ID for the artifact
  --priority-fee <N>    Compute unit price, micro-lamports / CU (default: $DEFAULT_PRIORITY_FEE)
  -h, --help            Show this help

With no options: downloads the reproducible artifact from the latest successful
$WORKFLOW run on master and deploys program $PROGRAM_ID on the configured cluster.
EOF
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --so)             LOCAL_SO="$2";       shift 2 ;;
        --run-id)         RUN_ID="$2";         shift 2 ;;
        --priority-fee)   PRIORITY_FEE="$2";   shift 2 ;;
        -h|--help)        usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

# --- Resolve the .so file ------------------------------------------------------
if [ -n "$LOCAL_SO" ]; then
    PROGRAM_SO="$LOCAL_SO"
    SOURCE_DESC="local file"
else
    if ! command -v gh >/dev/null 2>&1; then
        echo "Error: 'gh' CLI not found. Install it, or pass --so <path>." >&2
        exit 1
    fi
    if [ -z "$RUN_ID" ]; then
        RUN_ID=$(gh run list -R "$REPO" --workflow "$WORKFLOW" --branch master \
                    --status success --limit 1 --json databaseId --jq '.[0].databaseId')
        if [ -z "$RUN_ID" ]; then
            echo "Error: no successful $WORKFLOW run found on master." >&2
            exit 1
        fi
    fi
    DL_DIR=$(mktemp -d -t chiefliquidity-deploy-XXXXXX)
    trap "rm -rf '$DL_DIR'" EXIT
    echo "Downloading $ARTIFACT_NAME from $REPO run $RUN_ID..."
    gh run download "$RUN_ID" -R "$REPO" -n "$ARTIFACT_NAME" -D "$DL_DIR"
    PROGRAM_SO="$DL_DIR/$ARTIFACT_FILE"
    SOURCE_DESC="CI run $RUN_ID"
fi

if [ ! -f "$PROGRAM_SO" ]; then
    echo "Error: program file not found: $PROGRAM_SO" >&2
    exit 1
fi

# --- Pre-flight ----------------------------------------------------------------
LOCAL_AUTH=$("$SOLANA_CLI" address)
RPC_URL=$("$SOLANA_CLI" config get | awk '/^RPC URL:/ {print $3}')
BALANCE=$("$SOLANA_CLI" balance | awk '{print $1}')
SO_SIZE=$(wc -c < "$PROGRAM_SO" | tr -d ' ')

# Does the program already exist on this cluster?
if "$SOLANA_CLI" program show "$PROGRAM_ID" >/dev/null 2>&1; then
    MODE="upgrade"
    CHAIN_AUTH=$("$SOLANA_CLI" program show "$PROGRAM_ID" | awk '/^Authority:/ {print $2}')
else
    MODE="initial"
    CHAIN_AUTH="(none — first deploy)"
fi

echo ""
echo "=== Deploy plan ==="
echo "Program ID:    $PROGRAM_ID"
echo "Mode:          $MODE"
echo "Source:        $SOURCE_DESC"
echo "Binary:        $PROGRAM_SO ($SO_SIZE bytes)"
echo "Cluster:       $RPC_URL"
echo "Payer/auth:    $LOCAL_AUTH ($BALANCE SOL)"
echo "On-chain auth: $CHAIN_AUTH"
echo "Priority fee:  $PRIORITY_FEE micro-lamports / CU"
echo ""

if [ "$MODE" = "initial" ]; then
    # First deploy: the program keypair creates the account at the vanity address.
    if [ ! -f "$PROGRAM_KEYPAIR" ]; then
        echo "Error: program keypair not found at $PROGRAM_KEYPAIR." >&2
        echo "       This file IS the program address; it cannot be regenerated." >&2
        exit 1
    fi
    KP_ADDR=$("$SOLANA_CLI" address -k "$PROGRAM_KEYPAIR")
    if [ "$KP_ADDR" != "$PROGRAM_ID" ]; then
        echo "Error: $PROGRAM_KEYPAIR is $KP_ADDR, not $PROGRAM_ID. Aborting." >&2
        exit 1
    fi
    echo "Initial deploy creates the program at $PROGRAM_ID with upgrade authority $LOCAL_AUTH."
    read -r -p "Proceed? [y/N] " ans
    [ "$ans" = "y" ] || [ "$ans" = "Y" ] || { echo "Aborted."; exit 1; }

    "$SOLANA_CLI" program deploy \
        --program-id "$PROGRAM_KEYPAIR" \
        --upgrade-authority "$LOCAL_AUTH" \
        --with-compute-unit-price "$PRIORITY_FEE" \
        "$PROGRAM_SO"
else
    # Upgrade: only the upgrade authority is needed.
    if [ "$LOCAL_AUTH" != "$CHAIN_AUTH" ]; then
        echo "Error: local keypair is NOT the upgrade authority. Aborting." >&2
        exit 1
    fi
    "$SOLANA_CLI" program deploy \
        --program-id "$PROGRAM_ID" \
        --with-compute-unit-price "$PRIORITY_FEE" \
        "$PROGRAM_SO"
fi

echo ""
echo "Deployment complete! Verify reproducibility with: ./scripts/verify-deploy.sh"
