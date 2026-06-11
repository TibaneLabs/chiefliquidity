#!/bin/bash
# Build script for Solana BPF programs
#
# This script sets up the correct environment for building Solana programs
# using cargo-build-sbf with the platform-tools.

set -e

# Solana installation paths (prefer /pkg/main, then agave-install, then PATH)
AGAVE_LOCAL="$HOME/.local/share/solana/install/active_release/bin"
if [ -d "/pkg/main/net-p2p.agave.core/bin" ]; then
    SOLANA_BIN="/pkg/main/net-p2p.agave.core/bin"
elif [ -d "$AGAVE_LOCAL" ]; then
    SOLANA_BIN="$AGAVE_LOCAL"
else
    SOLANA_BIN="$(dirname "$(command -v solana 2>/dev/null || echo /usr/bin/solana)")"
fi
if [ -d "/pkg/main/dev-lang.rust.core.1.86.0/bin" ]; then
    RUST_BIN="/pkg/main/dev-lang.rust.core.1.86.0/bin"
else
    RUST_BIN="$(dirname "$(command -v rustc 2>/dev/null || echo /usr/bin/rustc)")"
fi
# For /pkg/main installs, use the custom SBF SDK and platform-tools paths
if [ -d "/pkg/main/net-p2p.agave.core/bin" ]; then
    SBF_SDK="$HOME/.cache/solana-sbf-sdk"
    PLATFORM_TOOLS_RUST="$HOME/.cache/solana/v1.52/platform-tools/rust/bin"
    export PATH="$PLATFORM_TOOLS_RUST:$SOLANA_BIN:$RUST_BIN:$PATH"

    if [ ! -d "$HOME/.cache/solana/v1.52/platform-tools" ]; then
        echo "Installing Solana platform-tools..."
        mkdir -p "$SBF_SDK/dependencies"
        cargo build-sbf --install-only --no-rustup-override --sbf-sdk "$SBF_SDK"
    fi

    SBF_ARGS=(--no-rustup-override --skip-tools-install --sbf-sdk "$SBF_SDK")
else
    # Install platform-tools if not already present (provides sbpf rustc)
    if [ ! -d "$HOME/.cache/solana/v1.52/platform-tools" ]; then
        echo "Installing Solana platform-tools..."
        cargo build-sbf --install-only --no-rustup-override
    fi
    PLATFORM_TOOLS_RUST="$HOME/.cache/solana/v1.52/platform-tools/rust/bin"
    export PATH="$PLATFORM_TOOLS_RUST:$SOLANA_BIN:$RUST_BIN:$PATH"
    SBF_ARGS=(--no-rustup-override --skip-tools-install)
fi

# Default to building all programs in the workspace
if [ $# -eq 0 ]; then
    echo "Building all Solana programs..."
    cargo build-sbf "${SBF_ARGS[@]}" --workspace "$@"
else
    echo "Building with args: $@"
    cargo build-sbf "${SBF_ARGS[@]}" "$@"
fi
