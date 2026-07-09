# ChiefLiquidity

A Solana liquidation-aware AMM lending protocol.

ChiefLiquidity combines an automated market maker (AMM) with collateralized
lending in a single program where **liquidation is part of swap execution
itself**, not a separate keeper race.

The core invariant:

> No successful swap may leave the pool unable to satisfy its own executable
> obligations.

If a swap would push the pool past one or more loan liquidation thresholds, the
program automatically liquidates the affected loans **inside the same
instruction**, reprices the pool, and only then commits the swap. If the
post-liquidation execution exceeds the trader's slippage tolerance, the entire
transaction reverts atomically.

This converts bad debt into controlled inventory imbalance and price
repricing instead of protocol insolvency. There are no external liquidators;
keepers are not required.

Pools are **immutable and authority-less**: every economic parameter (swap fee,
liquidation ratio, max LTV, interest curve) is a fixed program constant set at
creation, there is no admin that can retune a pool, and creating a pool grants
no special rights. Both **SPL Token** and **Token-2022** mints are supported;
Token-2022 mints are restricted to an allowlist of extensions that are safe for
the pool to custody (fee-on-transfer, transfer-hook, permanent-delegate, and the
like are rejected).

See [`DESIGN.md`](DESIGN.md) for the on-chain design (account layouts, the
band + bitmap loan-ordering index, the swap-with-liquidation algorithm, and the
reserve-accounting math).

## Status

**Live on Solana mainnet-beta** as a reproducible,
[verified build](https://verify.osec.io/status/ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw).
v1 is feature-complete — AMM, collateralized lending, and in-swap liquidation —
and covered by unit, `solana-program-test` integration, live-validator E2E,
adversarial + invariant-fuzz, and compute-budget test suites (see `CI`).

It has **not been independently audited.** Use at your own risk.

## Build

```sh
cargo build               # host build, runs all unit tests via `cargo test`
cargo build-sbf           # SBF bytecode for deployment
```

## Program ID

```
ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw
```

This is the program's fixed on-chain address, baked into the binary via
`declare_id!` and used by every client and script. The same vanity address is
used on all clusters; its keypair (required only for the first deploy) lives
outside the repo at `~/.config/solana/chiefliquidity-program.json`.

**Live on mainnet-beta.** The on-chain bytecode is byte-identical to the
reproducible CI artifact (`CI` workflow → `chiefliquidity-verifiable`):

| | |
|---|---|
| Cluster | mainnet-beta |
| Upgrade authority | `5uf3zFBnFM291C7Yyn34zg5fHhVSN4fWxDgqNFYpH9G7` |
| Reproducible build | ✅ [verified (OtterSec)](https://verify.osec.io/status/ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw) |
| On-chain IDL | published (native/Borsh; explorers decode instructions + accounts) |
| programdata size | 335072 bytes (tight — a larger upgrade needs `solana program extend`) |

Fee redemption (`ClaimProtocolFees`) is gated on the **program's upgrade
authority**, not any per-pool authority.

Maintainer workflow:

```sh
./scripts/deploy-program.sh    # upgrade from the latest green CI artifact
./scripts/verify-deploy.sh     # confirm on-chain bytecode reproduces the repo
./scripts/publish-idl.sh       # (re)publish the on-chain IDL
```

## License

MIT — see [`LICENSE`](LICENSE).

## Security

See [`SECURITY.md`](SECURITY.md). Please report vulnerabilities via GitHub
Security Advisories rather than public issues.
