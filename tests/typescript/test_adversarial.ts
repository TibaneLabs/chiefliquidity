/**
 * ChiefLiquidity adversarial + invariant-fuzzer E2E suite.
 *
 * Runs against the same live validator as test_liquidity.ts and reuses its
 * harness (the `Ctx` client). Two parts:
 *
 *   1. Account-substitution attacks on the swap liquidation-context path —
 *      the security-critical surface where the solvency completeness proof
 *      lives (DESIGN.md §6.3). Each test hands the program a deliberately
 *      malformed account tail and asserts the *specific* guard that rejects it.
 *
 *   2. A randomized invariant fuzzer: a seeded sequence of arbitrary
 *      operations (add/remove liquidity, swap-with-liquidation, open, repay)
 *      with the protocol's core accounting + solvency invariants checked after
 *      every committed transaction. Any drift trips an assertion that prints
 *      the seed and the operation log so the failure reproduces deterministically.
 *
 * Reverts are expected and fine during fuzzing (a rejected tx commits nothing,
 * so invariants can only break via a *successful* tx). The fuzzer's job is to
 * find a sequence the program accepts that nonetheless violates an invariant.
 */

import {
  ComputeBudgetProgram,
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
  AccountMeta,
  LAMPORTS_PER_SOL,
  sendAndConfirmTransaction,
} from '@solana/web3.js';
import { TOKEN_2022_PROGRAM_ID, mintTo, transfer } from '@solana/spl-token';
import bs58 from 'bs58';

import {
  Ctx, Err, Ix, DEFAULT_PARAMS,
  COLL_A, COLL_B, DIR_FALL, DIR_RISE,
  PROGRAM_ID, LOAN_LEN, LOAN_OFF, LOAN_DISCRIMINATOR,
  BPS, BAND_MAX_LOANS,
  decodeLoan, assert, assertEq, expectError,
} from './test_liquidity';

// ===== Low-level swap instruction builder (full control over the tail) =====
//
// Ctx.ixSwap derives every band PDA from the swap direction, which is exactly
// what a correct caller does. To forge a malformed context we need to place
// arbitrary accounts in the tail — so this builder takes explicit band PDAs,
// loan lists, and an optional trailing-junk array.

interface RawBand {
  bandPda: PublicKey;
  loans: PublicKey[];
}

function rawSwapIx(
  ctx: Ctx,
  user: PublicKey,
  amountIn: bigint,
  minOut: bigint,
  aToB: boolean,
  boundary: number,
  bands: RawBand[],
  extraTail: AccountMeta[] = [],
): TransactionInstruction {
  const counts = bands.map((b) => b.loans.length);
  const data = Buffer.alloc(1 + 8 + 8 + 1 + 4 + 4 + counts.length);
  let o = 0;
  data.writeUInt8(Ix.Swap, o); o += 1;
  data.writeBigUInt64LE(amountIn, o); o += 8;
  data.writeBigUInt64LE(minOut, o); o += 8;
  data.writeUInt8(aToB ? 1 : 0, o); o += 1;
  data.writeUInt32LE(boundary, o); o += 4;
  data.writeUInt32LE(counts.length, o); o += 4; // Vec<u8> length prefix
  for (const c of counts) { data.writeUInt8(c, o); o += 1; }

  const keys: AccountMeta[] = [
    { pubkey: ctx.pool, isSigner: false, isWritable: true },
    { pubkey: ctx.vaultA, isSigner: false, isWritable: true },
    { pubkey: ctx.vaultB, isSigner: false, isWritable: true },
    { pubkey: ctx.ata(user, ctx.mintA), isSigner: false, isWritable: true },
    { pubkey: ctx.ata(user, ctx.mintB), isSigner: false, isWritable: true },
    { pubkey: ctx.mintA, isSigner: false, isWritable: false },
    { pubkey: ctx.mintB, isSigner: false, isWritable: false },
    { pubkey: user, isSigner: true, isWritable: false },
    { pubkey: ctx.tokenProgram, isSigner: false, isWritable: false },
  ];
  for (const b of bands) {
    keys.push({ pubkey: b.bandPda, isSigner: false, isWritable: true });
    for (const loan of b.loans) keys.push({ pubkey: loan, isSigner: false, isWritable: true });
  }
  keys.push(...extraTail);
  return new TransactionInstruction({ programId: PROGRAM_ID, keys, data });
}

function withCu(ix: TransactionInstruction): TransactionInstruction[] {
  return [ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }), ix];
}

// ===== Deterministic PRNG (mulberry32) =====

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return function () {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const randInt = (rng: () => number, lo: number, hi: number) =>
  lo + Math.floor(rng() * (hi - lo + 1));

// ===== Invariant checker =====
//
// Reads ground truth off-chain (pool + every open Loan via getProgramAccounts +
// vault balances + band PDAs) and verifies the protocol's load-bearing
// invariants. Throws with a descriptive message on the first violation.

async function allOpenLoans(connection: Connection, pool: PublicKey) {
  const accts = await connection.getProgramAccounts(PROGRAM_ID, {
    commitment: 'confirmed',
    filters: [
      { dataSize: LOAN_LEN },
      { memcmp: { offset: 0, bytes: bs58.encode(LOAN_DISCRIMINATOR) } },
      { memcmp: { offset: LOAN_OFF.pool, bytes: pool.toBase58() } },
    ],
  });
  return accts
    .map((a) => decodeLoan(a.account.data))
    .filter((l) => l.status === 0); // open only (tombstones are zeroed)
}

async function checkInvariants(ctx: Ctx, connection: Connection): Promise<void> {
  const pool = await ctx.poolState();
  const { a: realA, b: realB } = await ctx.vaultBalances();
  const loans = await allOpenLoans(connection, ctx.pool);

  // I1 — Solvency floor: real vault balance covers all earmarked funds
  // (borrower collateral + protocol-fee skim). LP-claimable inventory is
  // whatever is left; it can never go negative without theft.
  assert(realA >= pool.totalCollateralA + pool.protocolFeesA,
    `I1a solvency: realA=${realA} < collateral=${pool.totalCollateralA} + fees=${pool.protocolFeesA}`);
  assert(realB >= pool.totalCollateralB + pool.protocolFeesB,
    `I1b solvency: realB=${realB} < collateral=${pool.totalCollateralB} + fees=${pool.protocolFeesB}`);

  // I2 — Aggregate totals equal the sum over open loans, exactly (no drift).
  let collA = 0n, collB = 0n, debtA = 0n, debtB = 0n;
  for (const l of loans) {
    if (l.sides === COLL_A) { collA += l.collateral; debtB += l.debtPrincipal; }
    else { collB += l.collateral; debtA += l.debtPrincipal; }
  }
  assertEq(pool.totalCollateralA, collA, 'I2 total_collateral_a == Σ loan collateral (A)');
  assertEq(pool.totalCollateralB, collB, 'I2 total_collateral_b == Σ loan collateral (B)');
  assertEq(pool.totalDebtA, debtA, 'I2 total_debt_a == Σ loan principal (debt A)');
  assertEq(pool.totalDebtB, debtB, 'I2 total_debt_b == Σ loan principal (debt B)');

  // I3 — open_loans counter equals the actual number of open Loan accounts.
  assertEq(pool.openLoans, BigInt(loans.length), 'I3 open_loans == #open loan accounts');

  // I4 — Band counts and the Pool bitmap are an exact, drift-free reflection
  // of open-loan membership. This is the foundation of the completeness proof:
  // if a populated band's bit were ever stale, a swap could legally skip it.
  const groups = new Map<string, number>(); // `${dir}:${band}` -> count
  for (const l of loans) {
    const k = `${l.triggerDirection}:${l.bandId}`;
    groups.set(k, (groups.get(k) ?? 0) + 1);
  }
  for (const [k, expectedCount] of groups) {
    const [dirS, bandS] = k.split(':');
    const dir = Number(dirS), bandId = Number(bandS);
    const band = await ctx.bandState(dir, bandId);
    assert(band !== null, `I4 band PDA exists for populated ${k}`);
    assertEq(band!.count, expectedCount, `I4 band.count matches members for ${k}`);
    const bitmap = dir === DIR_FALL ? pool.bandBitmapFall : pool.bandBitmapRise;
    assert(bitmapIsSetLocal(bitmap, bandId), `I4 bitmap bit set for populated ${k}`);
  }
  // Reverse direction: no bitmap bit may be set without open members behind it.
  for (let bandId = 0; bandId <= 127; bandId++) {
    if (bitmapIsSetLocal(pool.bandBitmapFall, bandId)) {
      assert(groups.has(`${DIR_FALL}:${bandId}`), `I4 stale fall bit at band ${bandId}`);
    }
    if (bitmapIsSetLocal(pool.bandBitmapRise, bandId)) {
      assert(groups.has(`${DIR_RISE}:${bandId}`), `I4 stale rise bit at band ${bandId}`);
    }
  }

  // I5 — Accounted reserves stay positive (pool never becomes degenerate while
  // it holds liquidity), and the monotone borrow indexes never regress below WAD.
  const accA = realA - pool.totalCollateralA - pool.protocolFeesA + pool.totalDebtA;
  const accB = realB - pool.totalCollateralB - pool.protocolFeesB + pool.totalDebtB;
  assert(accA > 0n && accB > 0n, `I5 accounted reserves positive (a=${accA} b=${accB})`);
}

function bitmapIsSetLocal(bitmap: Buffer, bandId: number): boolean {
  if (bandId > 127) return false;
  return (bitmap[bandId >> 3] & (1 << (bandId & 7))) !== 0;
}

// ===== Main =====

async function run() {
  console.log('=== ChiefLiquidity Adversarial + Fuzzer E2E ===\n');

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
      console.log(`  ${e.message}`);
      if (e.logs) console.log(`  Logs: ${e.logs.slice(-6).join('\n        ')}`);
      failed++;
    }
  }
  const T = (name: string, fn: (ctx: Ctx) => Promise<void>) =>
    test(name, async () => {
      const ctx = new Ctx(connection, payer, tokenProgram);
      await ctx.setup();
      await fn(ctx);
    });

  console.log('---------- Liquidation-context substitution attacks ----------\n');

  // Helper: a pool with one CollateralA loan → fall band populated (count 1).
  async function poolWithFallLoan(ctx: Ctx) {
    await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    const borrower = await ctx.newUser(10, 100_000_000n, 0n);
    const r = await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);
    return { borrower, ...r };
  }

  await T('Attack: foreign loan (other pool) injected into a band → InvalidPool', async (ctx) => {
    const { bandId } = await poolWithFallLoan(ctx);
    // A second, unrelated pool with its own loan.
    const other = new Ctx(connection, payer, tokenProgram);
    await other.setup();
    const oBorrower = (await other.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n));
    void oBorrower;
    const ob = await other.newUser(10, 100_000_000n, 0n);
    const foreign = await other.openLoan(ob, COLL_A, 100_000_000n, 300_000_000n);

    const trader = await ctx.newUser(10, 200_000_000n, 0n);
    // Supply ctx's real band (count 1, k 1) but with the foreign loan account.
    const ix = rawSwapIx(ctx, trader.publicKey, 200_000_000n, 1n, true, 0,
      [{ bandPda: ctx.bandPda(DIR_FALL, bandId), loans: [foreign.loan] }]);
    await expectError(ctx.send(withCu(ix), [trader]), Err.InvalidPool, 'foreign loan');
  });

  await T('Attack: loan from a different band → InvalidLiquidationContext', async (ctx) => {
    await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    const borrower = await ctx.newUser(10, 200_000_000n, 0n);
    // Two loans, different trigger prices → different bands.
    const big = await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n); // ~3.3 → band 66
    const small = await ctx.openLoan(borrower, COLL_A, 100_000_000n, 150_000_000n); // ~1.65 → band 65
    assert(big.bandId !== small.bandId, 'distinct bands for setup');
    const trader = await ctx.newUser(10, 200_000_000n, 0n);
    // Supply band `big` (count 1, k 1) but hand over the `small`-band loan.
    const ix = rawSwapIx(ctx, trader.publicKey, 200_000_000n, 1n, true, 0,
      [{ bandPda: ctx.bandPda(DIR_FALL, big.bandId), loans: [small.loan] }]);
    await expectError(ctx.send(withCu(ix), [trader]),
      Err.InvalidLiquidationContext, 'wrong-band loan');
  });

  await T('Attack: non-program-owned fake loan → InvalidAccountOwner', async (ctx) => {
    const { bandId } = await poolWithFallLoan(ctx);
    // A 210-byte account owned by the System program (attacker-controlled data).
    const fake = Keypair.generate();
    const rent = await connection.getMinimumBalanceForRentExemption(LOAN_LEN);
    const ctx0 = new Transaction().add(SystemProgram.createAccount({
      fromPubkey: payer.publicKey, newAccountPubkey: fake.publicKey,
      lamports: rent, space: LOAN_LEN, programId: SystemProgram.programId,
    }));
    await sendAndConfirmTransaction(connection, ctx0, [payer, fake], { commitment: 'confirmed' });

    const trader = await ctx.newUser(10, 200_000_000n, 0n);
    const ix = rawSwapIx(ctx, trader.publicKey, 200_000_000n, 1n, true, 0,
      [{ bandPda: ctx.bandPda(DIR_FALL, bandId), loans: [fake.publicKey] }]);
    await expectError(ctx.send(withCu(ix), [trader]),
      Err.InvalidAccountOwner, 'fake loan account');
  });

  await T('Attack: band PDA from another pool → BandMismatch', async (ctx) => {
    await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    const other = new Ctx(connection, payer, tokenProgram);
    await other.setup();
    await other.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    const ob = await other.newUser(10, 100_000_000n, 0n);
    const foreign = await other.openLoan(ob, COLL_A, 100_000_000n, 300_000_000n);

    const trader = await ctx.newUser(10, 200_000_000n, 0n);
    // Hand ctx's swap the *other* pool's band PDA + loan.
    const ix = rawSwapIx(ctx, trader.publicKey, 200_000_000n, 1n, true, 0,
      [{ bandPda: other.bandPda(DIR_FALL, foreign.bandId), loans: [foreign.loan] }]);
    await expectError(ctx.send(withCu(ix), [trader]), Err.BandMismatch, 'foreign band');
  });

  await T('Attack: wrong-direction band (rise band in a fall swap) → BandMismatch', async (ctx) => {
    await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    const borrower = await ctx.newUser(10, 0n, 400_000_000n);
    const rise = await ctx.openLoan(borrower, COLL_B, 400_000_000n, 75_000_000n); // OnRise
    const trader = await ctx.newUser(10, 200_000_000n, 0n);
    // a_to_b (fall) swap, but supply the rise band.
    const ix = rawSwapIx(ctx, trader.publicKey, 200_000_000n, 1n, true, 0,
      [{ bandPda: ctx.bandPda(DIR_RISE, rise.bandId), loans: [rise.loan] }]);
    await expectError(ctx.send(withCu(ix), [trader]), Err.BandMismatch, 'wrong direction');
  });

  await T('Attack: same band supplied twice → InvalidLiquidationContext', async (ctx) => {
    const { loan, bandId } = await poolWithFallLoan(ctx);
    const trader = await ctx.newUser(10, 200_000_000n, 0n);
    const band = ctx.bandPda(DIR_FALL, bandId);
    const ix = rawSwapIx(ctx, trader.publicKey, 200_000_000n, 1n, true, 0,
      [{ bandPda: band, loans: [loan] }, { bandPda: band, loans: [loan] }]);
    await expectError(ctx.send(withCu(ix), [trader]),
      Err.InvalidLiquidationContext, 'duplicate band');
  });

  await T('Attack: extra trailing account in the tail → InvalidLiquidationContext', async (ctx) => {
    const { loan, bandId } = await poolWithFallLoan(ctx);
    const trader = await ctx.newUser(10, 200_000_000n, 0n);
    const ix = rawSwapIx(ctx, trader.publicKey, 200_000_000n, 1n, true, 0,
      [{ bandPda: ctx.bandPda(DIR_FALL, bandId), loans: [loan] }],
      [{ pubkey: payer.publicKey, isSigner: false, isWritable: false }]);
    await expectError(ctx.send(withCu(ix), [trader]),
      Err.InvalidLiquidationContext, 'trailing junk account');
  });

  await T('Attack: count claims more loans than supplied → InvalidLiquidationContext', async (ctx) => {
    const { loan, bandId } = await poolWithFallLoan(ctx);
    const trader = await ctx.newUser(10, 200_000_000n, 0n);
    // band_loan_counts says 2 for this band but only 1 loan account follows.
    const data = Buffer.alloc(1 + 8 + 8 + 1 + 4 + 4 + 1);
    let o = 0;
    data.writeUInt8(Ix.Swap, o); o += 1;
    data.writeBigUInt64LE(200_000_000n, o); o += 8;
    data.writeBigUInt64LE(1n, o); o += 8;
    data.writeUInt8(1, o); o += 1;          // a_to_b
    data.writeUInt32LE(0, o); o += 4;        // boundary
    data.writeUInt32LE(1, o); o += 4;        // one band entry
    data.writeUInt8(2, o); o += 1;           // …claiming count = 2
    const keys: AccountMeta[] = [
      { pubkey: ctx.pool, isSigner: false, isWritable: true },
      { pubkey: ctx.vaultA, isSigner: false, isWritable: true },
      { pubkey: ctx.vaultB, isSigner: false, isWritable: true },
      { pubkey: ctx.ata(trader.publicKey, ctx.mintA), isSigner: false, isWritable: true },
      { pubkey: ctx.ata(trader.publicKey, ctx.mintB), isSigner: false, isWritable: true },
      { pubkey: ctx.mintA, isSigner: false, isWritable: false },
      { pubkey: ctx.mintB, isSigner: false, isWritable: false },
      { pubkey: trader.publicKey, isSigner: true, isWritable: false },
      { pubkey: ctx.tokenProgram, isSigner: false, isWritable: false },
      { pubkey: ctx.bandPda(DIR_FALL, bandId), isSigner: false, isWritable: true },
      { pubkey: loan, isSigner: false, isWritable: true }, // only ONE loan, not two
    ];
    const ix = new TransactionInstruction({ programId: PROGRAM_ID, keys, data });
    await expectError(ctx.send(withCu(ix), [trader]),
      Err.InvalidLiquidationContext, 'count overruns tail');
  });

  await T('Attack: swap user not marked signer → MissingRequiredSigner', async (ctx) => {
    await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    const trader = await ctx.newUser(10, 100_000_000n, 0n);
    const ix = ctx.ixSwap(trader.publicKey, 1_000_000n, 1n, true, 0, []);
    ix.keys[7] = { pubkey: trader.publicKey, isSigner: false, isWritable: false };
    // Send signed only by payer; the program must reject on its own signer check.
    await expectError(ctx.send(withCu(ix), []), Err.MissingRequiredSigner, 'non-signer user');
  });

  await T('Attack: substituted foreign mint → InvalidPool', async (ctx) => {
    await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    const other = new Ctx(connection, payer, tokenProgram);
    await other.setup();
    const trader = await ctx.newUser(10, 100_000_000n, 0n);
    const ix = ctx.ixSwap(trader.publicKey, 1_000_000n, 1n, true, 0, []);
    ix.keys[5] = { pubkey: other.mintA, isSigner: false, isWritable: false };
    await expectError(ctx.send(withCu(ix), [trader]), Err.InvalidPool, 'foreign mint');
  });

  await T('Attack: repay with a foreign band PDA → BandMismatch', async (ctx) => {
    await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    const borrower = await ctx.newUser(10, 100_000_000n, 50_000_000n);
    const { loan } = await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);
    const st = (await ctx.loanState(loan))!;
    // Build a repay ix but swap in a band PDA for a DIFFERENT band id.
    const wrongBand = ctx.bandPda(st.triggerDirection, st.bandId + 1);
    const ix = new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: ctx.pool, isSigner: false, isWritable: true },
        { pubkey: ctx.vaultA, isSigner: false, isWritable: true },
        { pubkey: ctx.vaultB, isSigner: false, isWritable: true },
        { pubkey: ctx.ata(borrower.publicKey, ctx.mintA), isSigner: false, isWritable: true },
        { pubkey: ctx.ata(borrower.publicKey, ctx.mintB), isSigner: false, isWritable: true },
        { pubkey: ctx.mintA, isSigner: false, isWritable: false },
        { pubkey: ctx.mintB, isSigner: false, isWritable: false },
        { pubkey: borrower.publicKey, isSigner: true, isWritable: true },
        { pubkey: loan, isSigner: false, isWritable: true },
        { pubkey: wrongBand, isSigner: false, isWritable: true },
        { pubkey: ctx.tokenProgram, isSigner: false, isWritable: false },
      ],
      data: Buffer.from([Ix.RepayLoan]),
    });
    // Wrong band is uninitialized (no such PDA) → not program-owned.
    await expectError(ctx.send([ix], [borrower]),
      Err.InvalidAccountOwner, 'foreign repay band');
  });

  console.log('\n---------- Economic / rounding probes ----------\n');

  await T('Vault donation cannot enable LP over-withdrawal (solvency holds)', async (ctx) => {
    const lp = await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
    // A borrower earmarks collateral, then someone donates raw tokens to a vault.
    const borrower = await ctx.newUser(10, 200_000_000n, 0n);
    await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);
    const donor = await ctx.newUser(10, 500_000_000n, 0n);
    await transfer(connection, payer, ctx.ata(donor.publicKey, ctx.mintA), ctx.vaultA,
      donor, 500_000_000n, [], { commitment: 'confirmed' }, ctx.tokenProgram);
    // Donation must not corrupt earmark accounting; solvency floor still holds.
    await checkInvariants(ctx, connection);
    // The LP cannot withdraw the earmarked collateral; a full burn is bounded
    // by swappable reserves (reverts) rather than draining the vault.
    const lpBal = await ctx.tokenBalance(lp.publicKey, ctx.lpMint);
    await ctx.removeLiquidity(lp, lpBal, 1n, 1n).catch(() => {});
    await checkInvariants(ctx, connection);
    const { a } = await ctx.vaultBalances();
    const pool = await ctx.poolState();
    assert(a >= pool.totalCollateralA, 'collateral still fully backed after withdrawal');
  });

  await T('Inflation probe: second depositor still receives nonzero LP after a donation', async (ctx) => {
    await ctx.initializePool();
    const first = await ctx.newUser(10, 2_000_000n, 2_000_000n);
    await ctx.addLiquidity(first, 1_000_000n, 1_000_000n, 1n); // exactly MIN_FIRST_DEPOSIT
    // Donate 10x to skew the share price, the classic inflation setup.
    const donor = await ctx.newUser(10, 20_000_000n, 0n);
    await transfer(connection, payer, ctx.ata(donor.publicKey, ctx.mintA), ctx.vaultA,
      donor, 10_000_000n, [], { commitment: 'confirmed' }, ctx.tokenProgram);
    const victim = await ctx.newUser(10, 1_000_000_000n, 1_000_000_000n);
    await ctx.addLiquidity(victim, 1_000_000_000n, 1_000_000_000n, 1n);
    const lp = await ctx.tokenBalance(victim.publicKey, ctx.lpMint);
    assert(lp > 0n, `victim received LP (${lp}) — deposit not rounded to zero`);
    await checkInvariants(ctx, connection);
  });

  await T('Inflation via remove-to-dust + donation cannot steal a depositor', async (ctx) => {
    // The MIN_FIRST_DEPOSIT floor only gates the *first* deposit; it does NOT
    // permanently lock a minimum LP supply the way Uniswap-v2's burned
    // MINIMUM_LIQUIDITY does. So an attacker can satisfy the floor, then
    // RemoveLiquidity back down to a single LP unit and donate raw tokens to
    // inflate the share price — the classic first-depositor inflation setup,
    // reached here via remove (the existing probe above keeps full supply, so
    // it never exercises this path). The guarantee under test: a victim's
    // deposit can never be pocketed for zero LP.
    await ctx.initializePool();
    // One wallet plays the whole attack: seed capital + donation capital.
    const attacker = await ctx.newUser(10, 200_000_000n, 2_000_000n);
    await ctx.addLiquidity(attacker, 1_000_000n, 1_000_000n, 1n); // exactly MIN_FIRST_DEPOSIT
    assertEq(await ctx.lpSupply(), 1_000_000n, 'first deposit minted sqrt(1e6*1e6)');

    // Drain supply to a single LP unit — the step the existing probe omits.
    // Proves the supply floor is not permanent.
    await ctx.removeLiquidity(attacker, 999_999n, 1n, 1n);
    assertEq(await ctx.lpSupply(), 1n, 'supply driven to one LP unit via remove');
    await checkInvariants(ctx, connection);

    // Inflate the share price: donate raw tokens straight to the vault so the
    // lone LP unit now backs a huge A reserve (accounted_a ≫ accounted_b).
    await transfer(connection, payer, ctx.ata(attacker.publicKey, ctx.mintA), ctx.vaultA,
      attacker, 100_000_000n, [], { commitment: 'confirmed' }, ctx.tokenProgram);
    await checkInvariants(ctx, connection);

    // Victim makes an ordinary balanced deposit. With accounted_a ~1e8 against
    // supply=1, the proportional B side rounds to zero, so lp_to_mint is zero.
    // The program MUST reject this rather than transfer the victim's tokens for
    // no LP — that rejection is the entire anti-theft guarantee.
    const victim = await ctx.newUser(10, 1_000_000_000n, 1_000_000_000n);
    const aBefore = await ctx.tokenBalance(victim.publicKey, ctx.mintA);
    const bBefore = await ctx.tokenBalance(victim.publicKey, ctx.mintB);
    await expectError(ctx.addLiquidity(victim, 50_000_000n, 50_000_000n, 1n),
      Err.ZeroAmount, 'donation-skewed deposit rounds to zero LP → reverts');

    // The reverted tx committed nothing: no tokens left the victim.
    assertEq(await ctx.tokenBalance(victim.publicKey, ctx.mintA), aBefore, 'victim A untouched');
    assertEq(await ctx.tokenBalance(victim.publicKey, ctx.mintB), bBefore, 'victim B untouched');

    // The grief is unprofitable: burning the last LP unwinds the pool and hands
    // the inflated reserves back to the attacker, so they only ever locked their
    // own donated capital — nothing was extracted from the victim.
    await ctx.removeLiquidity(attacker, 1n, 1n, 1n);
    assertEq(await ctx.lpSupply(), 0n, 'attacker burns last LP, pool fully unwound');
    const { a, b } = await ctx.vaultBalances();
    assert(a === 0n && b === 0n, `pool drained to empty, no value stranded (a=${a} b=${b})`);
  });

  console.log('\n---------- Randomized invariant fuzzer ----------\n');

  await test('Fuzz: random op sequence preserves all invariants', async () => {
    const seed = process.env.FUZZ_SEED ? Number(process.env.FUZZ_SEED) : 0xC0FFEE;
    const iterations = process.env.FUZZ_ITERS ? Number(process.env.FUZZ_ITERS) : 40;
    const rng = mulberry32(seed);
    console.log(`    seed=0x${seed.toString(16)} iterations=${iterations}`);

    const ctx = new Ctx(connection, payer, tokenProgram);
    await ctx.setup();
    // A "house" LP seeds the pool and never participates in fuzz ops, so
    // accounted reserves stay positive throughout (I5).
    await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);

    // Fuzz actors.
    const users: Keypair[] = [];
    for (let i = 0; i < 4; i++) users.push(await ctx.newUser(20, 0n, 0n));
    const open: { loan: PublicKey; borrower: Keypair; side: number }[] = [];
    const log: string[] = [];

    const pick = <X>(arr: X[]) => arr[randInt(rng, 0, arr.length - 1)];
    const mint = (m: PublicKey, to: PublicKey, amt: bigint) =>
      mintTo(connection, payer, m, ctx.ata(to, m), payer, amt, [], { commitment: 'confirmed' }, tokenProgram);

    for (let i = 0; i < iterations; i++) {
      const op = randInt(rng, 0, 4);
      let did = '';
      try {
        if (op === 0) {
          // ADD LIQUIDITY
          const u = pick(users);
          const a = BigInt(randInt(rng, 2_000_000, 200_000_000));
          const b = BigInt(randInt(rng, 2_000_000, 200_000_000));
          await mint(ctx.mintA, u.publicKey, a);
          await mint(ctx.mintB, u.publicKey, b);
          await ctx.addLiquidity(u, a, b, 1n);
          did = `add a=${a} b=${b}`;
        } else if (op === 1) {
          // REMOVE LIQUIDITY
          const u = pick(users);
          const bal = await ctx.tokenBalance(u.publicKey, ctx.lpMint);
          if (bal > 0n) {
            const amt = bal / BigInt(randInt(rng, 1, 4));
            if (amt > 0n) { await ctx.removeLiquidity(u, amt, 1n, 1n); did = `remove lp=${amt}`; }
          }
        } else if (op === 2) {
          // SWAP (router builds the complete liquidation context)
          const u = pick(users);
          const aToB = rng() < 0.5;
          const amt = BigInt(randInt(rng, 1_000_000, 300_000_000));
          await mint(aToB ? ctx.mintA : ctx.mintB, u.publicKey, amt);
          await ctx.swap(u, amt, 1n, aToB);
          did = `swap ${aToB ? 'A→B' : 'B→A'} ${amt}`;
        } else if (op === 3) {
          // OPEN LOAN at a healthy LTV computed from live accounted reserves.
          const u = pick(users);
          const pool = await ctx.poolState();
          const { a: realA, b: realB } = await ctx.vaultBalances();
          const accA = realA - pool.totalCollateralA - pool.protocolFeesA + pool.totalDebtA;
          const accB = realB - pool.totalCollateralB - pool.protocolFeesB + pool.totalDebtB;
          if (accA <= 0n || accB <= 0n) { did = 'open skipped (degenerate)'; }
          else {
            const side = rng() < 0.5 ? COLL_A : COLL_B;
            const target = BigInt(randInt(rng, 5000, 7500)); // 50–75% LTV
            const collAcc = side === COLL_A ? accA : accB;
            const collateral = collAcc / BigInt(randInt(rng, 20, 100));
            const debt = side === COLL_A
              ? (target * collateral * accB) / (accA * BPS)
              : (target * collateral * accA) / (accB * BPS);
            const swappableDebt = side === COLL_A
              ? realB - pool.totalCollateralB - pool.protocolFeesB
              : realA - pool.totalCollateralA - pool.protocolFeesA;
            if (collateral > 0n && debt > 0n && debt <= (swappableDebt * 8n) / 10n) {
              await mint(side === COLL_A ? ctx.mintA : ctx.mintB, u.publicKey, collateral);
              const r = await ctx.openLoan(u, side, collateral, debt);
              open.push({ loan: r.loan, borrower: u, side });
              did = `open side=${side} coll=${collateral} debt=${debt}`;
            } else did = 'open skipped (params)';
          }
        } else {
          // REPAY a still-open tracked loan.
          if (open.length > 0) {
            const idx = randInt(rng, 0, open.length - 1);
            const entry = open[idx];
            const st = await ctx.loanState(entry.loan);
            if (st && st.status === 0) {
              const dmint = entry.side === COLL_A ? ctx.mintB : ctx.mintA;
              await mint(dmint, entry.borrower.publicKey, st.debtPrincipal * 2n + 1_000_000n);
              await ctx.repayLoan(entry.borrower, entry.loan);
              did = `repay ${entry.loan.toBase58().slice(0, 6)}`;
            }
            open.splice(idx, 1); // drop whether repaid or already gone
          }
        }
      } catch (e: any) {
        // A rejected tx commits nothing — fine. Record it and keep fuzzing.
        did = `${did || 'op' + op} REVERTED (${e.message?.split('\n')[0] ?? e})`;
      }

      log.push(`#${i} ${did}`);
      try {
        await checkInvariants(ctx, connection);
      } catch (inv: any) {
        const tail = log.slice(-12).join('\n      ');
        throw new Error(`INVARIANT VIOLATED after op #${i} (seed=0x${seed.toString(16)}):\n  ${inv.message}\n  recent ops:\n      ${tail}`);
      }
    }
    console.log(`    ${iterations} ops survived; ${open.length} loans left open at end`);
  });

  console.log(`\n=== Results: ${passed} passed, ${failed} failed ===`);
  process.exit(failed > 0 ? 1 : 0);
}

run().catch((e) => { console.error(e); process.exit(1); });
