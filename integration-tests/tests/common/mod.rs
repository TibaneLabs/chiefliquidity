//! Shared test harness for `chiefliquidity` integration tests.
//!
//! Each test file lives in `tests/<name>.rs` and starts with
//! `mod common;` to pull this in. `TestEnv::new()` boots an in-process
//! `solana-program-test` bank, registers our program + spl_token_2022,
//! creates a sorted (mint_a, mint_b) pair backed by Token-2022, and
//! exposes helper methods for building / submitting instructions and
//! reading account state.

#![allow(dead_code)]

use borsh::{BorshDeserialize, BorshSerialize};
use chiefliquidity::{
    state::{bitmap_is_set, Loan, LoanIndexBand, LoanLink, Pool},
    LiquidityInstruction,
};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction,
};
use solana_program_test::{processor, BanksClient, ProgramTest, ProgramTestBanksClientExt};
use solana_sdk::{
    commitment_config::CommitmentLevel,
    hash::Hash,
    signature::{Keypair, Signer},
    transaction::Transaction,
    transport::TransportError,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id, instruction::create_associated_token_account,
};
use spl_token_2022::{
    extension::StateWithExtensions, instruction as token_ix,
    state::{Account as TokenAccount, Mint},
};

/// Default sane pool parameters used by most tests. Override per-test by
/// calling `init_pool_with_params`.
pub struct PoolParams {
    pub swap_fee_bps: u16,
    pub protocol_fee_bps: u16,
    pub liq_ratio_bps: u16,
    pub max_ltv_bps: u16,
    pub interest_base_bps_per_year: u16,
    pub interest_slope1_bps_per_year: u16,
    pub interest_slope2_bps_per_year: u16,
    pub interest_kink_bps: u16,
}

impl Default for PoolParams {
    fn default() -> Self {
        Self {
            swap_fee_bps: 30,
            protocol_fee_bps: 5,
            liq_ratio_bps: 11_000,
            max_ltv_bps: 8_000,
            interest_base_bps_per_year: 0,
            interest_slope1_bps_per_year: 400,
            interest_slope2_bps_per_year: 30_000,
            interest_kink_bps: 8_000,
        }
    }
}

pub struct TestEnv {
    pub program_id: Pubkey,
    pub banks_client: BanksClient,
    pub payer: Keypair,
    pub last_blockhash: Hash,
    /// Lexicographically smaller mint pubkey.
    pub mint_a: Keypair,
    /// Lexicographically larger mint pubkey.
    pub mint_b: Keypair,
    pub mint_a_decimals: u8,
    pub mint_b_decimals: u8,
    pub token_program: Pubkey,
}

impl TestEnv {
    /// Boot the bank, register chiefliquidity + spl_token_2022, create two
    /// fresh Token-2022 mints with `payer` as mint authority. Mints are
    /// sorted so `mint_a < mint_b`.
    pub async fn new() -> Self {
        let program_id = chiefliquidity::id();
        let mut program_test = ProgramTest::new(
            "chiefliquidity",
            program_id,
            processor!(chiefliquidity::process_instruction),
        );
        program_test.add_program(
            "spl_token_2022",
            spl_token_2022::id(),
            processor!(spl_token_2022::processor::Processor::process),
        );

        let (mut banks_client, payer, last_blockhash) = program_test.start().await;

        // Generate two mints, sort by pubkey so mint_a < mint_b.
        let mut a = Keypair::new();
        let mut b = Keypair::new();
        if a.pubkey().as_ref() > b.pubkey().as_ref() {
            std::mem::swap(&mut a, &mut b);
        }

        let token_program = spl_token_2022::id();
        let mint_a_decimals = 9;
        let mint_b_decimals = 6;

        create_mint(
            &mut banks_client,
            &payer,
            last_blockhash,
            &a,
            payer.pubkey(),
            mint_a_decimals,
            token_program,
        )
        .await;
        create_mint(
            &mut banks_client,
            &payer,
            last_blockhash,
            &b,
            payer.pubkey(),
            mint_b_decimals,
            token_program,
        )
        .await;

        Self {
            program_id,
            banks_client,
            payer,
            last_blockhash,
            mint_a: a,
            mint_b: b,
            mint_a_decimals,
            mint_b_decimals,
            token_program,
        }
    }

    // ---- PDAs ----

    pub fn pool_pda(&self) -> (Pubkey, u8) {
        Pool::derive_pda(&self.mint_a.pubkey(), &self.mint_b.pubkey(), &self.program_id)
    }
    pub fn vault_a_pda(&self) -> (Pubkey, u8) {
        Pool::derive_vault_a_pda(&self.pool_pda().0, &self.program_id)
    }
    pub fn vault_b_pda(&self) -> (Pubkey, u8) {
        Pool::derive_vault_b_pda(&self.pool_pda().0, &self.program_id)
    }
    pub fn lp_mint_pda(&self) -> (Pubkey, u8) {
        Pool::derive_lp_mint_pda(&self.pool_pda().0, &self.program_id)
    }
    pub fn loan_pda(&self, borrower: &Pubkey, nonce: u64) -> (Pubkey, u8) {
        Loan::derive_pda(&self.pool_pda().0, borrower, nonce, &self.program_id)
    }
    pub fn loan_link_pda(&self, loan: &Pubkey) -> (Pubkey, u8) {
        LoanLink::derive_pda(&self.pool_pda().0, loan, &self.program_id)
    }
    pub fn band_pda(&self, direction: u8, band_id: u32) -> (Pubkey, u8) {
        LoanIndexBand::derive_pda(&self.pool_pda().0, direction, band_id, &self.program_id)
    }

    // ---- User / token helpers ----

    pub async fn create_funded_user(&mut self, lamports: u64) -> Keypair {
        let user = Keypair::new();
        let ix =
            system_instruction::transfer(&self.payer.pubkey(), &user.pubkey(), lamports);
        self.send(&[ix], &[]).await.unwrap();
        user
    }

    pub async fn create_ata(&mut self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        let ata = get_associated_token_address_with_program_id(
            owner,
            mint,
            &self.token_program,
        );
        let ix = create_associated_token_account(
            &self.payer.pubkey(),
            owner,
            mint,
            &self.token_program,
        );
        self.send(&[ix], &[]).await.unwrap();
        ata
    }

    pub async fn mint_to(
        &mut self,
        mint: &Pubkey,
        recipient_ata: &Pubkey,
        amount: u64,
    ) {
        let ix = token_ix::mint_to(
            &self.token_program,
            mint,
            recipient_ata,
            &self.payer.pubkey(),
            &[],
            amount,
        )
        .unwrap();
        self.send(&[ix], &[]).await.unwrap();
    }

    /// Convenience: creates an ATA for `user` for the given mint, then mints
    /// `amount` tokens to it. Returns the ATA address.
    pub async fn fund_token(
        &mut self,
        user: &Pubkey,
        mint: &Pubkey,
        amount: u64,
    ) -> Pubkey {
        let ata = self.create_ata(user, mint).await;
        self.mint_to(mint, &ata, amount).await;
        ata
    }

    // ---- Instruction submission ----

    /// Send a transaction signed by `payer` plus any `extra` signers.
    pub async fn send(
        &mut self,
        ixs: &[Instruction],
        extra: &[&Keypair],
    ) -> Result<(), TransportError> {
        let mut tx = Transaction::new_with_payer(ixs, Some(&self.payer.pubkey()));
        let mut signers: Vec<&Keypair> = vec![&self.payer];
        signers.extend(extra.iter().copied());
        tx.sign(&signers, self.last_blockhash);
        self.banks_client
            .process_transaction_with_commitment(tx, CommitmentLevel::Processed)
            .await
            .map_err(Into::into)
    }

    /// Like `send`, but advances the test bank's blockhash first so two
    /// back-to-back instructions don't get rejected as duplicates.
    pub async fn send_with_new_blockhash(
        &mut self,
        ixs: &[Instruction],
        extra: &[&Keypair],
    ) -> Result<(), TransportError> {
        self.refresh_blockhash().await;
        self.send(ixs, extra).await
    }

    pub async fn refresh_blockhash(&mut self) {
        self.last_blockhash = self
            .banks_client
            .get_new_latest_blockhash(&self.last_blockhash)
            .await
            .unwrap();
    }

    // ---- Pool initialization ----

    pub fn ix_initialize_pool(&self, params: &PoolParams) -> Instruction {
        let pool = self.pool_pda().0;
        let vault_a = self.vault_a_pda().0;
        let vault_b = self.vault_b_pda().0;
        let lp_mint = self.lp_mint_pda().0;
        let data = LiquidityInstruction::InitializePool {
            swap_fee_bps: params.swap_fee_bps,
            protocol_fee_bps: params.protocol_fee_bps,
            liq_ratio_bps: params.liq_ratio_bps,
            max_ltv_bps: params.max_ltv_bps,
            interest_base_bps_per_year: params.interest_base_bps_per_year,
            interest_slope1_bps_per_year: params.interest_slope1_bps_per_year,
            interest_slope2_bps_per_year: params.interest_slope2_bps_per_year,
            interest_kink_bps: params.interest_kink_bps,
        };
        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(self.mint_a.pubkey(), false),
                AccountMeta::new_readonly(self.mint_b.pubkey(), false),
                AccountMeta::new(vault_a, false),
                AccountMeta::new(vault_b, false),
                AccountMeta::new(lp_mint, false),
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(solana_program::system_program::id(), false),
                AccountMeta::new_readonly(self.token_program, false),
                AccountMeta::new_readonly(solana_program::sysvar::rent::id(), false),
            ],
            data: borsh::to_vec(&data).unwrap(),
        }
    }

    pub async fn initialize_pool_default(&mut self) {
        let ix = self.ix_initialize_pool(&PoolParams::default());
        self.send(&[ix], &[]).await.unwrap();
    }

    // ---- Liquidity ----

    pub fn ix_add_liquidity(
        &self,
        user: &Pubkey,
        user_a_ata: &Pubkey,
        user_b_ata: &Pubkey,
        user_lp_ata: &Pubkey,
        amount_a_max: u64,
        amount_b_max: u64,
        min_lp_out: u64,
    ) -> Instruction {
        let pool = self.pool_pda().0;
        let vault_a = self.vault_a_pda().0;
        let vault_b = self.vault_b_pda().0;
        let lp_mint = self.lp_mint_pda().0;
        let data = LiquidityInstruction::AddLiquidity {
            amount_a_max,
            amount_b_max,
            min_lp_out,
        };
        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(vault_a, false),
                AccountMeta::new(vault_b, false),
                AccountMeta::new(lp_mint, false),
                AccountMeta::new(*user_a_ata, false),
                AccountMeta::new(*user_b_ata, false),
                AccountMeta::new(*user_lp_ata, false),
                AccountMeta::new_readonly(*user, true),
                AccountMeta::new_readonly(self.mint_a.pubkey(), false),
                AccountMeta::new_readonly(self.mint_b.pubkey(), false),
                AccountMeta::new_readonly(self.token_program, false),
            ],
            data: borsh::to_vec(&data).unwrap(),
        }
    }

    pub fn ix_remove_liquidity(
        &self,
        user: &Pubkey,
        user_a_ata: &Pubkey,
        user_b_ata: &Pubkey,
        user_lp_ata: &Pubkey,
        lp_amount: u64,
        min_a_out: u64,
        min_b_out: u64,
    ) -> Instruction {
        let pool = self.pool_pda().0;
        let vault_a = self.vault_a_pda().0;
        let vault_b = self.vault_b_pda().0;
        let lp_mint = self.lp_mint_pda().0;
        let data = LiquidityInstruction::RemoveLiquidity {
            lp_amount,
            min_a_out,
            min_b_out,
        };
        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(vault_a, false),
                AccountMeta::new(vault_b, false),
                AccountMeta::new(lp_mint, false),
                AccountMeta::new(*user_a_ata, false),
                AccountMeta::new(*user_b_ata, false),
                AccountMeta::new(*user_lp_ata, false),
                AccountMeta::new_readonly(*user, true),
                AccountMeta::new_readonly(self.mint_a.pubkey(), false),
                AccountMeta::new_readonly(self.mint_b.pubkey(), false),
                AccountMeta::new_readonly(self.token_program, false),
            ],
            data: borsh::to_vec(&data).unwrap(),
        }
    }

    /// One-stop helper: create a user funded with some lamports and tokens
    /// of both mints, plus an LP ATA. Returns `(user, ata_a, ata_b, ata_lp)`.
    pub async fn setup_user(
        &mut self,
        sol_lamports: u64,
        token_a: u64,
        token_b: u64,
    ) -> (Keypair, Pubkey, Pubkey, Pubkey) {
        let user = self.create_funded_user(sol_lamports).await;
        let ata_a = self
            .fund_token(&user.pubkey(), &self.mint_a.pubkey(), token_a)
            .await;
        let ata_b = self
            .fund_token(&user.pubkey(), &self.mint_b.pubkey(), token_b)
            .await;
        let ata_lp = self.create_ata(&user.pubkey(), &self.lp_mint_pda().0).await;
        (user, ata_a, ata_b, ata_lp)
    }

    /// `setup_user` + `initialize_pool_default`, then make one initial
    /// LP deposit. The depositor (returned) seeds the pool with
    /// `(amount_a, amount_b)`.
    pub async fn setup_pool_with_liquidity(
        &mut self,
        amount_a: u64,
        amount_b: u64,
    ) -> (Keypair, Pubkey, Pubkey, Pubkey) {
        self.initialize_pool_default().await;
        // First-deposit minimum is 1e6 per side; tests should pass amounts above.
        let (user, ata_a, ata_b, ata_lp) =
            self.setup_user(10_000_000_000, amount_a * 2, amount_b * 2).await;
        let ix = self.ix_add_liquidity(
            &user.pubkey(),
            &ata_a,
            &ata_b,
            &ata_lp,
            amount_a,
            amount_b,
            1,
        );
        self.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();
        (user, ata_a, ata_b, ata_lp)
    }

    // ---- Loan helpers ----

    pub async fn band_state(
        &mut self,
        direction: u8,
        band_id: u32,
    ) -> Option<LoanIndexBand> {
        let (band_pda, _) = self.band_pda(direction, band_id);
        self.banks_client
            .get_account(band_pda)
            .await
            .ok()
            .flatten()
            .and_then(|acc| LoanIndexBand::try_from_slice(&acc.data).ok())
    }

    pub async fn loan_state(&mut self, loan_pda: &Pubkey) -> Option<Loan> {
        self.banks_client
            .get_account(*loan_pda)
            .await
            .ok()
            .flatten()
            .and_then(|acc| Loan::try_from_slice(&acc.data).ok())
    }

    pub async fn loan_link_state(&mut self, link_pda: &Pubkey) -> Option<LoanLink> {
        self.banks_client
            .get_account(*link_pda)
            .await
            .ok()
            .flatten()
            .and_then(|acc| LoanLink::try_from_slice(&acc.data).ok())
    }

    /// Build + submit an OpenLoan instruction. Returns the assigned nonce
    /// (= pool.next_loan_nonce at call time) so the test can rederive the
    /// loan PDA later.
    pub async fn open_loan(
        &mut self,
        borrower: &Keypair,
        user_a: &Pubkey,
        user_b: &Pubkey,
        sides: u8,
        collateral_amount: u64,
        debt_amount: u64,
    ) -> Result<u64, TransportError> {
        let pool = self.pool_state().await;
        let nonce = pool.next_loan_nonce;
        let liq_ratio = pool.liq_ratio_bps;

        let (loan_pda, _) = self.loan_pda(&borrower.pubkey(), nonce);
        let (link_pda, _) = self.loan_link_pda(&loan_pda);

        let sides_enum =
            chiefliquidity::math::LoanSides::from_u8(sides).expect("sides byte");
        let (trigger_wad, dir) = chiefliquidity::math::recompute_trigger(
            sides_enum,
            collateral_amount as u128,
            debt_amount as u128,
            liq_ratio,
        )
        .expect("trigger");
        let band_id =
            chiefliquidity::math::band_id_for_trigger(trigger_wad).expect("band_id");
        let (band_pda, _) = self.band_pda(dir as u8, band_id);

        // Old tail: existing band tail if non-empty; else placeholder = link_pda.
        let old_tail = self
            .band_state(dir as u8, band_id)
            .await
            .filter(|b| b.count > 0)
            .map(|b| b.tail_link)
            .unwrap_or(link_pda);

        let data = LiquidityInstruction::OpenLoan {
            sides,
            collateral_amount,
            debt_amount,
            nonce,
        };
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.pool_pda().0, false),
                AccountMeta::new(self.vault_a_pda().0, false),
                AccountMeta::new(self.vault_b_pda().0, false),
                AccountMeta::new(*user_a, false),
                AccountMeta::new(*user_b, false),
                AccountMeta::new_readonly(self.mint_a.pubkey(), false),
                AccountMeta::new_readonly(self.mint_b.pubkey(), false),
                AccountMeta::new(borrower.pubkey(), true),
                AccountMeta::new(loan_pda, false),
                AccountMeta::new(link_pda, false),
                AccountMeta::new(band_pda, false),
                AccountMeta::new(old_tail, false),
                AccountMeta::new_readonly(solana_program::system_program::id(), false),
                AccountMeta::new_readonly(self.token_program, false),
            ],
            data: borsh::to_vec(&data).unwrap(),
        };
        self.send_with_new_blockhash(&[ix], &[borrower]).await?;
        Ok(nonce)
    }

    /// Build + submit a RepayLoan instruction.
    pub async fn repay_loan(
        &mut self,
        borrower: &Keypair,
        user_a: &Pubkey,
        user_b: &Pubkey,
        nonce: u64,
    ) -> Result<(), TransportError> {
        let (loan_pda, _) = self.loan_pda(&borrower.pubkey(), nonce);
        let (link_pda, _) = self.loan_link_pda(&loan_pda);
        let link = self
            .loan_link_state(&link_pda)
            .await
            .expect("loan_link not found");
        let (band_pda, _) = self.band_pda(link.direction, link.band_id);

        // Prev/next: if default, pass link itself as placeholder.
        let prev = if link.prev == Pubkey::default() {
            link_pda
        } else {
            link.prev
        };
        let next = if link.next == Pubkey::default() {
            link_pda
        } else {
            link.next
        };

        let data = LiquidityInstruction::RepayLoan;
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.pool_pda().0, false),
                AccountMeta::new(self.vault_a_pda().0, false),
                AccountMeta::new(self.vault_b_pda().0, false),
                AccountMeta::new(*user_a, false),
                AccountMeta::new(*user_b, false),
                AccountMeta::new_readonly(self.mint_a.pubkey(), false),
                AccountMeta::new_readonly(self.mint_b.pubkey(), false),
                AccountMeta::new(borrower.pubkey(), true),
                AccountMeta::new(loan_pda, false),
                AccountMeta::new(link_pda, false),
                AccountMeta::new(band_pda, false),
                AccountMeta::new(prev, false),
                AccountMeta::new(next, false),
                AccountMeta::new_readonly(self.token_program, false),
            ],
            data: borsh::to_vec(&data).unwrap(),
        };
        self.send_with_new_blockhash(&[ix], &[borrower]).await
    }

    // ---- Swap helpers ----

    /// Build and submit a Swap instruction with explicit per-band context.
    /// `bands` is `[(band_id, [(link_pda, loan_pda), ...]), ...]` in
    /// chain order (band.head_link forward via .next pointers).
    #[allow(clippy::too_many_arguments)]
    pub async fn swap(
        &mut self,
        user: &Keypair,
        ata_a: &Pubkey,
        ata_b: &Pubkey,
        amount_in: u64,
        min_out: u64,
        a_to_b: bool,
        band_boundary: u32,
        bands: &[(u32, Vec<(Pubkey, Pubkey)>)],
    ) -> Result<(), TransportError> {
        let mut accounts = vec![
            AccountMeta::new(self.pool_pda().0, false),
            AccountMeta::new(self.vault_a_pda().0, false),
            AccountMeta::new(self.vault_b_pda().0, false),
            AccountMeta::new(*ata_a, false),
            AccountMeta::new(*ata_b, false),
            AccountMeta::new_readonly(self.mint_a.pubkey(), false),
            AccountMeta::new_readonly(self.mint_b.pubkey(), false),
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new_readonly(self.token_program, false),
        ];
        let direction: u8 = if a_to_b { 0 } else { 1 };
        let mut band_link_counts = Vec::with_capacity(bands.len());
        for (band_id, chain) in bands {
            band_link_counts.push(chain.len() as u8);
            let (band_pda, _) = self.band_pda(direction, *band_id);
            accounts.push(AccountMeta::new(band_pda, false));
            for (link, _) in chain {
                accounts.push(AccountMeta::new(*link, false));
            }
            for (_, loan) in chain {
                accounts.push(AccountMeta::new(*loan, false));
            }
        }
        let data = LiquidityInstruction::Swap {
            amount_in,
            min_out,
            a_to_b,
            band_boundary,
            band_link_counts,
        };
        let ix = Instruction {
            program_id: self.program_id,
            accounts,
            data: borsh::to_vec(&data).unwrap(),
        };
        self.send_with_new_blockhash(&[ix], &[user]).await
    }

    /// Convenience: enumerate every populated band in the swap-relevant
    /// direction (entire bitmap) and walk each band's chain. Yields the
    /// `bands` argument for `swap` along with a wide-open boundary that
    /// covers any cascade. Use this for happy-path tests that don't care
    /// about minimizing the supplied account list.
    pub async fn swap_full(
        &mut self,
        user: &Keypair,
        ata_a: &Pubkey,
        ata_b: &Pubkey,
        amount_in: u64,
        min_out: u64,
        a_to_b: bool,
    ) -> Result<(), TransportError> {
        let pool = self.pool_state().await;
        let direction: u8 = if a_to_b { 0 } else { 1 };
        let bitmap = if a_to_b {
            pool.band_bitmap_fall
        } else {
            pool.band_bitmap_rise
        };
        let boundary: u32 = if a_to_b { 0 } else { 127 };
        let mut bands = Vec::new();
        for band_id in 0u32..=127 {
            if !bitmap_is_set(&bitmap, band_id) {
                continue;
            }
            let band = self.band_state(direction, band_id).await.unwrap();
            let mut chain = Vec::new();
            let mut cur = band.head_link;
            while cur != Pubkey::default() {
                let link = self.loan_link_state(&cur).await.unwrap();
                chain.push((cur, link.loan));
                cur = link.next;
            }
            bands.push((band_id, chain));
        }
        self.swap(user, ata_a, ata_b, amount_in, min_out, a_to_b, boundary, &bands)
            .await
    }

    // ---- Admin helpers ----

    pub async fn transfer_authority(
        &mut self,
        authority: &Keypair,
        new_authority: Pubkey,
    ) -> Result<(), TransportError> {
        let data = LiquidityInstruction::TransferAuthority { new_authority };
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.pool_pda().0, false),
                AccountMeta::new_readonly(authority.pubkey(), true),
            ],
            data: borsh::to_vec(&data).unwrap(),
        };
        self.send_with_new_blockhash(&[ix], &[authority]).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_pool_settings(
        &mut self,
        authority: &Keypair,
        params: &PoolParams,
    ) -> Result<(), TransportError> {
        let data = LiquidityInstruction::UpdatePoolSettings {
            swap_fee_bps: params.swap_fee_bps,
            protocol_fee_bps: params.protocol_fee_bps,
            liq_ratio_bps: params.liq_ratio_bps,
            max_ltv_bps: params.max_ltv_bps,
            interest_base_bps_per_year: params.interest_base_bps_per_year,
            interest_slope1_bps_per_year: params.interest_slope1_bps_per_year,
            interest_slope2_bps_per_year: params.interest_slope2_bps_per_year,
            interest_kink_bps: params.interest_kink_bps,
        };
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.pool_pda().0, false),
                AccountMeta::new_readonly(self.vault_a_pda().0, false),
                AccountMeta::new_readonly(self.vault_b_pda().0, false),
                AccountMeta::new_readonly(authority.pubkey(), true),
            ],
            data: borsh::to_vec(&data).unwrap(),
        };
        self.send_with_new_blockhash(&[ix], &[authority]).await
    }

    pub async fn claim_protocol_fees(
        &mut self,
        authority: &Keypair,
        dest_a: &Pubkey,
        dest_b: &Pubkey,
    ) -> Result<(), TransportError> {
        let data = LiquidityInstruction::ClaimProtocolFees;
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.pool_pda().0, false),
                AccountMeta::new(self.vault_a_pda().0, false),
                AccountMeta::new(self.vault_b_pda().0, false),
                AccountMeta::new(*dest_a, false),
                AccountMeta::new(*dest_b, false),
                AccountMeta::new_readonly(self.mint_a.pubkey(), false),
                AccountMeta::new_readonly(self.mint_b.pubkey(), false),
                AccountMeta::new_readonly(authority.pubkey(), true),
                AccountMeta::new_readonly(self.token_program, false),
            ],
            data: borsh::to_vec(&data).unwrap(),
        };
        self.send_with_new_blockhash(&[ix], &[authority]).await
    }

    /// Build + submit a ClaimLiquidatedRent instruction.
    pub async fn claim_liquidated_rent(
        &mut self,
        borrower: &Keypair,
        nonce: u64,
    ) -> Result<(), TransportError> {
        let (loan_pda, _) = self.loan_pda(&borrower.pubkey(), nonce);
        let (link_pda, _) = self.loan_link_pda(&loan_pda);
        let data = LiquidityInstruction::ClaimLiquidatedRent;
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(loan_pda, false),
                AccountMeta::new(link_pda, false),
                AccountMeta::new(borrower.pubkey(), true),
            ],
            data: borsh::to_vec(&data).unwrap(),
        };
        self.send_with_new_blockhash(&[ix], &[borrower]).await
    }

    // ---- State readers ----

    pub async fn pool_state(&mut self) -> Pool {
        let acc = self
            .banks_client
            .get_account(self.pool_pda().0)
            .await
            .unwrap()
            .expect("pool account not found");
        Pool::try_from_slice(&acc.data).unwrap()
    }

    pub async fn maybe_pool_state(&mut self) -> Option<Pool> {
        self.banks_client
            .get_account(self.pool_pda().0)
            .await
            .ok()
            .flatten()
            .and_then(|acc| Pool::try_from_slice(&acc.data).ok())
    }

    pub async fn token_balance(&mut self, ata: &Pubkey) -> u64 {
        let acc = self
            .banks_client
            .get_account(*ata)
            .await
            .unwrap()
            .expect("token account not found");
        StateWithExtensions::<TokenAccount>::unpack(&acc.data)
            .unwrap()
            .base
            .amount
    }

    pub async fn mint_supply(&mut self, mint: &Pubkey) -> u64 {
        let acc = self
            .banks_client
            .get_account(*mint)
            .await
            .unwrap()
            .expect("mint not found");
        StateWithExtensions::<Mint>::unpack(&acc.data)
            .unwrap()
            .base
            .supply
    }
}

// ---- Standalone helpers ----

async fn create_mint(
    banks_client: &mut BanksClient,
    payer: &Keypair,
    blockhash: Hash,
    mint: &Keypair,
    mint_authority: Pubkey,
    decimals: u8,
    token_program: Pubkey,
) {
    let rent = banks_client.get_rent().await.unwrap();
    let mint_size = Mint::LEN;
    let mint_rent = rent.minimum_balance(mint_size);

    let create_ix = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        mint_rent,
        mint_size as u64,
        &token_program,
    );
    let init_ix = token_ix::initialize_mint2(
        &token_program,
        &mint.pubkey(),
        &mint_authority,
        None,
        decimals,
    )
    .unwrap();

    let mut tx = Transaction::new_with_payer(&[create_ix, init_ix], Some(&payer.pubkey()));
    tx.sign(&[payer, mint], blockhash);
    banks_client.process_transaction(tx).await.unwrap();
}

/// Convert a `BanksClient` error into a `(custom_code, error_name)` pair if
/// the underlying error is a `ProgramError::Custom(n)`. Useful for asserting
/// that an instruction reverted with the expected `LiquidityError` variant.
pub fn extract_custom_error(err: &TransportError) -> Option<u32> {
    let msg = format!("{err:?}");
    // Looks for "Custom(<n>)" in the error chain.
    let needle = "Custom(";
    let idx = msg.find(needle)?;
    let rest = &msg[idx + needle.len()..];
    let end = rest.find(')')?;
    rest[..end].trim().parse().ok()
}

/// Cast a `LiquidityError` enum variant to its u32 discriminant.
pub fn err_code(e: chiefliquidity::error::LiquidityError) -> u32 {
    e as u32
}
