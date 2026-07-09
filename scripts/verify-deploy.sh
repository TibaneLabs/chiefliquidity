#!/bin/bash
# Verify the deployed program against this repo via solana-verify (OtterSec).
#
# Usage: ./scripts/verify-deploy.sh [commit-hash]
# If no commit hash is provided, uses the current HEAD. Use the commit the
# deployed CI artifact was built from so the on-chain hash matches.

set -e

export PATH="$HOME/.cargo/bin:$PATH"

PROGRAM_ID="ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw"
REPO_URL="https://github.com/TibaneLabs/chiefliquidity"
LIBRARY_NAME="chiefliquidity"

COMMIT="${1:-$(git rev-parse HEAD)}"

echo "Verifying program $PROGRAM_ID"
echo "Repo:   $REPO_URL"
echo "Commit: $COMMIT"
echo ""

solana-verify verify-from-repo --remote -y \
  --program-id "$PROGRAM_ID" \
  --commit-hash "$COMMIT" \
  --library-name "$LIBRARY_NAME" \
  "$REPO_URL"
