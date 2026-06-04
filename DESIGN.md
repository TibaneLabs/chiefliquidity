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
| A          | B    | …rises above threshold (collateral A loses value relative to debt B) | `(debt_b × liq_ratio) / collateral_a` |
| B          | A    | …falls below threshold (collateral B loses value relative to debt A) | `collateral_b / (debt_a × liq_ratio)` |

Wait — re-derive the A-collateral case carefully. Liquidation fires when
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
| `Pool`             | `["pool", mint_a, mint_b]` (mints sorted)        | Per-pair config + reserve totals + index heads |
| `Vault A`          | `["vault_a", pool]`                              | SPL token A holdings (real_a) |
| `Vault B`          | `["vault_b", pool]`                              | SPL token B holdings (real_b) |
| `LpMint`           | `["lp_mint", pool]`                              | LP share mint |
| `Loan`             | `["loan", pool, borrower, nonce]`                | Per-position loan state |
| `LoanLink`         | `["loan_link", pool, loan]`                      | Sorted-list link node (see §6) |
| `LoanIndexBand`    | `["band", pool, direction, band_id]`             | Bucket head pointer + count (see §6) |
| `PoolMetadata`     | `["metadata", pool]`                             | Display name/url, optional |

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
    pub liq_penalty_bps: u16,            // 2    bonus credited to pool on liquidation
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

    // Loan-ordering index heads (see §6)
    pub head_fall: Pubkey,               // 32   Loan PDA at head of TriggerOnFall list
    pub head_rise: Pubkey,               // 32   Loan PDA at head of TriggerOnRise list
    pub band_count_fall: u32,            // 4    populated OnFall bands (debug/telemetry)
    pub band_count_rise: u32,            // 4    populated OnRise bands

    // Counters
    pub open_loans: u64,                 // 8
    pub next_loan_nonce: u64,            // 8    pool-monotonic; see §5.3
    pub last_update_slot: u64,           // 8

    // Treasury accounting
    pub protocol_fees_a: u64,            // 8    skimmed; redeemable by authority
    pub protocol_fees_b: u64,            // 8

    // Band-presence bitmaps (see §6.5). Bit i set ↔ a LoanIndexBand PDA exists
    // for (pool, direction, band_id=i) with count > 0. 16 bytes = 128 bits;
    // band ids ≥ 128 (MAX_BAND_ID = 127) are not representable.
    pub band_bitmap_fall: [u8; 16],      // 16
    pub band_bitmap_rise: [u8; 16],      // 16

    pub _reserved: [u8; 32],             // 32   forward-compat
}
```

`LEN` = 8 + 32×6 + 4 + 16×4 + (1 + 2 + 2 + 3) + (2×3 + 2) + 2×4 + (16×2 + 8)
+ (32×2 + 4×2) + 8×3 + 8×2 + 16×2 + 32
= 8 + 192 + 4 + 64 + 8 + 8 + 8 + 40 + 72 + 24 + 16 + 32 + 32 = **508 bytes**
(verified by `state::tests::pool_size` borsh roundtrip).

Notes:
- `authority` is renounceable by setting to `Pubkey::default()`, same convention as
  `StakingPool`.
- All `Pubkey` head pointers default to `Pubkey::default()` for an empty list.
- The interest model started as a flat APR (one `interest_rate_bps_per_year`
  field); it now stores the four-parameter utilization-kink curve plus the two
  per-side borrow indexes that capitalize accrued interest lazily — see §8.
- `band_bitmap_*` are the on-chain source of truth for "which bands are
  populated", letting a swap prove it supplied every band a price move could
  cross without an off-chain `getProgramAccounts` walk — see §6.5.
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

    // Lifecycle
    pub opened_slot: u64,                // 8
    pub closed_slot: u64,                // 8

    pub _reserved: [u8; 32],             // 32
}
```

`LEN` = 8 + 32×2 + 8 + 1 + 1 + 16×3 + 8 + 16 + 1 + (1 + 6) + 8×2 + 32
= 8 + 64 + 8 + 1 + 1 + 48 + 8 + 16 + 1 + 7 + 16 + 32 = **210 bytes** (verified by
`state::tests::loan_size`).

The on-chain index lives in a **separate** `LoanLink` PDA so we can rewire the
sorted list without touching the loan account (which carries the bump and is
referenced from the borrower's UI). Splitting also lets us realloc the link layout
later without breaking loan deserialization.

### 5.3 `LoanLink`

Doubly-linked list node, one per open loan. Keyed at `["loan_link", pool, loan]`.

```rust
pub struct LoanLink {
    pub discriminator: [u8; 8],

    pub pool: Pubkey,                    // 32
    pub loan: Pubkey,                    // 32   back-reference

    pub band_id: u32,                    // 4    bucket id (see §6)
    pub direction: u8,                   // 1    matches Loan.trigger_direction
    pub bump: u8,                        // 1
    pub _pad: [u8; 2],                   // 2

    pub prev: Pubkey,                    // 32   prev LoanLink in band's intra-band list (or default = head)
    pub next: Pubkey,                    // 32   next LoanLink (or default = tail)
    pub trigger_price_wad: u128,         // 16   denormalized for skip-list ordering checks

    pub _reserved: [u8; 16],             // 16
}
```

`LEN` = 8 + 32 + 32 + 4 + 1 + 1 + 2 + 32 + 32 + 16 + 16 = **176 bytes**.

`prev`/`next` point to **other LoanLink PDAs** (not Loan PDAs), so traversal only
requires the link accounts plus the loan accounts that get mutated. A pure
"read-only walk to find the next trigger" needs only links.

### 5.4 `LoanIndexBand`

One per `(pool, direction, band_id)` tuple. The pool-level `head_fall` /
`head_rise` point at the head Loan; bands let us skip whole price regions when
walking to find the next-triggered loan.

```rust
pub struct LoanIndexBand {
    pub discriminator: [u8; 8],

    pub pool: Pubkey,                    // 32
    pub band_id: u32,                    // 4
    pub direction: u8,                   // 1
    pub bump: u8,                        // 1
    pub _pad: [u8; 2],                   // 2

    pub head_link: Pubkey,               // 32   first LoanLink in band (default = empty)
    pub tail_link: Pubkey,               // 32   last LoanLink in band
    pub count: u32,                      // 4    # of links in this band
    pub _pad2: [u8; 4],                  // 4

    pub min_trigger_wad: u128,           // 16   tight bound on band contents
    pub max_trigger_wad: u128,           // 16

    pub _reserved: [u8; 32],             // 32
}
```

`LEN` = 8 + 32 + 4 + 1 + 1 + 2 + 32 + 32 + 4 + 4 + 16 + 16 + 32 = **184 bytes**.

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

### 6.2 Strategy

A two-level structure:

- **Bands** partition price space into geometric buckets of fixed log-spacing.
  e.g. `band_id = floor(log_{1.05}(trigger_price_wad / unit))`. Each band is a
  PDA storing head/tail link pointers and a `count`. Bands are cheap to enumerate
  off-chain because the key derivation is `["band", pool, direction, band_id]`.
- **Intra-band linked list** — within a band, loans form a sorted doubly-linked
  list of `LoanLink` accounts.

Off-chain (caller / router):
1. Read pool current price.
2. Simulate swap to get `post_price` (assuming no liquidations).
3. Enumerate bands between `current_price` and `post_price` for the relevant
   direction.
4. For each band, deterministically read its `LoanLink` chain (off-chain RPC walk).
5. Pass to the program: `(Pool, [bands_in_play...], [loan_links_in_play...],
   [loans_in_play...], [collateral_token_accounts_for_those_loans...])`.

On-chain:
1. Verify each band PDA matches expected `band_id`.
2. Verify each `LoanLink` belongs to a band that was supplied **and** that the
   chain `prev/next` pointers are consistent with the supplied account ordering
   (prevents the caller from skipping a triggered loan).
3. Walk loans in order, applying liquidations until the next-trigger is past
   `post_price`. Update `post_price` after each liquidation.
4. Compute final swap output against the post-liquidation accounted reserves.

### 6.3 Why this beats a single global linked list

- A simple sorted list across all loans means: to find the next triggered loan,
  the program walks from `head` until it finds one. Even if no loans are
  triggered, the caller has to supply the head loan's link, the program reads it,
  decides it's safe, done. Fine for a swap that triggers nothing.
- But for a swap that crosses many bands, the linked list forces the caller to
  supply every link from the head down to the last triggered loan, **even though
  most are not in play**. Bands let the caller jump.

### 6.4 Why we still need links inside a band

- Without intra-band links, the caller would need to supply *every* loan in a
  band even if only the first triggers — same problem as above, just smaller
  scope. The intra-band linked list keeps "I supplied a contiguous prefix of
  this band's chain" cheap to verify on-chain (each `next` pointer is checked
  against the next supplied account).

### 6.5 Completeness verification (the subtle part)

The program must reject input that **omits** a triggered loan that should have
fired. The implemented check (see `instructions/swap.rs`) has three layers:

1. **Per-band wholeness.** For every band the caller supplies, the supplied
   link count must equal `band.count`, the first supplied link must equal
   `band.head_link`, each `link[i].prev` must equal `link[i-1]`, and the last
   supplied link's `next` must be `default()`. A caller therefore cannot hand
   over a partial band — it's all-or-nothing per band.

2. **Bitmap coverage.** The caller passes a `band_boundary: u32` asserting how
   far the price moves: for a falling price (`a_to_b`, OnFall) the post-swap
   band id is `≥ band_boundary`; for a rising price (`b_to_a`, OnRise) it is
   `≤ band_boundary`. The program walks the pool's `band_bitmap_{fall,rise}`
   over the implied id range (`[band_boundary, MAX]` or `[0, band_boundary]`)
   and requires **every set bit** in that range to correspond to a supplied
   band. Because the bitmap is the on-chain source of truth for "this band has
   loans", a caller cannot silently drop a populated band on the path.

3. **Post-cascade boundary recheck.** After the liquidation loop settles and
   the final swap is quoted, the program recomputes the true post-swap price's
   band id and reverts (`IncompleteBandWalk`) if the cascade pushed the price
   *past* the caller's claimed `band_boundary` — i.e. if more bands could have
   triggered than were proven complete in step 2.

This bitmap scheme replaces the original "sentinel link" stop-condition design:
the pool carries the populated-band set directly, so the program never has to
read an extra read-only link to learn where the chain ends.

### 6.6 Bounded liquidation per swap

- Hard cap: `MAX_LIQ_PER_SWAP` (start at 8, tune from CU measurements).
- If more loans would trigger than the cap allows, the swap reverts with
  `TooManyLiquidationsRequired`.
- Caller's recourse: split the swap, or wait for an arbitrage-driven correction
  to clear earlier loans. This is part of the "inventory stress, not default"
  failure mode.

### 6.7 Open question — band sizing

Geometric base of 1.05 gives ~14 bands per 2× price range. SOL/USDC at $200
spans `[$50, $800]` in ~57 bands. That's a lot of PDAs. Alternatives:

- **Sparse bands** — only allocate band PDAs that contain at least one loan.
  Empty bands don't exist; off-chain enumeration becomes "list all band PDAs
  for this pool and filter". Costs an extra `getProgramAccounts` call.
- **Fewer, wider bands** — log base 2 → ~1 band per 2× → 7 bands SOL/USDC.
  Larger intra-band lists, more compute per liquidation walk.

Suggested default: log base 2 bands (small fixed set), intra-band linked list,
with a hard cap on intra-band count (e.g. 64) that forces band subdivision when
exceeded.

**As implemented:** `LoanIndexBand::MAX_LINKS = 64`, and `OpenLoan` reverts with
`BandFull` once a band is saturated. The `RebalanceBands` subdivision
instruction is **not yet implemented** — a saturated band currently has no
on-chain remedy beyond repaying/liquidating its loans. The bitmap (§6.5) caps
band ids at `MAX_BAND_ID = 127`. Subdivision remains the planned escape hatch
and is tracked as future work (see §9.4).

---

## 7. Swap algorithm — account access pattern

A `Swap` instruction takes:

```
0.   [writable]  Pool
1.   [writable]  Vault A
2.   [writable]  Vault B
3.   [writable]  User token account (input side)
4.   [writable]  User token account (output side)
5.   [signer]    User
6.   []          Token program
7.   []          Vault authority PDA
8..N             Liquidation context, in order:
                   [writable] Band PDA #1
                   [writable] LoanLink #1.1
                   [writable] Loan #1.1
                   [writable] Borrower's collateral SPL account #1.1
                   ...
                   [writable] Sentinel link (read past last triggered) — optional
```

Algorithm:

```
1. Load Pool, Vault A, Vault B; compute (real_a, real_b).
2. Compute (accounted_a, accounted_b) = (real_a + total_debt_a,
                                         real_b + total_debt_b).
3. Determine direction (a→b raises price, b→a lowers price).
4. Pre-quote on (accounted_a, accounted_b); compute provisional post_price.
5. For each supplied band in order:
    a. Verify band PDA, direction, completeness (§6.5).
    b. For each supplied link in band:
       i.  Verify link.next chain matches next supplied account.
       ii. Load Loan; check trigger_direction matches & trigger crosses post_price.
       iii. Apply liquidation:
              - Move collateral_amount from borrower's collateral SPL acct → vault.
              - total_debt_x -= debt; total_collateral_y -= collateral.
              - Set Loan.status = liquidated; zero amounts.
              - Unlink LoanLink (rewire prev/next; update band.count, band heads,
                pool head if needed).
       iv. Recompute (accounted_a, accounted_b), post_price.
       v.  If next supplied link's trigger no longer crosses → stop band early.
    c. Verify sentinel: first non-supplied link's trigger is past post_price,
       or chain ended.
6. Compute final swap output against the now-stable (accounted_a, accounted_b).
7. Apply swap fee; split protocol_fee.
8. Check: output_side ≤ corresponding real reserve.
9. Check: user min_out / slippage.
10. Transfer input from user → vault; transfer output from vault → user.
11. Re-derive (accounted_a, accounted_b) post-swap; persist Pool.
12. Emit SwapExecuted event with liquidation count.
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
4. **Band scheme** — recommendation in §6.7. Need a CU benchmark before
   committing.
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
| `state.rs` | ✅ | Accounts §5; `Pool` carries the §6.5 band bitmaps + §8 indexes |
| `math.rs` | ✅ | CPMM quoting §8, trigger derivation §3, utilization-kink interest §8 |
| `events.rs` | ✅ | Structured `sol_log_data` events (§11) |
| `error.rs` | ✅ | |
| `instructions/initialize_pool.rs` | ✅ | Validates Token-2022 extensions, creates vaults + LP mint |
| `instructions/add_liquidity.rs` | ✅ | |
| `instructions/remove_liquidity.rs` | ✅ | Executable-reserve coverage gate |
| `instructions/open_loan.rs` | ✅ | Also creates LoanLink, allocates/inserts into band |
| `instructions/repay_loan.rs` | ✅ | Also unlinks; refunds Loan/LoanLink/empty-Band rent |
| `instructions/swap.rs` | ✅ | §7 + in-flight liquidation cascade |
| `instructions/claim_protocol_fees.rs` | ✅ | Authority-only treasury drain |
| `instructions/transfer_authority.rs` | ✅ | Rotate / renounce |
| `instructions/claim_liquidated_rent.rs` | ✅ | Borrower reclaims tombstone rent |
| `instructions/update_pool_settings.rs` | ✅ | Prospective param retune |
| `instructions/rebalance_bands.rs` | ❌ | **Not implemented** — band subdivision; see §6.7 |

Integration tests live in the separate `integration-tests/` cargo project
(`solana-program-test`), kept out of the deployable crate's lockfile so the
verifiable-build container (cargo 1.78) can parse it.

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
