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
GoZxsxr2Na4auUuY7TMRi8psnU2X9NtnE73CE5cHieF
```

This is the program's fixed on-chain address, baked into the binary via
`declare_id!` and used by every client and script. The same vanity address is
used on all clusters (its keypair lives at
`target/deploy/chiefliquidity-keypair.json`). It has **not** yet been deployed
to a public cluster — deploy with `./scripts/deploy-program.sh` and confirm the
on-chain bytecode matches a local build with `./scripts/verify-deploy.sh`.

## License

MIT — see [`LICENSE`](LICENSE).

## Security

See [`SECURITY.md`](SECURITY.md). Please report vulnerabilities via GitHub
Security Advisories rather than public issues.
