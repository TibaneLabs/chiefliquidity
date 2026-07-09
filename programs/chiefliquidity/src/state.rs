//! Account state structures — see `DESIGN.md` §4–§5.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{account_info::AccountInfo, pubkey::Pubkey};

use crate::error::LiquidityError;

// ===== PDA seed prefixes =====

pub const POOL_SEED: &[u8] = b"pool";
pub const VAULT_A_SEED: &[u8] = b"vault_a";
pub const VAULT_B_SEED: &[u8] = b"vault_b";
pub const LP_MINT_SEED: &[u8] = b"lp_mint";
pub const LOAN_SEED: &[u8] = b"loan";
pub const BAND_SEED: &[u8] = b"band";

// ===== Account discriminators (random sentinels — not Anchor-derived) =====

pub const POOL_DISCRIMINATOR: [u8; 8] = [0xa1, 0xc4, 0xe7, 0x12, 0x3b, 0x8f, 0xd5, 0x6e];
pub const LOAN_DISCRIMINATOR: [u8; 8] = [0xb2, 0x7e, 0x3c, 0xa0, 0x91, 0x4d, 0x8e, 0x55];
pub const LOAN_INDEX_BAND_DISCRIMINATOR: [u8; 8] =
    [0xd4, 0x95, 0x3a, 0x71, 0x68, 0x2e, 0xc1, 0x88];

// ===== Foreign program IDs =====

/// The original SPL Token program ID (TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA).
pub const SPL_TOKEN_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0x06, 0xdd, 0xf6, 0xe1, 0xd7, 0x65, 0xa1, 0x93,
    0xd9, 0xcb, 0xe1, 0x46, 0xce, 0xeb, 0x79, 0xac,
    0x1c, 0xb4, 0x85, 0xed, 0x5f, 0x5b, 0x37, 0x91,
    0x3a, 0x8c, 0xf5, 0x85, 0x7e, 0xff, 0x00, 0xa9,
]);

/// Returns true if `key` is one of the two SPL Token program IDs we accept.
pub fn is_valid_token_program(key: &Pubkey) -> bool {
    *key == spl_token_2022::id() || *key == SPL_TOKEN_PROGRAM_ID
}

/// Validate that `token_program` is a supported token program **and** is the
/// program that actually owns `mint`. Each pool side carries its own token
/// program (a Token-2022 mint can be paired with a legacy SPL mint like wSOL),
/// so every token CPI must target the program owning that side's mint.
pub fn validate_token_program_for_mint(
    token_program: &AccountInfo,
    mint: &AccountInfo,
) -> Result<(), LiquidityError> {
    if !is_valid_token_program(token_program.key) || token_program.key != mint.owner {
        return Err(LiquidityError::InvalidTokenProgram);
    }
    Ok(())
}

// ===== Curve kinds =====

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurveKind {
    Cpmm = 0,
}

// ===== Pool =====

/// Per-pool state. PDA: `["pool", mint_a, mint_b]` with `mint_a < mint_b`.
///
/// `accounted_x = real_x + total_debt_x` (see DESIGN.md §2). `real_x` lives
/// in the corresponding Vault SPL account; only `total_debt_x` is stored here
/// so liquidation can update it locally without re-aggregating loan balances.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct Pool {
    pub discriminator: [u8; 8],

    // Identity
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub lp_mint: Pubkey,
    pub authority: Pubkey,

    // PDA bumps
    pub pool_bump: u8,
    pub vault_a_bump: u8,
    pub vault_b_bump: u8,
    pub lp_mint_bump: u8,

    // Reserve accounting
    pub total_debt_a: u128,
    pub total_debt_b: u128,
    pub total_collateral_a: u128,
    pub total_collateral_b: u128,

    // Curve config
    pub curve_kind: u8,
    pub swap_fee_bps: u16,
    pub protocol_fee_bps: u16,
    pub _curve_pad: [u8; 3],

    // Lending config (collateral health)
    pub liq_ratio_bps: u16,
    pub max_ltv_bps: u16,
    pub _lending_pad: [u8; 2],

    // Interest model — shared rate curve, applied independently to each
    // side's utilization (see DESIGN.md §8). Per-side state below.
    pub interest_base_bps_per_year: u16,
    pub interest_slope1_bps_per_year: u16,
    pub interest_slope2_bps_per_year: u16,
    pub interest_kink_bps: u16,

    /// Monotonic borrow index for side A's debt (WAD-scaled, ≥ WAD).
    /// owed_a = principal * borrow_index_a_wad / loan.borrow_index_snapshot_wad
    pub borrow_index_a_wad: u128,
    /// Same for side B.
    pub borrow_index_b_wad: u128,
    /// Slot at which both indexes were last bumped.
    pub last_index_update_slot: u64,

    // Counters
    pub open_loans: u64,
    pub next_loan_nonce: u64,
    pub last_update_slot: u64,

    // Treasury
    pub protocol_fees_a: u64,
    pub protocol_fees_b: u64,

    /// Bitmap of populated band ids in the OnFall direction. Bit `i` is set
    /// iff a `LoanIndexBand` PDA exists for `(pool, OnFall, band_id=i)` with
    /// `count > 0`. 16 bytes = 128 bits; band ids ≥ 128 are not representable
    /// (well above any realistic price range, see DESIGN.md §6).
    pub band_bitmap_fall: [u8; 16],
    /// As above, OnRise direction.
    pub band_bitmap_rise: [u8; 16],

    pub _reserved: [u8; 32],
}

impl Pool {
    /// Size in bytes when serialized with borsh.
    pub const LEN: usize = 8                 // discriminator
        + 32 * 6                              // mint_a, mint_b, vault_a, vault_b, lp_mint, authority
        + 4                                   // 4× bump
        + 16 * 4                              // 4× u128 debt/collateral totals
        + 1 + 2 + 2 + 3                       // curve_kind, swap_fee_bps, protocol_fee_bps, _curve_pad
        + 2 * 2 + 2                           // liq_ratio_bps, max_ltv_bps + _lending_pad
        + 2 * 4                               // 4× u16 interest model params
        + 16 * 2 + 8                          // borrow_index_a/b_wad + last_index_update_slot
        + 8 * 3                               // open_loans, next_loan_nonce, last_update_slot
        + 8 * 2                               // protocol_fees_a, protocol_fees_b
        + 16 * 2                              // band_bitmap_fall, band_bitmap_rise
        + 32;                                 // _reserved

    pub fn is_initialized(&self) -> bool {
        self.discriminator == POOL_DISCRIMINATOR
    }

    pub fn is_authority_renounced(&self) -> bool {
        self.authority == Pubkey::default()
    }

    /// Derive the pool PDA. Caller must pass mints already sorted (mint_a <
    /// mint_b lexicographically).
    pub fn derive_pda(
        mint_a: &Pubkey,
        mint_b: &Pubkey,
        program_id: &Pubkey,
    ) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[POOL_SEED, mint_a.as_ref(), mint_b.as_ref()],
            program_id,
        )
    }

    pub fn derive_vault_a_pda(pool: &Pubkey, program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[VAULT_A_SEED, pool.as_ref()], program_id)
    }

    pub fn derive_vault_b_pda(pool: &Pubkey, program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[VAULT_B_SEED, pool.as_ref()], program_id)
    }

    pub fn derive_lp_mint_pda(pool: &Pubkey, program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[LP_MINT_SEED, pool.as_ref()], program_id)
    }

    /// LP-owned share of each side, given current vault balances.
    /// Excludes borrower-deposited collateral (DESIGN.md §2) and the
    /// protocol's accumulated fee share.
    ///
    /// `swappable_x = real_x - total_collateral_x - protocol_fees_x`
    pub fn swappable(
        &self,
        real_a: u128,
        real_b: u128,
    ) -> Result<(u128, u128), LiquidityError> {
        let a = real_a
            .checked_sub(self.total_collateral_a)
            .ok_or(LiquidityError::MathUnderflow)?
            .checked_sub(self.protocol_fees_a as u128)
            .ok_or(LiquidityError::MathUnderflow)?;
        let b = real_b
            .checked_sub(self.total_collateral_b)
            .ok_or(LiquidityError::MathUnderflow)?
            .checked_sub(self.protocol_fees_b as u128)
            .ok_or(LiquidityError::MathUnderflow)?;
        Ok((a, b))
    }

    /// Accounted reserves (used for AMM pricing and LP share math).
    /// Borrowing doesn't move pricing; collateral isn't part of LP claim.
    ///
    /// `accounted_x = (real_x - total_collateral_x) + total_debt_x`
    pub fn accounted(
        &self,
        real_a: u128,
        real_b: u128,
    ) -> Result<(u128, u128), LiquidityError> {
        let (s_a, s_b) = self.swappable(real_a, real_b)?;
        let a = s_a
            .checked_add(self.total_debt_a)
            .ok_or(LiquidityError::MathOverflow)?;
        let b = s_b
            .checked_add(self.total_debt_b)
            .ok_or(LiquidityError::MathOverflow)?;
        Ok((a, b))
    }

    /// Mutable reference to the bitmap for the given trigger direction.
    pub fn band_bitmap_mut(&mut self, direction_byte: u8) -> Result<&mut [u8; 16], LiquidityError> {
        match direction_byte {
            0 => Ok(&mut self.band_bitmap_fall),
            1 => Ok(&mut self.band_bitmap_rise),
            _ => Err(LiquidityError::InvalidSidesEncoding),
        }
    }

    /// Read-only reference to the bitmap for the given trigger direction.
    pub fn band_bitmap(&self, direction_byte: u8) -> Result<&[u8; 16], LiquidityError> {
        match direction_byte {
            0 => Ok(&self.band_bitmap_fall),
            1 => Ok(&self.band_bitmap_rise),
            _ => Err(LiquidityError::InvalidSidesEncoding),
        }
    }

    /// Bump both per-side borrow indexes to `current_slot` using the rate
    /// curve evaluated at the current per-side utilization. Idempotent
    /// when `current_slot == last_index_update_slot`.
    ///
    /// MUST be called at the start of every instruction that reads or
    /// writes `total_debt_x` or that reads a per-loan owed amount; the
    /// indexes carry the LP's claim on accrued (but unrealized) interest.
    pub fn bump_indexes(
        &mut self,
        real_a: u128,
        real_b: u128,
        current_slot: u64,
    ) -> Result<(), LiquidityError> {
        let slots_elapsed = current_slot.saturating_sub(self.last_index_update_slot);
        if slots_elapsed == 0 {
            return Ok(());
        }
        let (acc_a, acc_b) = self.accounted(real_a, real_b)?;

        let util_a = crate::math::utilization_wad(self.total_debt_a, acc_a);
        let util_b = crate::math::utilization_wad(self.total_debt_b, acc_b);

        let rate_a = crate::math::compute_borrow_rate_wad_per_year(
            util_a,
            self.interest_base_bps_per_year,
            self.interest_slope1_bps_per_year,
            self.interest_slope2_bps_per_year,
            self.interest_kink_bps,
        )?;
        let rate_b = crate::math::compute_borrow_rate_wad_per_year(
            util_b,
            self.interest_base_bps_per_year,
            self.interest_slope1_bps_per_year,
            self.interest_slope2_bps_per_year,
            self.interest_kink_bps,
        )?;

        self.borrow_index_a_wad = crate::math::bump_index_wad(
            self.borrow_index_a_wad,
            rate_a,
            slots_elapsed,
        )?;
        self.borrow_index_b_wad = crate::math::bump_index_wad(
            self.borrow_index_b_wad,
            rate_b,
            slots_elapsed,
        )?;
        self.last_index_update_slot = current_slot;
        Ok(())
    }

    /// Borrow index for the side that a `LoanSides`-encoded loan owes.
    ///
    /// CollateralA → debt is B → use borrow_index_b_wad.
    /// CollateralB → debt is A → use borrow_index_a_wad.
    pub fn borrow_index_for_debt_side(&self, sides_byte: u8) -> Result<u128, LiquidityError> {
        match sides_byte {
            0 => Ok(self.borrow_index_b_wad),
            1 => Ok(self.borrow_index_a_wad),
            _ => Err(LiquidityError::InvalidSidesEncoding),
        }
    }
}

// ===== Band-presence bitmap helpers =====

/// Maximum band id supported by the Pool's bitmap (16 bytes × 8 bits = 128).
pub const MAX_BAND_ID: u32 = 127;

/// Set bit `band_id` in the bitmap. Errors if `band_id > MAX_BAND_ID`.
pub fn bitmap_set(bitmap: &mut [u8; 16], band_id: u32) -> Result<(), LiquidityError> {
    if band_id > MAX_BAND_ID {
        return Err(LiquidityError::SettingExceedsMaximum);
    }
    let byte = (band_id / 8) as usize;
    let bit = (band_id % 8) as u8;
    bitmap[byte] |= 1 << bit;
    Ok(())
}

/// Clear bit `band_id`. No-op if the bit was already clear.
pub fn bitmap_clear(bitmap: &mut [u8; 16], band_id: u32) -> Result<(), LiquidityError> {
    if band_id > MAX_BAND_ID {
        return Err(LiquidityError::SettingExceedsMaximum);
    }
    let byte = (band_id / 8) as usize;
    let bit = (band_id % 8) as u8;
    bitmap[byte] &= !(1 << bit);
    Ok(())
}

/// Returns true if bit `band_id` is set. Returns false for out-of-range ids
/// (since they cannot be in the bitmap).
pub fn bitmap_is_set(bitmap: &[u8; 16], band_id: u32) -> bool {
    if band_id > MAX_BAND_ID {
        return false;
    }
    let byte = (band_id / 8) as usize;
    let bit = (band_id % 8) as u8;
    (bitmap[byte] & (1 << bit)) != 0
}

/// Iterate the set bits in `bitmap` whose ids fall in `[lo, hi]` inclusive.
/// `hi` is clamped to `MAX_BAND_ID`. Closes the iterator when `lo > hi`.
pub fn bitmap_iter_set_range(
    bitmap: &[u8; 16],
    lo: u32,
    hi: u32,
) -> impl Iterator<Item = u32> + '_ {
    let hi = hi.min(MAX_BAND_ID);
    (lo..=hi).filter(move |&id| bitmap_is_set(bitmap, id))
}

// ===== Loan =====

/// Per-position loan state. PDA: `["loan", pool, borrower, nonce_le_bytes]`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct Loan {
    pub discriminator: [u8; 8],

    pub pool: Pubkey,
    pub borrower: Pubkey,
    pub nonce: u64,
    pub bump: u8,

    /// `LoanSides` (CollateralA = 0, CollateralB = 1).
    pub sides: u8,

    pub collateral_amount: u128,
    pub debt_principal: u128,
    /// Pool's borrow index for this loan's debt side at the moment of open
    /// (or last touch). Owed = `principal * pool.current_index / snapshot`.
    pub borrow_index_snapshot_wad: u128,
    /// Slot at which this loan last had its accrual realized (informational).
    pub last_touch_slot: u64,

    /// B-per-A trigger price, WAD-scaled.
    pub trigger_price_wad: u128,
    /// `TriggerDirection` (OnFall = 0, OnRise = 1).
    pub trigger_direction: u8,

    /// 0 = open, 1 = closed-by-repay, 2 = liquidated.
    pub status: u8,
    pub _status_pad: [u8; 6],

    /// Band bucket = `band_id_for_trigger(trigger_price_wad)`. Set once at open
    /// (immutable, since `trigger_price_wad` never changes). Cached here so a
    /// swap's completeness proof and off-chain band enumeration don't recompute.
    pub band_id: u32,

    pub opened_slot: u64,
    pub closed_slot: u64,

    pub _reserved: [u8; 28],
}

impl Loan {
    pub const LEN: usize = 8                 // discriminator
        + 32 * 2                              // pool, borrower
        + 8                                   // nonce
        + 1                                   // bump
        + 1                                   // sides
        + 16 * 3                              // collateral, principal, borrow_index_snapshot
        + 8                                   // last_touch_slot
        + 16                                  // trigger_price_wad
        + 1                                   // trigger_direction
        + 1 + 6                               // status + _status_pad
        + 4                                   // band_id
        + 8 * 2                               // opened_slot, closed_slot
        + 28;                                 // _reserved

    pub const STATUS_OPEN: u8 = 0;
    pub const STATUS_REPAID: u8 = 1;
    pub const STATUS_LIQUIDATED: u8 = 2;

    pub fn is_initialized(&self) -> bool {
        self.discriminator == LOAN_DISCRIMINATOR
    }

    pub fn is_open(&self) -> bool {
        self.status == Self::STATUS_OPEN
    }

    /// Compute current owed (principal + interest) given the pool's
    /// current borrow index for this loan's debt side.
    pub fn owed(&self, pool_current_index_wad: u128) -> Result<u128, LiquidityError> {
        crate::math::owed_from_index(
            self.debt_principal,
            self.borrow_index_snapshot_wad,
            pool_current_index_wad,
        )
    }

    pub fn derive_pda(
        pool: &Pubkey,
        borrower: &Pubkey,
        nonce: u64,
        program_id: &Pubkey,
    ) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[LOAN_SEED, pool.as_ref(), borrower.as_ref(), &nonce.to_le_bytes()],
            program_id,
        )
    }
}

// ===== LoanIndexBand =====

/// Membership counter for one (pool, direction, band_id) bucket. PDA:
/// `["band", pool, direction_byte, band_id_le_bytes]`.
///
/// The band stores only how many open loans fall in its 2× price bucket — not
/// *which* ones. A swap proves it supplied a band's full membership by checking
/// it was handed exactly `count` distinct open loans whose cached `band_id` and
/// `direction` match (see `DESIGN.md` §6). The Pool bitmap (`band_bitmap_*`)
/// tracks which bands have `count > 0`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct LoanIndexBand {
    pub discriminator: [u8; 8],

    pub pool: Pubkey,
    pub band_id: u32,
    pub direction: u8,
    pub bump: u8,
    pub _pad: [u8; 2],

    pub count: u32,
    pub _pad2: [u8; 4],

    pub _reserved: [u8; 32],
}

impl LoanIndexBand {
    pub const LEN: usize = 8                 // discriminator
        + 32                                  // pool
        + 4 + 1 + 1 + 2                       // band_id, direction, bump, _pad
        + 4 + 4                               // count, _pad2
        + 32;                                 // _reserved

    /// Hard cap on open loans per band. `OpenLoan` reverts with `BandFull` once
    /// a band's 2× price bucket holds this many loans; the bound keeps a swap's
    /// supplied account list (which must include a crossed band's full
    /// membership) within tx limits. See `DESIGN.md` §6.6.
    pub const MAX_LOANS: u32 = 64;

    pub fn is_initialized(&self) -> bool {
        self.discriminator == LOAN_INDEX_BAND_DISCRIMINATOR
    }

    pub fn derive_pda(
        pool: &Pubkey,
        direction: u8,
        band_id: u32,
        program_id: &Pubkey,
    ) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[
                BAND_SEED,
                pool.as_ref(),
                &[direction],
                &band_id.to_le_bytes(),
            ],
            program_id,
        )
    }
}

// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_pool() -> Pool {
        Pool {
            discriminator: POOL_DISCRIMINATOR,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
            vault_a: Pubkey::new_unique(),
            vault_b: Pubkey::new_unique(),
            lp_mint: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            pool_bump: 255,
            vault_a_bump: 254,
            vault_b_bump: 253,
            lp_mint_bump: 252,
            total_debt_a: 0,
            total_debt_b: 0,
            total_collateral_a: 0,
            total_collateral_b: 0,
            curve_kind: CurveKind::Cpmm as u8,
            swap_fee_bps: 30,
            protocol_fee_bps: 5,
            _curve_pad: [0; 3],
            liq_ratio_bps: 11000,
            max_ltv_bps: 8000,
            _lending_pad: [0; 2],
            interest_base_bps_per_year: 0,
            interest_slope1_bps_per_year: 400,
            interest_slope2_bps_per_year: 30_000,
            interest_kink_bps: 8000,
            borrow_index_a_wad: crate::math::WAD,
            borrow_index_b_wad: crate::math::WAD,
            last_index_update_slot: 0,
            open_loans: 0,
            next_loan_nonce: 0,
            last_update_slot: 0,
            protocol_fees_a: 0,
            protocol_fees_b: 0,
            band_bitmap_fall: [0; 16],
            band_bitmap_rise: [0; 16],
            _reserved: [0; 32],
        }
    }

    fn fake_loan() -> Loan {
        Loan {
            discriminator: LOAN_DISCRIMINATOR,
            pool: Pubkey::new_unique(),
            borrower: Pubkey::new_unique(),
            nonce: 1,
            bump: 255,
            sides: 0,
            collateral_amount: 50,
            debt_principal: 100,
            borrow_index_snapshot_wad: crate::math::WAD,
            last_touch_slot: 0,
            trigger_price_wad: 2_200_000_000_000_000_000,
            trigger_direction: 0,
            status: Loan::STATUS_OPEN,
            _status_pad: [0; 6],
            band_id: 7,
            opened_slot: 0,
            closed_slot: 0,
            _reserved: [0; 28],
        }
    }

    fn fake_band() -> LoanIndexBand {
        LoanIndexBand {
            discriminator: LOAN_INDEX_BAND_DISCRIMINATOR,
            pool: Pubkey::new_unique(),
            band_id: 7,
            direction: 0,
            bump: 255,
            _pad: [0; 2],
            count: 0,
            _pad2: [0; 4],
            _reserved: [0; 32],
        }
    }

    #[test]
    fn pool_size() {
        let p = fake_pool();
        let v = borsh::to_vec(&p).unwrap();
        assert_eq!(v.len(), Pool::LEN);
    }

    #[test]
    fn loan_size() {
        let l = fake_loan();
        let v = borsh::to_vec(&l).unwrap();
        assert_eq!(v.len(), Loan::LEN);
    }

    #[test]
    fn band_size() {
        let b = fake_band();
        let v = borsh::to_vec(&b).unwrap();
        assert_eq!(v.len(), LoanIndexBand::LEN);
    }

    #[test]
    fn pool_roundtrip() {
        let p = fake_pool();
        let v = borsh::to_vec(&p).unwrap();
        let p2 = Pool::try_from_slice(&v).unwrap();
        assert_eq!(p2.swap_fee_bps, 30);
        assert_eq!(p2.liq_ratio_bps, 11000);
        assert!(p2.is_initialized());
        assert!(!p2.is_authority_renounced());
    }

    #[test]
    fn loan_roundtrip() {
        let l = fake_loan();
        let v = borsh::to_vec(&l).unwrap();
        let l2 = Loan::try_from_slice(&v).unwrap();
        assert_eq!(l2.collateral_amount, 50);
        assert_eq!(l2.debt_principal, 100);
        assert_eq!(l2.band_id, 7);
        assert!(l2.is_open());
    }

    #[test]
    fn band_roundtrip() {
        let b = fake_band();
        let v = borsh::to_vec(&b).unwrap();
        let b2 = LoanIndexBand::try_from_slice(&v).unwrap();
        assert_eq!(b2.band_id, 7);
        assert_eq!(b2.count, 0);
    }

    #[test]
    fn spl_token_program_id_constant() {
        let expected: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            .parse()
            .unwrap();
        assert_eq!(SPL_TOKEN_PROGRAM_ID, expected);
    }

    #[test]
    fn is_valid_token_program_accepts_both() {
        assert!(is_valid_token_program(&SPL_TOKEN_PROGRAM_ID));
        assert!(is_valid_token_program(&spl_token_2022::id()));
        assert!(!is_valid_token_program(&Pubkey::default()));
    }

    #[test]
    fn pool_pda_is_canonical() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        let (a, _) = Pool::derive_pda(&mint_a, &mint_b, &prog);
        let (b, _) = Pool::derive_pda(&mint_a, &mint_b, &prog);
        assert_eq!(a, b);
        // Different ordering produces different PDA — caller must sort.
        let (c, _) = Pool::derive_pda(&mint_b, &mint_a, &prog);
        if mint_a != mint_b {
            assert_ne!(a, c);
        }
    }

    #[test]
    fn band_pda_distinct_per_direction() {
        let pool = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        let (a, _) = LoanIndexBand::derive_pda(&pool, 0, 5, &prog);
        let (b, _) = LoanIndexBand::derive_pda(&pool, 1, 5, &prog);
        let (c, _) = LoanIndexBand::derive_pda(&pool, 0, 6, &prog);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn pool_accounted_excludes_collateral() {
        // Bare pool: accounted == real
        let mut p = fake_pool();
        let (a, b) = p.accounted(1000, 5000).unwrap();
        assert_eq!((a, b), (1000, 5000));

        // Open a loan: collateral 50 A, debt 100 B.
        // After open_loan, real_a = 1050, real_b = 4900, total_coll_a = 50, total_debt_b = 100.
        p.total_collateral_a = 50;
        p.total_debt_b = 100;
        let (a, b) = p.accounted(1050, 4900).unwrap();
        // LP claim should be unchanged from the pre-loan state.
        assert_eq!((a, b), (1000, 5000));

        // After liquidation: collateral seized (total_coll_a -= 50), debt
        // forgiven (total_debt_b -= 100). Real balances unchanged.
        p.total_collateral_a = 0;
        p.total_debt_b = 0;
        let (a, b) = p.accounted(1050, 4900).unwrap();
        assert_eq!((a, b), (1050, 4900)); // LP gained 50 A, lost 100 B
    }

    #[test]
    fn bitmap_set_clear_is_set() {
        let mut bm = [0u8; 16];
        assert!(!bitmap_is_set(&bm, 0));
        bitmap_set(&mut bm, 0).unwrap();
        assert!(bitmap_is_set(&bm, 0));
        assert_eq!(bm[0], 0x01);

        bitmap_set(&mut bm, 7).unwrap();
        assert_eq!(bm[0], 0x81);
        bitmap_set(&mut bm, 8).unwrap();
        assert_eq!(bm[1], 0x01);

        bitmap_clear(&mut bm, 7).unwrap();
        assert_eq!(bm[0], 0x01);
        assert!(!bitmap_is_set(&bm, 7));

        // Boundary: highest valid id
        bitmap_set(&mut bm, MAX_BAND_ID).unwrap();
        assert!(bitmap_is_set(&bm, MAX_BAND_ID));
        assert_eq!(bm[15], 0x80);

        // Out of range
        assert!(bitmap_set(&mut bm, MAX_BAND_ID + 1).is_err());
        assert!(!bitmap_is_set(&bm, MAX_BAND_ID + 1));
    }

    #[test]
    fn bitmap_iter_in_range() {
        let mut bm = [0u8; 16];
        bitmap_set(&mut bm, 5).unwrap();
        bitmap_set(&mut bm, 64).unwrap();
        bitmap_set(&mut bm, 65).unwrap();
        bitmap_set(&mut bm, 100).unwrap();
        let v: Vec<u32> = bitmap_iter_set_range(&bm, 60, 70).collect();
        assert_eq!(v, vec![64, 65]);
        let all: Vec<u32> = bitmap_iter_set_range(&bm, 0, 127).collect();
        assert_eq!(all, vec![5, 64, 65, 100]);
        let empty: Vec<u32> = bitmap_iter_set_range(&bm, 6, 60).collect();
        assert!(empty.is_empty());
    }

    #[test]
    fn pool_swappable_excludes_collateral() {
        let mut p = fake_pool();
        p.total_collateral_a = 50;
        p.total_collateral_b = 0;
        let (a, b) = p.swappable(1050, 5000).unwrap();
        assert_eq!((a, b), (1000, 5000));
    }

    #[test]
    fn pool_swappable_excludes_protocol_fees() {
        let mut p = fake_pool();
        p.total_collateral_a = 0;
        p.protocol_fees_a = 25;
        p.protocol_fees_b = 100;
        let (a, b) = p.swappable(1000, 5000).unwrap();
        assert_eq!((a, b), (975, 4900));
        // accounted excludes both (collateral + protocol fees) but adds debt
        p.total_debt_b = 500;
        let (acc_a, acc_b) = p.accounted(1000, 5000).unwrap();
        assert_eq!((acc_a, acc_b), (975, 4900 + 500));
    }

    /// Confirm the LEN constants. Sizes drift as fields are added; tests
    /// guard against accidental layout breakage.
    #[test]
    fn known_sizes() {
        assert_eq!(Pool::LEN, 434);
        assert_eq!(Loan::LEN, 210);
        assert_eq!(LoanIndexBand::LEN, 88);
    }
}
