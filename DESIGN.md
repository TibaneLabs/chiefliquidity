# ChiefLiquidity — On-chain Design

Solana liquidation-aware AMM lending protocol. Native `solana-program` (no Anchor),
borsh-serialized accounts, file layout matching `../chiefstaker` (`programs/chiefliquidity/src/{lib.rs, error.rs, events.rs, math.rs, state.rs, instructions/}`).

This document covers what the swap-with-liquidation algorithm requires from on-chain
state. It does **not** specify instruction signatures or wire formats.

---

## 1. Invariant (the only thing that matters)

> After applying every liquidation triggered by a state transition, every executable
> outflow committed by the same transaction must be fully covered by **real** vault
> balances.

Everything below is in service of making this provable inside a Solana instruction
handler with bounded compute and a bounded number of pre-declared accounts.

---

## 2. Reserve model

Two reserve concepts per pool, per side:

| Symbol           | Meaning                                                     | Used for             |
|------------------|-------------------------------------------------------------|----------------------|
| `real_a`         | Vault A's actual SPL token balance                          | Settlement, revert checks |
| `real_b`         | Vault B's actual SPL token balance                          | Settlement, revert checks |
| `accounted_a`    | `real_a + Σ outstanding_debt_a`                             | AMM pricing, LP value |
| `accounted_b`    | `real_b + Σ outstanding_debt_b`                             | AMM pricing, LP value |

`outstanding_debt_x` is the sum of `borrowed_amount` across all open loans whose
**debt side** is `x`. Collateral does **not** appear in either reserve until
liquidation occurs.

We do **not** store `accounted_*` derived values in the pool. We store `real_*`
implicitly (via the vault accounts) plus `total_debt_a` and `total_debt_b` as
explicit `u128` running sums on the `Pool` account. This makes liquidation a
local update (`total_debt_x -= repaid`) and avoids drift.

### Pricing (CPMM, a stand-in)

For the design doc, assume constant-product on accounted reserves:

```
k = accounted_a × accounted_b
price_b_per_a = accounted_b / accounted_a
```

The exact invariant is a parameter — anything the math module can express with the
same `(accounted_a, accounted_b) → quote` interface is fine.

### Invariant restatement

After all triggered liquidations are applied to the simulated state and the swap
output is computed:

```
out_a ≤ real_a    (if A is the output side)
out_b ≤ real_b    (if B is the output side)
```

Otherwise the entire transaction reverts.

---

## 3. Loan trigger price (one number, two directions)

For a loan with collateral side `c` and debt side `d`, define `trigger_price` in
**B-per-A units** for the whole pool, regardless of which side is which:

| Collateral | Debt | Direction loan triggers when price… | `trigger_price` (B-per-A) |
|-----------:|:-----|:------------------------------------|:--------------------------|
| A          | B    | …falls below threshold (collateral A loses value relative to debt B) | `(debt_b × liq_ratio) / collateral_a` |
| B          | A    | …rises above threshold (collateral B loses value relative to debt A) | `collateral_b / (debt_a × liq_ratio)` |

Derivation: liquidation fires when
`collateral_value_in_debt_terms < debt × liq_ratio`:

- Collateral A, debt B:
  `collateral_a × price_b_per_a < debt_b × liq_ratio`
  → `price_b_per_a < (debt_b × liq_ratio) / collateral_a`
  → triggers when **price falls** below `trigger = (debt_b × liq_ratio) / collateral_a`.

- Collateral B, debt A:
  `collateral_b × price_a_per_b < debt_a × liq_ratio`
  `collateral_b / price_b_per_a < debt_a × liq_ratio`
  → `price_b_per_a > collateral_b / (debt_a × liq_ratio)`
  → triggers when **price rises** above `trigger = collateral_b / (debt_a × liq_ratio)`.

**Conclusion:** every loan has exactly one `trigger_price` (B-per-A units) and one
`trigger_direction`:

- `TriggerOnFall` — A-collateral loans (debt is B)
- `TriggerOnRise` — B-collateral loans (debt is A)

Any swap moves the price monotonically in **one** direction. So at most one of the
two trigger sets is in play per swap.

---

## 4. Account inventory

PDAs (seed conventions match `../chiefstaker` — `pub const FOO_SEED: &[u8] = b"...";`):

| Account            | Seeds                                            | Purpose |
|--------------------|--------------------------------------------------|---------|
| `Pool`             | `["pool", mint_a, mint_b]` (mints sorted)        | Per-pair config + reserve totals + band bitmaps |
| `Vault A`          | `["vault_a", pool]`                              | SPL token A holdings (real_a) |
| `Vault B`          | `["vault_b", pool]`                              | SPL token B holdings (real_b) |
| `LpMint`           | `["lp_mint", pool]`                              | LP share mint |
| `Loan`             | `["loan", pool, borrower, nonce]`                | Per-position loan state (carries its band_id) |
| `LoanIndexBand`    | `["band", pool, direction, band_id]`             | Per-band membership count (see §6) |
| `PoolMetadata`     | `["metadata", pool]`                             | Display name/url, optional (not implemented) |

Mints sorted lexicographically so `(A, B)` and `(B, A)` produce the same pool.

---

## 5. Account layouts

Field-by-field, with `LEN` totals matching `../chiefstaker`'s style. All accounts
lead with an 8-byte `discriminator` chosen as a random sentinel (not Anchor-derived).

### 5.1 `Pool`

```rust
pub struct Pool {
    pub discriminator: [u8; 8],

    // Identity
    pub mint_a: Pubkey,                  // 32
    pub mint_b: Pubkey,                  // 32
    pub vault_a: Pubkey,                 // 32
    pub vault_b: Pubkey,                 // 32
    pub lp_mint: Pubkey,                 // 32
    pub authority: Pubkey,               // 32   admin (renounceable)

    // PDA bumps
    pub pool_bump: u8,                   // 1
    pub vault_a_bump: u8,                // 1
    pub vault_b_bump: u8,                // 1
    pub lp_mint_bump: u8,                // 1

    // Reserve accounting (see §2)
    pub total_debt_a: u128,              // 16   Σ debt where debt side = A
    pub total_debt_b: u128,              // 16   Σ debt where debt side = B
    pub total_collateral_a: u128,        // 16   Σ collateral held against B-debt loans
    pub total_collateral_b: u128,        // 16   Σ collateral held against A-debt loans

    // Curve config
    pub curve_kind: u8,                  // 1    0 = CPMM, room for others
    pub swap_fee_bps: u16,               // 2    e.g. 30 = 0.30%
    pub protocol_fee_bps: u16,           // 2    skim of swap_fee for treasury
    pub _curve_pad: [u8; 3],             // 3    padding to keep alignment readable

    // Lending config (collateral health)
    pub liq_ratio_bps: u16,              // 2    e.g. 11000 = 110%
    pub max_ltv_bps: u16,                // 2    initial borrow cap (< 1 / liq_ratio)
    pub _lending_pad: [u8; 2],           // 2

    // Interest model — shared utilization-kink curve, applied per side (see §8).
    pub interest_base_bps_per_year: u16,   // 2  APR at zero utilization
    pub interest_slope1_bps_per_year: u16, // 2  added APR from 0 → kink
    pub interest_slope2_bps_per_year: u16, // 2  added APR from kink → 100%
    pub interest_kink_bps: u16,            // 2  kink point in bps of utilization

    // Per-side borrow indexes (monotone, WAD-scaled, ≥ WAD). See §8.
    pub borrow_index_a_wad: u128,        // 16   owed_a = principal·index_a/snapshot
    pub borrow_index_b_wad: u128,        // 16
    pub last_index_update_slot: u64,     // 8    slot both indexes last bumped

    // Counters
    pub open_loans: u64,                 // 8
    pub next_loan_nonce: u64,            // 8    pool-monotonic; see §5.3
    pub last_update_slot: u64,           // 8

    // Treasury accounting
    pub protocol_fees_a: u64,            // 8    skimmed; redeemable by authority
    pub protocol_fees_b: u64,            // 8

    // Band-presence bitmaps (see §6). Bit i set ↔ a LoanIndexBand PDA exists
    // for (pool, direction, band_id=i) with count > 0. 16 bytes = 128 bits;
    // band ids ≥ 128 (MAX_BAND_ID = 127) are not representable.
    pub band_bitmap_fall: [u8; 16],      // 16
    pub band_bitmap_rise: [u8; 16],      // 16

    pub _reserved: [u8; 32],             // 32   forward-compat
}
```

`LEN` = 8 + 32×6 + 4 + 16×4 + (1 + 2 + 2 + 3) + (2×2 + 2) + 2×4 + (16×2 + 8)
+ 8×3 + 8×2 + 16×2 + 32
= 8 + 192 + 4 + 64 + 8 + 6 + 8 + 40 + 24 + 16 + 32 + 32 = **434 bytes**
(verified by `state::tests::pool_size` borsh roundtrip).

Notes:
- `authority` is renounceable by setting to `Pubkey::default()`, same convention as
  `StakingPool`.
- The interest model started as a flat APR (one `interest_rate_bps_per_year`
  field); it now stores the four-parameter utilization-kink curve plus the two
  per-side borrow indexes that capitalize accrued interest lazily — see §8.
- `band_bitmap_*` are the on-chain source of truth for "which bands are
  populated", letting a swap prove it supplied every band a price move could
  cross without an off-chain `getProgramAccounts` walk — see §6.
- The Pool carries no loan-list head pointers or band counters: the bitmaps say
  which bands exist, and each `LoanIndexBand` carries its own membership count
  (§5.3). (The original `head_fall`/`head_rise`/`band_count_*` fields were
  removed when the linked-list index was retired — see §6.)
- Reserved bytes mirror chiefstaker's pattern of leaving room for new fields with
  `unwrap_or(0)` deserialize.

### 5.2 `Loan`

A loan is one position. Stored at `["loan", pool, borrower, nonce]` so a borrower
may hold multiple positions.

```rust
pub struct Loan {
    pub discriminator: [u8; 8],

    // Identity / back-references
    pub pool: Pubkey,                    // 32
    pub borrower: Pubkey,                // 32
    pub nonce: u64,                      // 8    pool.next_loan_nonce at create time
    pub bump: u8,                        // 1

    // Sides — encoded as a single byte for compactness
    pub sides: u8,                       // 1    0 = collateral A / debt B, 1 = collateral B / debt A

    // Amounts (raw token units)
    pub collateral_amount: u128,         // 16
    pub debt_principal: u128,            // 16   never increases after open
    pub borrow_index_snapshot_wad: u128, // 16   pool index for this debt side at open/last-touch
    pub last_touch_slot: u64,            // 8    slot accrual was last realized (informational)

    // Liquidation-trigger cache (recomputed on every collateral / debt change)
    // Stored as fixed-point 128-bit price in B-per-A units, WAD-scaled (1e18).
    pub trigger_price_wad: u128,         // 16
    pub trigger_direction: u8,           // 1    0 = TriggerOnFall, 1 = TriggerOnRise

    // Status
    pub status: u8,                      // 1    0 = open, 1 = closed-by-repay, 2 = liquidated
    pub _status_pad: [u8; 6],            // 6

    // Band bucket = band_id_for_trigger(trigger_price_wad). Set once at open;
    // immutable (trigger_price never changes). Cached so a swap's completeness
    // proof and off-chain band enumeration don't recompute it. (See §6.)
    pub band_id: u32,                    // 4

    // Lifecycle
    pub opened_slot: u64,                // 8
    pub closed_slot: u64,                // 8

    pub _reserved: [u8; 28],             // 28
}
```

`LEN` = 8 + 32×2 + 8 + 1 + 1 + 16×3 + 8 + 16 + 1 + (1 + 6) + 4 + 8×2 + 28
= 8 + 64 + 8 + 1 + 1 + 48 + 8 + 16 + 1 + 7 + 4 + 16 + 28 = **210 bytes** (verified by
`state::tests::loan_size`).

There is **no separate per-loan index account**. A loan's membership in its band
is established entirely by its cached `band_id` + `trigger_direction`; the band
account (§5.3) only stores a count. (The earlier `LoanLink` doubly-linked-list
node was removed — see §6.)

### 5.3 `LoanIndexBand`

One per `(pool, direction, band_id)` tuple that holds at least one open loan.
Stores **only a membership count** — not a list of members. PDA:
`["band", pool, direction_byte, band_id_le_bytes]`.

```rust
pub struct LoanIndexBand {
    pub discriminator: [u8; 8],

    pub pool: Pubkey,                    // 32
    pub band_id: u32,                    // 4
    pub direction: u8,                   // 1
    pub bump: u8,                        // 1
    pub _pad: [u8; 2],                   // 2

    pub count: u32,                      // 4    # of open loans in this band
    pub _pad2: [u8; 4],                  // 4

    pub _reserved: [u8; 32],             // 32
}
```

`LEN` = 8 + 32 + 4 + 1 + 1 + 2 + 4 + 4 + 32 = **88 bytes**.

`count` is maintained by the program: `+1` on `OpenLoan`, `-1` on `RepayLoan`
and on each in-swap liquidation. A loan never changes band (its trigger price is
immutable), so `count` is an exact, drift-free tally of the band's membership —
which is all the completeness proof in §6 needs. When `count` reaches 0 the
band's bit in the Pool bitmap is cleared; `RepayLoan` additionally closes the
PDA and refunds its rent, while a swap that empties a band leaves the PDA
allocated (`count = 0`). The bitmap — not PDA existence — is therefore the
source of truth for "populated", and `OpenLoan` sets the bit whenever `count`
goes 0 → 1 (covering both a fresh PDA and a swap-emptied one being reused).

---

## 6. Loan-ordering index — the hard problem

### 6.1 Constraints

- A Solana instruction has a fixed `accounts: &[AccountInfo]` — every account it
  touches must be declared by the caller before execution begins. The program
  cannot follow a pointer to an account not in the list.
- Compute budget per tx is bounded (~200k CU default, 1.4M max). A swap that has
  to walk N loans pays per loan: account read + borsh deserialize + math +
  account write.
- Tx size limit (~1232 B, ~64 accounts in v0 even with ALTs realistically) bounds
  how many loans a single swap can liquidate.

### 6.2 Strategy — two structures, no linked list

The index is **bands + a Pool bitmap**. There is no per-loan link node and no
intra-band ordering.

- **Bands** partition price space into deterministic log2 buckets:
  `band_id = floor(log2(trigger_price_wad)) − floor(log2(WAD)) + offset`
  (see `band_id_for_trigger` in `math.rs`). Each populated `(pool, direction, band_id)` has a
  `LoanIndexBand` PDA storing only a membership `count` (§5.3). Band membership
  of a loan is a pure function of its (immutable) trigger price, so it never
  needs maintenance.
- **Pool bitmap** (`band_bitmap_fall` / `band_bitmap_rise`, 128 bits each): bit
  `i` is set iff band `i` has `count > 0`. This is the on-chain source of truth
  for "which bands are populated".

Off-chain (caller / router):
1. Read pool current price; simulate the swap to get a provisional `post_price`.
2. From the bitmap, list the populated bands the price move crosses for the
   relevant direction.
3. For each such band, enumerate its open loans via `getProgramAccounts`
   (filter: program id, `Loan` discriminator, `pool`, `band_id`, `direction`),
   and sort them ascending by pubkey.
4. Pass to the program: `(Pool, vaults, user accounts, [Band, Loan×k]…)` plus a
   `band_boundary` asserting how far the price moves.

On-chain — the **set-membership completeness proof** (see §6.3).

### 6.3 Completeness verification (the subtle part)

The program must reject input that **omits** a triggered loan that should have
fired. The check (see `instructions/swap.rs`) has three layers:

1. **Per-band wholeness by count.** For every band the caller supplies, the
   supplied loan count must equal `band.count`, the loans must be sorted
   *strictly* ascending by pubkey (⇒ distinct), and each must be open with a
   cached `band_id` and `trigger_direction` matching the band. `k` distinct
   members of a band whose true size is `band.count` are — by pigeonhole —
   exactly the band. No band may be supplied twice, so supplied loans are
   globally distinct. This needs no ordering state on-chain: the only trusted
   datum is the integer `count`, which the program maintains and which cannot
   drift (a loan never changes band).

2. **Bitmap coverage.** The caller passes a `band_boundary: u32` asserting how
   far the price moves: for a falling price (`a_to_b`, OnFall) the post-swap
   band id is `≥ band_boundary`; for a rising price (`b_to_a`, OnRise) it is
   `≤ band_boundary`. The program walks the pool's `band_bitmap_{fall,rise}`
   over the implied id range (`[band_boundary, MAX]` or `[0, band_boundary]`)
   and requires **every set bit** in that range to correspond to a supplied
   band. So a caller cannot silently drop a populated band on the path.

3. **Post-cascade boundary recheck.** After the liquidation loop settles and
   the final swap is quoted, the program recomputes the true post-swap price's
   band id and reverts (`IncompleteBandWalk`) if the cascade pushed the price
   *past* the caller's claimed `band_boundary` — i.e. if more bands could have
   triggered than were proven complete in step 2.

This replaces an earlier doubly-linked-list design (`LoanLink` nodes with
`prev`/`next` pointers, chain-walk verification, and a "sentinel link" stop
condition). Once completeness became all-or-nothing per band, the ordering
earned nothing; the set-membership proof above is both smaller and easier to
audit (no pointer state to corrupt — see commit history / §6.4).

### 6.4 Why no linked list

A linked list would let a swap supply a *prefix* of a band. But completeness is
already all-or-nothing per band (you must supply every loan in any band you
touch), so a prefix is never valid — the ordering bought nothing. Its only
residual job, "prove the supplied set is exactly the band with no duplicates,"
is handled by *count == k* + *strictly ascending pubkeys* + *no band supplied
twice*. Dropping it removed an entire account type (`LoanLink`) and all the
prev/next rewiring on open, repay, and liquidation.

### 6.5 Bounded liquidation per swap

- Hard cap: `MAX_LIQ_PER_SWAP` (start at 8, tune from CU measurements).
- If more loans would trigger than the cap allows, the swap reverts with
  `TooManyLiquidationsRequired`.
- Caller's recourse: split the swap, or wait for an arbitrage-driven correction
  to clear earlier loans. This is part of the "inventory stress, not default"
  failure mode.

### 6.6 Band sizing

**As implemented:** log base 2 bands (each band spans a 2× price range), a
fixed set addressed by the 128-bit Pool bitmap (`MAX_BAND_ID = 127`). Sparse —
a `LoanIndexBand` PDA exists only while a band has loans. Per-band membership is
capped: `LoanIndexBand::MAX_LOANS = 64`, and `OpenLoan` reverts with `BandFull`
once a band's 2× bucket is saturated. The cap bounds a swap's supplied account
list (a crossed band's full membership must be handed over).

**`RebalanceBands` is retired — it will not be implemented.** The original
subdivision idea predates the pivot to the bitmap index, which made band
membership a *globally-deterministic* function of price
(`band_id = floor(log2(trigger_price)) + offset`). A single band cannot be
subdivided at runtime: the bitmap and `band_id_for_trigger` must agree across
the whole pool, so finer granularity would have to change the band function for
*every* band at once — a migration, not an in-place instruction. Subdivision
also wouldn't raise swap-time capacity: `MAX_LIQ_PER_SWAP = 8` already bounds
how many loans one swap can liquidate, so a dense price cluster needs multiple
swaps regardless of how it's bucketed. `BandFull` therefore stands as the
intended guard — it only limits *opening* a 65th loan whose trigger falls in an
already-saturated 2× price bucket (an extreme concentration). If more
open-loan headroom is ever genuinely needed, the coherent fix is a global
finer-granularity band scheme (the bitmap already has 128 slots), introduced as
a versioned migration.

---

## 7. Swap algorithm — account access pattern

A `Swap` instruction takes a fixed prefix, then a liquidation context of
`(Band, Loan×k)` groups — one group per `band_loan_counts[i]`. No collateral
token accounts are needed: collateral is already in the vault, so liquidation is
pure accounting.

```
Fixed prefix:
0.   [writable]  Pool
1.   [writable]  Vault A
2.   [writable]  Vault B
3.   [writable]  User token account A
4.   [writable]  User token account B
5.   []          Mint A
6.   []          Mint B
7.   [signer]    User
8.   []          Token program

Per band (repeated for each entry in band_loan_counts):
     [writable]  Band PDA
     [writable]  Loan × k   (all of the band's open loans, sorted strictly
                             ascending by pubkey)
```

Algorithm:

```
1. Load Pool, Vault A, Vault B; compute (real_a, real_b); bump borrow indexes.
2. Compute (accounted_a, accounted_b) via Pool::accounted (collateral excluded,
   outstanding debt included).
3. Determine direction (a→b lowers price → OnFall set; b→a raises it → OnRise).
4. Parse the tail: per band, verify the set-membership completeness proof
   (§6.3.1) — count == k, strictly ascending distinct loans, matching band_id +
   direction, no band supplied twice. Build the in-memory loan list.
5. Verify bitmap coverage over [band_boundary, MAX] / [0, band_boundary] (§6.3.2).
6. Iterate: quote on current accounted reserves → provisional post_price → find
   the next supplied loan whose trigger has crossed → liquidate it (accounting
   only: total_debt_x -= principal; total_collateral_y -= collateral) →
   recompute accounted reserves. Stop when none remain or the cap is hit.
7. Compute final swap output against the post-liquidation accounted reserves.
8. Apply swap fee; accrue protocol_fee skim.
9. Check: output ≤ swappable reserve (the solvency cap); check min_out.
10. Post-cascade boundary recheck (§6.3.3).
11. Tombstone liquidated loans (status=LIQUIDATED, amounts zeroed); decrement
    each touched band's count; clear the bitmap bit for any band that emptied.
12. Transfer input from user → vault; transfer output from vault → user.
13. Persist Pool. Emit LoanLiquidated per loan + SwapExecuted.
```

Failure modes:
- Account mismatch / chain inconsistency → `InvalidLiquidationContext`.
- Slippage exceeded → `SlippageExceeded` (whole tx reverts).
- Liquidation cap hit → `TooManyLiquidationsRequired`.
- Output > real reserve after liquidations → `Insolvent` (should not happen
  if liquidation logic is correct; sanity check).

---

## 8. Math (rough sketch — to be filled in `math.rs`)

- WAD = `1e18`, fixed-point u128 throughout, U256 for intermediate products
  (same `uint::U256` pattern as chiefstaker).
- `quote_out(amount_in, reserve_in, reserve_out, fee_bps) → amount_out`
  — standard `xy=k` with fee.
- `recompute_trigger(loan) → (trigger_price_wad, direction)` — closed form per
  §3 table.
- `next_band_in_direction(current_band, direction)` — `+1` or `-1`.
- Interest accrual (**implemented as a per-side index model**, not the flat
  accumulator originally sketched here):
  - Utilization per side: `util = total_debt_x / accounted_x`, WAD-scaled
    (`utilization_wad`).
  - Borrow rate: a two-slope **utilization-kink** curve
    (`compute_borrow_rate_wad_per_year`):
    - `util ≤ kink`: `rate = base + slope1 · util / kink`
    - `util > kink`: `rate = base + slope1 + slope2 · (util − kink) / (1 − kink)`
  - Each side carries a monotone `borrow_index_x_wad` (starts at WAD). On any
    pool touch (add/remove liquidity, open/repay loan, swap, settings update),
    `Pool::bump_indexes` advances both indexes by
    `index ·= 1 + rate_per_slot · Δslots` (`bump_index_wad`, linear within a
    bump — a slight under-estimate of `e^{rt}` over long idle windows).
  - A loan stores `borrow_index_snapshot_wad` at open; the amount owed is
    `debt_principal · current_index / snapshot` (`owed_from_index`). On repay,
    accrued interest (owed − principal) stays in the vault as LP yield; on
    liquidation it is forfeited (the principal is written off, not paid).
  - Indexes are always bumped at the rate in effect *before* a parameter change
    (`UpdatePoolSettings` bumps first), so retuning the curve is prospective.

---

## 9. Open questions / next decisions

1. **CPMM vs. concentrated** — sticking with CPMM for v1. Concentrated would
   change reserve math meaningfully; revisit after v1 ships.
2. **Interest model** — ✅ resolved. Implemented as a per-side
   utilization-kink curve with monotone borrow indexes (§8), retunable via
   `UpdatePoolSettings`. Linear-within-bump accrual; compounding refinement
   deferred.
3. **Oracle** — no external oracle in v1. Trigger prices are denominated in the
   pool's own price (B-per-A). This means the *only* signal driving liquidation
   is real swap activity. That's the design intent (§ project spec) but worth
   double-checking against attack scenarios (is there an arbitrage vector that
   lets you set up a loan that's instantly underwater but no one swaps to
   trigger it? Probably not, since it'd be opened against the live pool price,
   but worth a note).
4. **Band scheme** — ✅ resolved. log2 buckets + bitmap index, shipped;
   `RebalanceBands` is retired (§6.6). The benchmark is now in place
   (`tests/typescript/test_benchmark.ts`): the worst-case cascade — 8
   liquidations across 8 *distinct* bands (max band-PDA loads) — measures
   ~129k CU, under 10% of the 1.4M per-tx ceiling, at ~9.6k CU marginal per
   liquidation. **CU is not what bounds `MAX_LIQ_PER_SWAP = 8`** — by compute
   alone the budget would fit ~140 liquidations. The binding constraint is the
   **1232-byte legacy transaction**: every liquidated loan + its band PDA is a
   32-byte account key, so the account list fills the transaction at roughly
   8–20 liquidations (worst-case distinct-band spread vs. a single dense band).
   The cap of 8 is sized to the safe worst-case spread and the benchmark
   asserts the cap-depth swap serializes under 1232 bytes. Raising the cap
   would require migrating the client/router to v0 transactions + address
   lookup tables (256-account ceiling), not a bigger compute budget — deferred
   until a concrete >8-at-one-price clustering scenario justifies it.
5. **Multi-hop / Jupiter integration** — completely deferred. Routers will need
   a "preview liquidation context" RPC; design when we get there.
6. **Borrower nonce** — using a per-pool monotonic `next_loan_nonce` keeps loan
   PDAs unique even if a borrower opens & closes repeatedly. Closed loan
   accounts can be `lamport-zeroed` and reused via realloc, or kept as history.
   Lean toward closing them (refund rent) and incrementing the pool nonce.
7. **Authority renounce** — same model as chiefstaker (`Pubkey::default()`
   means renounced); only the swap-fee/liq-config setters are gated by it.

---

## 10. Implementation status (by file)

| File | Status | Notes |
|------|--------|-------|
| `state.rs` | ✅ | Accounts §5; `Pool` carries the §6 band bitmaps + §8 indexes |
| `math.rs` | ✅ | CPMM quoting §8, trigger derivation §3, utilization-kink interest §8 |
| `events.rs` | ✅ | Structured `sol_log_data` events (§11) |
| `error.rs` | ✅ | |
| `instructions/initialize_pool.rs` | ✅ | Validates Token-2022 extensions, creates vaults + LP mint |
| `instructions/add_liquidity.rs` | ✅ | |
| `instructions/remove_liquidity.rs` | ✅ | Executable-reserve coverage gate |
| `instructions/open_loan.rs` | ✅ | Allocates band on first use; increments band count |
| `instructions/repay_loan.rs` | ✅ | Decrements band count; refunds Loan + empty-Band rent |
| `instructions/swap.rs` | ✅ | §7 + in-flight liquidation cascade |
| `instructions/claim_protocol_fees.rs` | ✅ | Authority-only treasury drain |
| `instructions/transfer_authority.rs` | ✅ | Rotate / renounce |
| `instructions/claim_liquidated_rent.rs` | ✅ | Borrower reclaims tombstone rent |
| `instructions/update_pool_settings.rs` | ✅ | Prospective param retune |
| `instructions/rebalance_bands.rs` | ⊘ | **Retired** — incoherent with deterministic log2 bands; see §6.6 |

Integration tests live in the separate `integration-tests/` cargo project
(`solana-program-test`), kept out of the deployable crate's lockfile so the
verifiable-build container (cargo 1.78) can parse it.

End-to-end tests live in `tests/typescript/` (same layout as
`../chiefstaker`): a ts-node suite run against a live `solana-test-validator`
over RPC, covering every instruction on both token programs plus the
off-chain router flow (§6.2 — bitmap walk + `getProgramAccounts` band
enumeration), adversarial completeness attacks, the liquidation / band caps,
and conservation checks. Three files: `test_liquidity.ts` (functional, both
token programs + the shared `Ctx` harness), `test_adversarial.ts`
(account-substitution attacks + the randomized invariant fuzzer), and
`test_benchmark.ts` (the §6.6 compute-unit benchmark — cascade depth sweep +
worst-case 8-distinct-band layout, asserting the cascade clears the 1.4M-CU
per-tx ceiling with margin). Run locally via `./scripts/run-e2e-tests.sh`; CI
runs all three on every push (`.github/workflows/verifiable-build.yml`).

---

## 11. Events

Every state-changing instruction emits one structured event (and per-liquidation
events from `Swap`) via `sol_log_data`, defined in `events.rs`. The wire format
of each `Program data:` line is:

```
discriminator (8 bytes, random sentinel, leads with 0xe_) ++ borsh(payload)
```

Off-chain consumers match on the first 8 bytes, then borsh-deserialize the
remainder into the matching struct. Emission is best-effort: a serialization
failure is swallowed so a dropped log line can never revert committed state.

| Event | Emitted by | Key fields |
|-------|-----------|------------|
| `PoolInitialized` | `InitializePool` | pool, mints, authority, full config |
| `LiquidityAdded` | `AddLiquidity` | pool, user, amount_a/b_in, lp_minted |
| `LiquidityRemoved` | `RemoveLiquidity` | pool, user, lp_burned, amount_a/b_out |
| `LoanOpened` | `OpenLoan` | pool, loan, borrower, sides, amounts, band, trigger |
| `LoanRepaid` | `RepayLoan` | pool, loan, borrower, debt_principal, total_owed |
| `LoanLiquidated` | `Swap` (per loan) | pool, loan, borrower, sides, collateral, debt, trigger |
| `SwapExecuted` | `Swap` | pool, user, a_to_b, amount_in/out, liquidations, protocol_fee |
| `ProtocolFeesClaimed` | `ClaimProtocolFees` | pool, authority, amount_a/b |
| `AuthorityTransferred` | `TransferAuthority` | pool, old_authority, new_authority |
| `LiquidatedRentClaimed` | `ClaimLiquidatedRent` | pool, loan, borrower |
| `PoolSettingsUpdated` | `UpdatePoolSettings` | pool, full config |

Discriminators are pinned and round-trip-tested in `events::tests`; they are
disjoint from account discriminators (which lead with `0xa_`–`0xd_`).
