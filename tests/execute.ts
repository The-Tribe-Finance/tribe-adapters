import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { startAnchor, BanksClient, ProgramTestContext, Clock } from "solana-bankrun";
import { BankrunProvider } from "anchor-bankrun";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  LAMPORTS_PER_SOL,
  AccountMeta,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { assert } from "chai";

import { TribeVault } from "../target/types/tribe_vault";
import { TestAdapter } from "../target/types/test_adapter";
import { AdapterSwap } from "../target/types/adapter_swap";
import { createMint, createAta, mintTo, tokenBalance, ata } from "./spl";
import {
  PYTH_RECEIVER_PROGRAM_ID,
  encodePriceUpdate,
  healthyPrice,
  feedIdFor,
} from "./pyth";

const USDC_DECIMALS = 6;
const SOL_DECIMALS = 9;
const USDC_FEED = feedIdFor("usdc-feed");
const SOL_FEED = feedIdFor("sol-feed");
const USDC_PRICE = 1;
const SOL_PRICE = 100;

/// adapter-swap's action_id. The vault does NOT understand what this number means — it
/// only uses it as a seed for the Capability. The semantics "0 = swap" are the
/// ADAPTER's convention.
const ACTION_SWAP = 0;

/**
 * Tests for `execute_action` — where money leaves the vault.
 *
 * The architecture's real CPI chain:
 *
 *     tribe-vault  ──►  adapter-swap  ──►  test-adapter
 *     (verifies)        (knows swaps)      (a fixture that plays Jupiter)
 *
 * The questions this file answers:
 *
 *   1. Can the vault verify the result WITHOUT understanding swaps?
 *   2. Does the NAV-delta layer stop an adapter that misbehaves?
 *
 * `test-adapter` is deliberately capable of doing bad things — spending more than it is
 * allowed to, returning less than it promised, taking money and returning nothing.
 * Each test is one concrete attack.
 */
describe("execute_action — vault → adapter → dex", () => {
  let ctx: ProgramTestContext;
  let provider: BankrunProvider;
  let vaultProgram: Program<TribeVault>;
  let dexProgram: Program<TestAdapter>;
  let adapterProgram: Program<AdapterSwap>;
  let client: BanksClient;

  let admin: Keypair;
  let alice: Keypair;

  let usdcMint: PublicKey;
  let solMint: PublicKey;
  let vaultPda: PublicKey;
  let vaultAuthority: PublicKey;
  let shareMint: Keypair;
  let usdcOracle: PublicKey;
  let solOracle: PublicKey;

  let poolAuthority: PublicKey;
  let poolUsdc: PublicKey;
  let poolSol: PublicKey;

  let now: bigint;

  const assetPda = (mint: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("asset"), vaultPda.toBuffer(), mint.toBuffer()],
      vaultProgram.programId
    )[0];

  const adapterPda = (programId: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("adapter"), vaultPda.toBuffer(), programId.toBuffer()],
      vaultProgram.programId
    )[0];

  const capabilityPda = (programId: PublicKey, actionId: number, mint: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [
        Buffer.from("capability"),
        vaultPda.toBuffer(),
        programId.toBuffer(),
        Buffer.from([actionId]),
        mint.toBuffer(),
      ],
      vaultProgram.programId
    )[0];

  const setPrice = (
    address: PublicKey,
    feedId: Buffer,
    priceUsd: number,
    publishTime: bigint = now
  ) => {
    ctx.setAccount(address, {
      lamports: LAMPORTS_PER_SOL,
      data: encodePriceUpdate(healthyPrice(feedId, priceUsd, publishTime)),
      owner: PYTH_RECEIVER_PROGRAM_ID,
      executable: false,
    });
  };

  const advanceClock = async (seconds: bigint) => {
    const clock = await client.getClock();
    now = clock.unixTimestamp + seconds;
    ctx.setClock(
      new Clock(
        clock.slot + 1000n,
        clock.epochStartTimestamp,
        clock.epoch,
        clock.leaderScheduleEpoch,
        now
      )
    );
    setPrice(usdcOracle, USDC_FEED, USDC_PRICE);
    setPrice(solOracle, SOL_FEED, SOL_PRICE);
  };

  const vaultTokenAccount = async (mint: PublicKey) =>
    (await vaultProgram.account.asset.fetch(assetPda(mint))).tokenAccount;

  /// The triple (Asset, token account, oracle) for EVERY asset — used by deposit and
  /// assert_exposure, the places that NEED a complete NAV.
  const navAccounts = async (): Promise<AccountMeta[]> => {
    const vault = await vaultProgram.account.vault.fetch(vaultPda);
    const out: AccountMeta[] = [];
    for (const mint of vault.assetMints) {
      const a = await vaultProgram.account.asset.fetch(assetPda(mint));
      out.push({ pubkey: assetPda(mint), isSigner: false, isWritable: false });
      out.push({ pubkey: a.tokenAccount, isSigner: false, isWritable: false });
      out.push({ pubkey: a.oracle, isSigner: false, isWritable: false });
    }
    return out;
  };

  /**
   * Accounts for `execute_action` — a THREE-region layout.
   *
   *   [0, 2N)     (Asset, token) pairs for EVERY asset -> measure BALANCES (cheap)
   *   [2N, 2N+2)  oracle_in, oracle_out                -> measure VALUE (expensive)
   *   [2N+2, ..)  the adapter's accounts
   *
   * Why we do not pass an oracle for every asset: Jupiter *requests* the 1.4M CU
   * ceiling — Solana's maximum — regardless of how complex the route is. It actually
   * *consumes* far less (measured: ~72k CU for a 1-hop route, ~508k for a 3-hop one),
   * so there is real compute budget left. But that headroom varies with the route and
   * cannot be relied on, so we keep the vault's per-execute cost bounded: price only
   * the two assets that actually change, and merely check balances for the rest,
   * instead of pricing 24 assets × 2 passes.
   */
  const meterAccounts = async (): Promise<AccountMeta[]> => {
    const vault = await vaultProgram.account.vault.fetch(vaultPda);
    const out: AccountMeta[] = [];
    for (const mint of vault.assetMints) {
      const a = await vaultProgram.account.asset.fetch(assetPda(mint));
      out.push({ pubkey: assetPda(mint), isSigner: false, isWritable: false });
      out.push({ pubkey: a.tokenAccount, isSigner: false, isWritable: false });
    }
    out.push({ pubkey: usdcOracle, isSigner: false, isWritable: false });
    out.push({ pubkey: solOracle, isSigner: false, isWritable: false });
    return out;
  };

  /// The PAIR (Asset, token account) — redeem needs no oracle.
  const redeemAccounts = async (): Promise<AccountMeta[]> => {
    const vault = await vaultProgram.account.vault.fetch(vaultPda);
    const out: AccountMeta[] = [];
    for (const mint of vault.assetMints) {
      const a = await vaultProgram.account.asset.fetch(assetPda(mint));
      out.push({ pubkey: assetPda(mint), isSigner: false, isWritable: true });
      out.push({ pubkey: a.tokenAccount, isSigner: false, isWritable: false });
    }
    return out;
  };

  /**
   * The account list the VAULT sends to the ADAPTER.
   *
   * The first three accounts belong to the adapter (its `Swap` struct); the rest are
   * forwarded by the adapter down to the DEX. The vault neither understands nor imposes
   * this ordering — the client builds it, the vault forwards it verbatim, and only signs
   * for exactly vault_authority.
   */
  const adapterAccounts = async (): Promise<AccountMeta[]> => [
    // --- the adapter's Swap struct ---
    { pubkey: vaultAuthority, isSigner: false, isWritable: false },
    { pubkey: await vaultTokenAccount(solMint), isSigner: false, isWritable: true },
    { pubkey: dexProgram.programId, isSigner: false, isWritable: false },
    // --- the adapter's remaining_accounts = the DEX's account list ---
    { pubkey: vaultAuthority, isSigner: false, isWritable: false },
    { pubkey: await vaultTokenAccount(usdcMint), isSigner: false, isWritable: true },
    { pubkey: await vaultTokenAccount(solMint), isSigner: false, isWritable: true },
    { pubkey: poolAuthority, isSigner: false, isWritable: false },
    { pubkey: poolUsdc, isSigner: false, isWritable: true },
    { pubkey: poolSol, isSigner: false, isWritable: true },
    { pubkey: usdcMint, isSigner: false, isWritable: false },
    { pubkey: solMint, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
  ];

  const disc = (name: string) => {
    const h = require("crypto").createHash("sha256");
    h.update(`global:${name}`);
    return h.digest().subarray(0, 8);
  };

  /// Payload for test_adapter::swap(amount_in, amount_out).
  const dexPayload = (amountIn: bigint, amountOut: bigint): Buffer => {
    const buf = Buffer.alloc(16);
    buf.writeBigUInt64LE(amountIn, 0);
    buf.writeBigUInt64LE(amountOut, 8);
    return Buffer.concat([disc("swap"), buf]);
  };

  /// Payload for adapter_swap::swap(amount_in, min_out, dex_payload).
  const adapterPayload = (
    amountIn: bigint,
    minOut: bigint,
    dexData: Buffer
  ): Buffer => {
    const head = Buffer.alloc(16);
    head.writeBigUInt64LE(amountIn, 0);
    head.writeBigUInt64LE(minOut, 8);
    const len = Buffer.alloc(4);
    len.writeUInt32LE(dexData.length, 0);
    return Buffer.concat([disc("swap"), head, len, dexData]);
  };

  /**
   * Call vault.execute_action.
   *
   * `dexIn`/`dexOut` are what the DEX ACTUALLY does — which is what lets the fixture
   * misbehave. `minOut` is what the adapter DEMANDS.
   */
  const doExecute = async (
    amountIn: bigint,
    minOut: bigint,
    dexIn: bigint,
    dexOut: bigint
  ) =>
    vaultProgram.methods
      .executeAction(
        ACTION_SWAP,
        new BN(amountIn.toString()),
        adapterPayload(amountIn, minOut, dexPayload(dexIn, dexOut))
      )
      .accounts({
        authority: admin.publicKey,
        capability: capabilityPda(adapterProgram.programId, ACTION_SWAP, usdcMint),
        adapter: adapterPda(adapterProgram.programId),
        adapterProgram: adapterProgram.programId,
        assetIn: assetPda(usdcMint),
        assetOut: assetPda(solMint),
      })
      .remainingAccounts([...(await meterAccounts()), ...(await adapterAccounts())])
      .signers([admin])
      .rpc();

  /// An "honest" swap at the Pyth price: 1000 USDC ($1000) → 10 SOL ($100/SOL).
  const fairOut = (usdcIn: bigint): bigint =>
    (usdcIn * BigInt(1e9) * BigInt(USDC_PRICE)) / BigInt(1e6) / BigInt(SOL_PRICE);

  before(async () => {
    admin = Keypair.generate();
    alice = Keypair.generate();

    ctx = await startAnchor(
      ".",
      [],
      [admin, alice].map((kp) => ({
        address: kp.publicKey,
        info: {
          lamports: 100 * LAMPORTS_PER_SOL,
          data: Buffer.alloc(0),
          owner: SystemProgram.programId,
          executable: false,
        },
      }))
    );

    provider = new BankrunProvider(ctx);
    anchor.setProvider(provider);
    vaultProgram = anchor.workspace.tribeVault as Program<TribeVault>;
    dexProgram = anchor.workspace.testAdapter as Program<TestAdapter>;
    adapterProgram = anchor.workspace.adapterSwap as Program<AdapterSwap>;
    client = ctx.banksClient;
    now = (await client.getClock()).unixTimestamp;

    usdcMint = await createMint(provider, admin, USDC_DECIMALS);
    solMint = await createMint(provider, admin, SOL_DECIMALS);

    usdcOracle = Keypair.generate().publicKey;
    solOracle = Keypair.generate().publicKey;
    setPrice(usdcOracle, USDC_FEED, USDC_PRICE);
    setPrice(solOracle, SOL_FEED, SOL_PRICE);

    [vaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault")],
      vaultProgram.programId
    );
    [vaultAuthority] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority"), vaultPda.toBuffer()],
      vaultProgram.programId
    );
    shareMint = Keypair.generate();

    await vaultProgram.methods
      .initializeVault()
      .accounts({
        admin: admin.publicKey,
        treasury: admin.publicKey,
        shareMint: shareMint.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([admin, shareMint])
      .rpc();

    for (const [mint, feed, oracle] of [
      [usdcMint, USDC_FEED, usdcOracle],
      [solMint, SOL_FEED, solOracle],
    ] as const) {
      // Must be the canonical ATA of vault_authority — see tests/surfnet-jupiter.ts.
      await vaultProgram.methods
        .registerAsset(Array.from(feed), new BN(0))
        .accounts({
          admin: admin.publicKey,
          mint,
          vaultTokenAccount: ata(mint, vaultAuthority),
          oracle,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([admin])
        .rpc();
    }

    // Deposit 10k USDC into the vault.
    const aliceUsdc = await createAta(provider, alice, usdcMint, alice.publicKey);
    await mintTo(provider, admin, usdcMint, aliceUsdc, 10_000_000_000n);
    const aliceShares = await createAta(
      provider,
      alice,
      shareMint.publicKey,
      alice.publicKey
    );

    await vaultProgram.methods
      .deposit(new BN(10_000_000_000))
      .accounts({
        depositor: alice.publicKey,
        depositMint: usdcMint,
        depositorTokenAccount: aliceUsdc,
        vaultTokenAccount: await vaultTokenAccount(usdcMint),
        shareMint: shareMint.publicKey,
        depositorShareAccount: aliceShares,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .remainingAccounts(await navAccounts())
      .signers([alice])
      .rpc();

    // The test adapter's liquidity pool.
    [poolAuthority] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool")],
      dexProgram.programId
    );
    poolUsdc = await createAta(provider, admin, usdcMint, poolAuthority);
    poolSol = await createAta(provider, admin, solMint, poolAuthority);
    await mintTo(provider, admin, solMint, poolSol, 1_000_000_000_000n);

    // --- Register ADAPTER-SWAP (not the DEX!) as the Action adapter ---
    //
    // The vault CPIs into the adapter, and the adapter CPIs into the DEX. The vault has
    // no idea that any DEX exists.
    await vaultProgram.methods
      .registerAdapter(adapterProgram.programId, { action: {} }, "swap")
      .accounts({ admin: admin.publicKey })
      .signers([admin])
      .rpc();

    await advanceClock(BigInt(8 * 24 * 3600));

    // is_entry = true: a swap is an ENTRY (buying a new asset) → pause can block it.
    await vaultProgram.methods
      .registerCapability(ACTION_SWAP, true, PublicKey.default, new BN(0))
      .accounts({
        admin: admin.publicKey,
        adapter: adapterPda(adapterProgram.programId),
        asset: assetPda(usdcMint),
        receiptAsset: null,
        capability: capabilityPda(adapterProgram.programId, ACTION_SWAP, usdcMint),
      })
      .signers([admin])
      .rpc();

    await advanceClock(BigInt(8 * 24 * 3600));
  });

  // -------------------------------------------------------------------------

  it("a swap succeeds through the vault → adapter → dex chain", async () => {
    const vaultUsdc = await vaultTokenAccount(usdcMint);
    const vaultSol = await vaultTokenAccount(solMint);
    const usdcBefore = await tokenBalance(provider, vaultUsdc);
    const solBefore = await tokenBalance(provider, vaultSol);

    const amountIn = 1_000_000_000n; // 1000 USDC
    const out = fairOut(amountIn); // 10 SOL at the Pyth price

    await doExecute(amountIn, out, amountIn, out);

    assert.equal(await tokenBalance(provider, vaultUsdc), usdcBefore - amountIn);
    assert.equal(await tokenBalance(provider, vaultSol), solBefore + out);
  });

  it("🛡 NAV-delta stops an adapter that TAKES THE MONEY AND RETURNS NOTHING", async () => {
    // The DEX takes 1000 USDC and returns 0 SOL. The vault loses value → NAV drops →
    // revert.
    //
    // The key point: the vault does NOT need to know this is a swap in order to catch
    // it. All it sees is "my value dropped more than allowed" — and that is an AGNOSTIC
    // check, valid for every action, including actions that do not exist yet.
    const vaultUsdc = await vaultTokenAccount(usdcMint);
    const before = await tokenBalance(provider, vaultUsdc);

    let rejected = false;
    try {
      await doExecute(1_000_000_000n, 0n, 1_000_000_000n, 0n);
    } catch {
      rejected = true;
    }

    assert.isTrue(rejected, "must revert — the vault lost value");
    assert.equal(
      await tokenBalance(provider, vaultUsdc),
      before,
      "the balance must NOT change — the whole transaction reverts"
    );
  });

  it("🛡 NAV-delta stops a swap that LOSES MORE THAN THE SLIPPAGE ALLOWANCE (>1%)", async () => {
    const vaultUsdc = await vaultTokenAccount(usdcMint);
    const before = await tokenBalance(provider, vaultUsdc);

    const amountIn = 1_000_000_000n;
    const fair = fairOut(amountIn);
    const bad = (fair * 95n) / 100n; // only returns 95% → a 5% loss

    let rejected = false;
    try {
      await doExecute(amountIn, 0n, amountIn, bad); // min_out = 0: the adapter lets it through
    } catch {
      rejected = true;
    }

    assert.isTrue(
      rejected,
      "must revert with ValueLost — even though the ADAPTER allowed it (min_out = 0)"
    );
    assert.equal(await tokenBalance(provider, vaultUsdc), before);
  });

  it("a swap within the slippage allowance (<1%) still goes through", async () => {
    const vaultSol = await vaultTokenAccount(solMint);
    const before = await tokenBalance(provider, vaultSol);

    const amountIn = 1_000_000_000n;
    const fair = fairOut(amountIn);
    const ok = (fair * 995n) / 1000n; // a 0.5% loss — within the 1% allowance

    await doExecute(amountIn, ok, amountIn, ok);
    assert.equal(await tokenBalance(provider, vaultSol), before + ok);
  });

  it("🛡 the adapter blocks it itself when the DEX returns less than its min_out", async () => {
    // The ADAPTER's defense layer (not the vault's). It is redundant from a safety
    // standpoint — the vault would catch it via NAV-delta — but it produces the error
    // message in the right place.
    const amountIn = 1_000_000_000n;
    const fair = fairOut(amountIn);

    let rejected = false;
    try {
      // the adapter demands `fair`, the DEX only returns 90%.
      await doExecute(amountIn, fair, amountIn, (fair * 90n) / 100n);
    } catch {
      rejected = true;
    }

    assert.isTrue(rejected, "the adapter must revert with SlippageExceeded");
  });

  it("🔒 reserved: the adapter CANNOT spend what has been promised to redeemers", async () => {
    // The most serious hole this code has ever had.
    //
    // The scenario (before `reserved` existed):
    //   1. Alice calls redeem_request → shares are burned, the vault locks in "we owe
    //      Alice X USDC"
    //   2. Alice has not claimed yet (24 assets take several transactions)
    //   3. execute swaps away ALL of the USDC — "valid" according to every other check
    //   4. Alice claims → the vault no longer has enough → FAIL. And her shares are
    //      already burned.

    const vaultUsdc = await vaultTokenAccount(usdcMint);
    const usdcBalance = await tokenBalance(provider, vaultUsdc);

    const aliceShares = ata(shareMint.publicKey, alice.publicKey);
    const shareBalance = await tokenBalance(provider, aliceShares);

    await vaultProgram.methods
      .redeemRequest(new BN((shareBalance / 2n).toString()))
      .accounts({
        owner: alice.publicKey,
        shareMint: shareMint.publicKey,
        ownerShareAccount: aliceShares,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .remainingAccounts(await redeemAccounts())
      .signers([alice])
      .rpc();

    const assetIn = await vaultProgram.account.asset.fetch(assetPda(usdcMint));
    const reserved = BigInt(assetIn.reserved.toString());
    assert.isAbove(Number(reserved), 0, "redeem_request must lock the promised amount");

    // The tokens are STILL fully in the vault — nobody has claimed yet.
    assert.equal(
      await tokenBalance(provider, vaultUsdc),
      usdcBalance,
      "the tokens have not left the vault, they are only locked on the books"
    );

    const available = usdcBalance - reserved;

    // THE ATTACK: try to swap more than the available portion.
    // The PHYSICAL balance is still enough — only `reserved` stops it.
    let rejected = false;
    try {
      const amt = available + 1n;
      await doExecute(amt, fairOut(amt), amt, fairOut(amt));
    } catch {
      rejected = true;
    }

    assert.isTrue(
      rejected,
      "must revert with InsufficientAvailableBalance — even though the physical balance is sufficient"
    );

    // But a swap within the available portion STILL RUNS — reserved does not freeze the
    // vault.
    const half = available / 2n;
    await doExecute(half, fairOut(half), half, fairOut(half));

    // And Alice can STILL claim exactly her share — which is the whole point.
    const aliceUsdc = ata(usdcMint, alice.publicKey);
    const beforeClaim = await tokenBalance(provider, aliceUsdc);

    const ticketAddr = PublicKey.findProgramAddressSync(
      [
        Buffer.from("redeem_ticket"),
        vaultPda.toBuffer(),
        alice.publicKey.toBuffer(),
        new BN(0).toArrayLike(Buffer, "le", 8),
      ],
      vaultProgram.programId
    )[0];

    await vaultProgram.methods
      .claimAsset(0)
      .accounts({
        owner: alice.publicKey,
        ticket: ticketAddr,
        mint: usdcMint,
        vaultTokenAccount: vaultUsdc,
        ownerTokenAccount: aliceUsdc,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([alice])
      .rpc();

    assert.equal(
      await tokenBalance(provider, aliceUsdc),
      beforeClaim + reserved,
      "Alice receives exactly what was promised — even after the vault has swapped"
    );

    const after = await vaultProgram.account.asset.fetch(assetPda(usdcMint));
    assert.equal(
      BigInt(after.reserved.toString()),
      0n,
      "reserved goes back to 0 after the claim — otherwise it grows unbounded and locks the vault dead"
    );
  });

  it("🛡 pause blocks ENTRIES (is_entry = true)", async () => {
    await vaultProgram.methods
      .setPaused(true)
      .accounts({ admin: admin.publicKey })
      .signers([admin])
      .rpc();

    let err = "";
    try {
      const amt = 100_000_000n;
      await doExecute(amt, fairOut(amt), amt, fairOut(amt));
    } catch (e: any) {
      err = e.toString();
    }

    // The vault does not know that action 0 is a "swap". It reads the `is_entry` flag
    // that governance set when the capability was registered — that is how it knows this
    // is an entry without understanding the semantics.
    assert.include(err, "VaultPaused", "an entry must be blocked by pause");

    await vaultProgram.methods
      .setPaused(false)
      .accounts({ admin: admin.publicKey })
      .signers([admin])
      .rpc();
  });
});
