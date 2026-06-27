/**
 * ChiefLiquidity compute-unit benchmark (DESIGN.md §6.6 / §9.4).
 *
 * The protocol's core promise — "no successful swap leaves the pool unable to
 * satisfy its obligations" — is enforced by liquidating triggered loans *inside
 * the swap instruction*. That only holds if the worst-case cascade actually
 * fits in a Solana transaction's compute budget. `MAX_LIQ_PER_SWAP` (8) was
 * chosen by reasoning; this suite turns it into a measured, defensible number.
 *
 * Runs against the same live validator as test_liquidity.ts and reuses its
 * `Ctx` harness. It does three things:
 *
 *   1. Depth sweep (0,1,2,4,8 liquidations in one band) → isolates the fixed
 *      swap overhead from the marginal CU per liquidation via a linear fit.
 *   2. Worst-case account layout: 8 liquidations spread across 8 *distinct*
 *      bands — the maximum number of band PDAs the on-chain bitmap walk must
 *      load and decrypt in a single instruction. This is the true CU ceiling
 *      of the cascade and the headline assertion.
 *   3. The binding constraint: measures the worst-case swap *transaction size*
 *      against the 1232-byte legacy limit and shows that — not CU — is what
 *      caps MAX_LIQ_PER_SWAP. Every liquidated loan + band PDA is a 32-byte
 *      account key, so the account list fills the transaction long before the
 *      cascade exhausts the compute budget (CU has ~10x more headroom).
 *
 * All measurements use Token-2022 (heavier token CPI than legacy SPL) so the
 * numbers are the conservative upper bound across both supported token programs.
 */

import {
  ComputeBudgetProgram,
  Connection,
  Keypair,
  PublicKey,
  Transaction,
  TransactionInstruction,
  LAMPORTS_PER_SOL,
} from '@solana/web3.js';
import { TOKEN_2022_PROGRAM_ID, mintTo } from '@solana/spl-token';

import {
  Ctx, DEFAULT_PARAMS,
  COLL_A, DIR_FALL,
  PROGRAM_ID, WAD, BPS, MAX_LIQ_PER_SWAP,
  bandIdForTrigger, recomputeTrigger,
  isqrt, waitFor,
  assert, assertEq,
} from './test_liquidity';

// The per-transaction compute ceiling on Solana: a caller can request at most
// 1,400,000 CU via ComputeBudgetProgram. A cascade that needs more than this
// can never land — which is exactly the solvency failure the cap exists to
// prevent. This is the hard line the worst case must clear.
const TX_CU_CEILING = 1_400_000;

// Regression guard: the measured worst case must keep this much headroom under
// the ceiling. Loose enough to absorb compiler/runtime drift, tight enough to
// catch a real blow-up (e.g. an accidental per-loan quadratic).
const HEADROOM_FACTOR = 2.0; // worst case must be < CEILING / 2

// The *actual* binding constraint on MAX_LIQ_PER_SWAP. A legacy Solana
// transaction is capped at 1232 bytes, and every liquidated loan + its band
// PDA must be passed as a 32-byte account key. CU has ~10x more headroom than
// this (see the projection test), so the cap is set by account-list size, not
// compute. Reaching higher caps requires v0 transactions + address lookup
// tables, not a bigger compute budget.
const LEGACY_TX_LIMIT = 1232;

const POOL_A = 1_000_000_000n;
const POOL_B = 4_000_000_000n;

// ===== CU measurement =====

async function swapCu(
  ctx: Ctx,
  connection: Connection,
  trader: Keypair,
  amountIn: bigint,
  aToB: boolean,
): Promise<number> {
  const sig = await ctx.swap(trader, amountIn, 1n, aToB);
  // The swap commits at 'confirmed'; the ledger entry carries the metered CU.
  const tx = await connection.getTransaction(sig, {
    commitment: 'confirmed',
    maxSupportedTransactionVersion: 0,
  });
  const cu = tx?.meta?.computeUnitsConsumed;
  if (cu === undefined || cu === null) throw new Error(`no CU metered for ${sig}`);
  return cu;
}

/**
 * Open `n` identical CollateralA loans. With identical (collateral, debt) every
 * loan shares one trigger and therefore one band — the cheapest layout per
 * liquidation (a single band PDA covers all of them). Returns the shared band.
 */
async function openSameBandLoans(
  ctx: Ctx,
  connection: Connection,
  payer: Keypair,
  tokenProgram: PublicKey,
  borrower: Keypair,
  n: number,
  collateral: bigint,
  debt: bigint,
): Promise<number> {
  let nonce = (await ctx.poolState()).nextLoanNonce;
  let bandId = -1;
  for (let i = 0; i < n; i++) {
    const r = await ctx.openLoan(borrower, COLL_A, collateral, debt,
      { knownNonce: nonce, commitment: 'processed' });
    bandId = r.bandId;
    nonce += 1n;
  }
  await waitFor(async () => (await ctx.poolState()).openLoans === BigInt(n),
    `${n} same-band opens confirmed`);
  return bandId;
}

// ===== Main =====

async function run() {
  console.log('=== ChiefLiquidity Compute-Unit Benchmark ===\n');

  const connection = new Connection('http://localhost:8899', 'confirmed');
  try { await connection.getVersion(); } catch {
    console.error('ERROR: validator not reachable on localhost:8899 — run ./scripts/run-e2e-tests.sh');
    process.exit(1);
  }
  if (!(await connection.getAccountInfo(PROGRAM_ID))) {
    console.error(`ERROR: program ${PROGRAM_ID.toBase58()} not deployed.`);
    process.exit(1);
  }

  const payer = Keypair.generate();
  {
    const sig = await connection.requestAirdrop(payer.publicKey, 2_000 * LAMPORTS_PER_SOL);
    await connection.confirmTransaction(sig);
  }
  const tokenProgram = TOKEN_2022_PROGRAM_ID;

  let passed = 0, failed = 0;
  async function test(name: string, fn: () => Promise<void>) {
    const t0 = Date.now();
    try {
      await fn();
      console.log(`✓ ${name} (${((Date.now() - t0) / 1000).toFixed(1)}s)`);
      passed++;
    } catch (e: any) {
      console.log(`✗ ${name}`);
      console.log(`  Error: ${e.message}`);
      if (e.logs) console.log(`  Logs: ${e.logs.slice(-6).join('\n        ')}`);
      failed++;
    }
  }

  const fresh = async (): Promise<Ctx> => {
    const ctx = new Ctx(connection, payer, tokenProgram);
    await ctx.setup();
    await ctx.setupPoolWithLiquidity(POOL_A, POOL_B);
    return ctx;
  };

  // Collected so the depth sweep and the worst-case test can share findings.
  const cuByDepth = new Map<number, number>();

  // ---------- 1. Depth sweep: marginal CU per liquidation ----------
  //
  // A 200M-A swap drops the price from 4.0 to ~2.78, below the shared 3.3
  // trigger of every (10M coll, 30M debt) loan, so all `depth` of them
  // liquidate in one instruction. Each depth runs on a fresh pool.

  for (const depth of [0, 1, 2, 4, MAX_LIQ_PER_SWAP]) {
    await test(`Depth sweep: ${depth} liquidation(s) in one band`, async () => {
      const ctx = await fresh();
      if (depth > 0) {
        const borrower = await ctx.newUser(20, BigInt(depth) * 10_000_000n, 0n);
        await openSameBandLoans(ctx, connection, payer, tokenProgram, borrower,
          depth, 10_000_000n, 30_000_000n);
        assertEq((await ctx.poolState()).openLoans, BigInt(depth), 'loans open');
      }
      const trader = await ctx.newUser(20, 200_000_000n, 0n);
      const cu = await swapCu(ctx, connection, trader, 200_000_000n, true);
      assertEq((await ctx.poolState()).openLoans, 0n, 'all loans liquidated');
      cuByDepth.set(depth, cu);
      console.log(`    depth=${depth}: ${cu.toLocaleString()} CU`);
    });
  }

  await test('Depth sweep: CU grows monotonically and roughly linearly', async () => {
    const depths = [0, 1, 2, 4, MAX_LIQ_PER_SWAP].filter((d) => cuByDepth.has(d));
    assert(depths.length >= 4, 'enough sweep samples collected');
    for (let i = 1; i < depths.length; i++) {
      assert(cuByDepth.get(depths[i])! >= cuByDepth.get(depths[i - 1])!,
        `CU non-decreasing: depth ${depths[i]} >= depth ${depths[i - 1]}`);
    }
    const base = cuByDepth.get(0)!;
    const top = cuByDepth.get(MAX_LIQ_PER_SWAP)!;
    const marginal = Math.round((top - base) / MAX_LIQ_PER_SWAP);
    console.log(`    swap floor (0 liq):       ${base.toLocaleString()} CU`);
    console.log(`    marginal per liquidation: ~${marginal.toLocaleString()} CU`);
    assert(marginal > 0, 'each liquidation costs measurable CU');
    // Sanity: the per-liquidation cost should be a small fraction of the budget.
    assert(marginal < TX_CU_CEILING / 10,
      `marginal per liquidation (${marginal}) is well under budget`);
  });

  // ---------- 2. Worst-case layout: 8 liquidations across 8 distinct bands ----------
  //
  // Distinct bands are the costliest layout: the on-chain bitmap walk must load
  // and decode one band PDA per band (8 vs 1), on top of the 8 per-loan
  // liquidations. Doubling each loan's debt steps the trigger up exactly one
  // log2 band, giving 8 consecutive, distinct bands that all fire on one swap.

  await test('Worst case: 8 liquidations across 8 distinct bands fits CU budget', async () => {
    const ctx = await fresh();
    const COLL = 20_000_000n;
    const DEBT0 = 450_000n; // trigger_0 ≈ 0.0248; trigger_7 ≈ 3.17 (ltv 0.72 < 0.80)

    const borrower = await ctx.newUser(30, COLL * BigInt(MAX_LIQ_PER_SWAP), 0n);
    const liqRatio = BigInt((await ctx.poolState()).liqRatioBps);
    const bandIds = new Set<number>();
    let nonce = (await ctx.poolState()).nextLoanNonce;
    for (let i = 0; i < MAX_LIQ_PER_SWAP; i++) {
      const debt = DEBT0 << BigInt(i);
      const { triggerWad } = recomputeTrigger(COLL_A, COLL, debt, liqRatio);
      bandIds.add(bandIdForTrigger(triggerWad));
      await ctx.openLoan(borrower, COLL_A, COLL, debt,
        { knownNonce: nonce, commitment: 'processed' });
      nonce += 1n;
    }
    await waitFor(async () => (await ctx.poolState()).openLoans === BigInt(MAX_LIQ_PER_SWAP),
      'eight distinct-band opens confirmed');
    assertEq(bandIds.size, MAX_LIQ_PER_SWAP, '8 distinct bands');

    // Size the swap so the post-trade price lands just under the lowest trigger,
    // guaranteeing every band fires. Computed from accounted reserves (the
    // basis the program prices on); liquidations only push the price further
    // down, so targeting the pre-cascade reserves is conservative.
    const pool = await ctx.poolState();
    const { a: realA, b: realB } = await ctx.vaultBalances();
    const accA = realA - pool.totalCollateralA - pool.protocolFeesA + pool.totalDebtA;
    const accB = realB - pool.totalCollateralB - pool.protocolFeesB + pool.totalDebtB;
    const { triggerWad: trigger0 } =
      recomputeTrigger(COLL_A, COLL, DEBT0, liqRatio);
    const targetWad = (trigger0 * 95n) / 100n;          // 5% below the lowest trigger
    const accAPrime = isqrt((accA * accB * WAD) / targetWad);
    const feeBps = BigInt(pool.swapFeeBps);
    const amountIn = ((accAPrime - accA) * BPS) / (BPS - feeBps) * 105n / 100n; // +5% buffer

    const trader = await ctx.newUser(30, amountIn + amountIn / 10n, 0n);
    await mintTo(connection, payer, ctx.mintA, ctx.ata(trader.publicKey, ctx.mintA),
      payer, amountIn, [], { commitment: 'confirmed' }, tokenProgram);

    const cu = await swapCu(ctx, connection, trader, amountIn, true);
    assertEq((await ctx.poolState()).openLoans, 0n, 'all 8 liquidated');

    const ceilingHeadroom = ((TX_CU_CEILING - cu) / TX_CU_CEILING * 100).toFixed(1);
    console.log(`    8 distinct bands: ${cu.toLocaleString()} CU ` +
      `(${ceilingHeadroom}% headroom under ${TX_CU_CEILING.toLocaleString()})`);

    if (cuByDepth.has(MAX_LIQ_PER_SWAP)) {
      const sameBand = cuByDepth.get(MAX_LIQ_PER_SWAP)!;
      const perBand = Math.round((cu - sameBand) / (MAX_LIQ_PER_SWAP - 1));
      console.log(`    vs ${sameBand.toLocaleString()} CU same-band ` +
        `→ ~${perBand.toLocaleString()} CU per extra band PDA`);
      assert(cu >= sameBand, 'distinct bands cost at least as much as one band');
    }

    // The headline gate: the worst case must clear the per-tx ceiling with
    // the required margin.
    assert(cu < TX_CU_CEILING, `worst case ${cu} under hard ceiling ${TX_CU_CEILING}`);
    assert(cu < TX_CU_CEILING / HEADROOM_FACTOR,
      `worst case ${cu} keeps ${HEADROOM_FACTOR}x headroom (< ${TX_CU_CEILING / HEADROOM_FACTOR})`);
  });

  // ---------- 3. The binding constraint: transaction size, not CU ----------
  //
  // CU has enormous headroom (the projection below), so it is NOT what caps
  // MAX_LIQ_PER_SWAP. The real limit is the 1232-byte legacy transaction: every
  // liquidated loan + its band PDA is a 32-byte account key in the message. This
  // test measures the worst-case swap transaction directly and shows the cap of
  // 8 is sized to that limit — raising it needs v0 + address lookup tables, not
  // a bigger compute budget.

  // Serialized size (signatures + message) of a legacy swap tx carrying the
  // given liquidation context. Uses synthetic pubkeys — size depends only on
  // the account count, not on whether the accounts exist.
  function swapTxBytes(
    ctx: Ctx, trader: PublicKey,
    bands: { bandId: number; loans: PublicKey[] }[],
  ): number {
    const ix: TransactionInstruction = ctx.ixSwap(trader, 1n, 1n, true, 0, bands);
    const tx = new Transaction().add(
      ComputeBudgetProgram.setComputeUnitLimit({ units: TX_CU_CEILING }), ix);
    tx.feePayer = trader;
    tx.recentBlockhash = '11111111111111111111111111111111'; // 32 zero bytes
    const msg = tx.compileMessage().serialize();
    const numSigs = tx.compileMessage().header.numRequiredSignatures;
    return 1 + 64 * numSigs + msg.length; // sig-count byte + sigs + message
  }

  await test('Worst-case swap transaction fits the legacy 1232-byte limit', async () => {
    const ctx = new Ctx(connection, payer, tokenProgram);
    await ctx.setup(); // derive pool/vault/mint PDAs (no on-chain init needed)
    const trader = Keypair.generate().publicKey;
    const fakeLoan = () => Keypair.generate().publicKey;

    // Worst-case layout for account count: each liquidation in its own band, so
    // every liquidation costs TWO account keys (a band PDA + a loan).
    const distinctBands = Array.from({ length: MAX_LIQ_PER_SWAP },
      (_, i) => ({ bandId: i, loans: [fakeLoan()] }));

    const size0 = swapTxBytes(ctx, trader, []);
    const sizeCap = swapTxBytes(ctx, trader, distinctBands);
    const perLiq = (sizeCap - size0) / MAX_LIQ_PER_SWAP;
    const txMax = Math.floor((LEGACY_TX_LIMIT - size0) / perLiq);

    console.log(`    swap tx floor (0 liq):   ${size0} bytes`);
    console.log(`    swap tx at cap (8 liq):  ${sizeCap} bytes / ${LEGACY_TX_LIMIT} limit`);
    console.log(`    ~${perLiq.toFixed(0)} bytes per liquidation → legacy tx allows ~${txMax}`);

    assert(sizeCap <= LEGACY_TX_LIMIT,
      `worst-case cap-depth swap (${sizeCap}B) fits legacy tx (${LEGACY_TX_LIMIT}B)`);
    // The cap must not exceed what a transaction can actually carry.
    assert(MAX_LIQ_PER_SWAP <= txMax,
      `cap ${MAX_LIQ_PER_SWAP} fits the tx-size ceiling (~${txMax})`);
  });

  await test('Projection: CU is not the binding constraint (tx size is)', async () => {
    const base = cuByDepth.get(0);
    const top = cuByDepth.get(MAX_LIQ_PER_SWAP);
    assert(base !== undefined && top !== undefined, 'sweep endpoints present');
    const marginal = (top! - base!) / MAX_LIQ_PER_SWAP;
    // Liquidations the CU budget *alone* would allow. This is deliberately NOT a
    // justification for raising the cap: it far exceeds what a legacy
    // transaction can carry (previous test), which is the real ceiling. It only
    // confirms compute is comfortably in surplus at the current cap.
    const cuMax = Math.floor((TX_CU_CEILING - base!) / marginal);
    console.log(`    CU budget alone would allow ~${cuMax} liquidations — but tx size caps it far lower`);
    assert(cuMax > MAX_LIQ_PER_SWAP * 4,
      `CU is in clear surplus at the cap (projected ${cuMax} >> ${MAX_LIQ_PER_SWAP})`);
  });

  console.log(`\n=== Benchmark: ${passed} passed, ${failed} failed ===`);
  process.exit(failed > 0 ? 1 : 0);
}

run().catch((e) => {
  console.error(e);
  process.exit(1);
});
