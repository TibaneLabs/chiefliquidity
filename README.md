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

See [`DESIGN.md`](DESIGN.md) for the on-chain design (account layouts, the
band + linked-list loan-ordering index, the swap-with-liquidation algorithm,
and the reserve-accounting math).

## Status

Early development. The design is documented; the program is being implemented
incrementally. See `programs/chiefliquidity/src/instructions/` for the
instruction handlers that have landed so far.

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

**Deployed to mainnet-beta.** The live bytecode is byte-identical to the
reproducible CI artifact (`CI` workflow → `chiefliquidity-verifiable`):

| | |
|---|---|
| Cluster | mainnet-beta |
| Upgrade authority | `5uf3zFBnFM291C7Yyn34zg5fHhVSN4fWxDgqNFYpH9G7` |
| programdata size | 335072 bytes (tight — a larger upgrade needs `solana program extend`) |

Redeploy/upgrade with `./scripts/deploy-program.sh` (it downloads the latest
green CI artifact) and confirm reproducibility with `./scripts/verify-deploy.sh`.

## License

MIT — see [`LICENSE`](LICENSE).

## Security

See [`SECURITY.md`](SECURITY.md). Please report vulnerabilities via GitHub
Security Advisories rather than public issues.
