#!/bin/bash
# Verify the deployed program against this repo via solana-verify (OtterSec).
#
# Usage: ./scripts/verify-deploy.sh [commit-hash]
# If no commit hash is provided, uses the current HEAD. Use the commit the
# deployed CI artifact was built from so the on-chain hash matches.

set -e

export PATH="$HOME/.cargo/bin:$PATH"

PROGRAM_ID="GoZxsxr2Na4auUuY7TMRi8psnU2X9NtnE73CE5cHieF"
REPO_URL="https://github.com/KarpelesLab/chiefliquidity"
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
