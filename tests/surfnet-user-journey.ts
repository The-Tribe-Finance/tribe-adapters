/**
 * One user's full journey, on a surfnet mainnet fork, with REAL Jupiter.
 *
 *   1. the user gets USDC
 *   2. the user deposits into the vault  -> receives shares
 *   3. governance (the admin, for now) swaps vault USDC -> WSOL through Jupiter
 *   4. the user redeems -> gets back a pro-rata slice of BOTH assets, in kind
 *
 * Every transaction signature is printed so it can be inspected afterwards.
 *
 * # Two things that will bite you on a fork
 *
 * 1. **The oracle must agree with the market.** The vault rejects any action that loses
 *    more than MAX_SLIPPAGE_BPS of the value sent in, measured against Pyth. If the fake
 *    Pyth said SOL is $100 while Jupiter's pools price it at $77, the vault would see a
 *    23% loss and revert — correctly. So the fake price is derived from Jupiter's own
 *    quote, exactly as mainnet would have it: one market, one price.
 *
 * 2. **Fork-only route constraints.** Oracle-based AMMs (HumidiFi, Quantum, BisonFi)
 *    revert once the clock is jumped forward past the 7-day timelock — they read the
 *    clock to price. And multi-hop routes touch pools the fork has not lazily fetched.
 *    So the route is pinned to a single direct Raydium hop. Neither applies on mainnet.
 */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import * as anchor from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  Connection,
  LAMPORTS_PER_SOL,
  AccountMeta,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountInstruction,
  getAssociatedTokenAddressSync,
  AccountLayout,
} from "@solana/spl-token";
import { assert } from "chai";
import { encodePriceUpdate, healthyPrice, feedIdFor, PYTH_RECEIVER_PROGRAM_ID } from "./pyth";

const { BN, AnchorProvider, Program, Wallet } = anchor;

const RPC = "http://127.0.0.1:8899";
const JUP_API = "https://lite-api.jup.ag/swap/v1";
const JUPITER_PROGRAM = new PublicKey("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

const USDC = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const WSOL = new PublicKey("So11111111111111111111111111111111111111112");

const USDC_FEED = feedIdFor("usdc-feed");
const SOL_FEED = feedIdFor("sol-feed");
const ACTION_SWAP = 0;

const connection = new Connection(RPC, "confirmed");

/** The operator's wallet, as configured by `solana config get`. */
const KEYPAIR_PATH =
  process.env.SOLANA_KEYPAIR ?? path.join(os.homedir(), ".config", "solana", "id.json");

const admin = Keypair.fromSecretKey(
  Uint8Array.from(JSON.parse(fs.readFileSync(KEYPAIR_PATH, "utf8")))
);
const provider = new AnchorProvider(connection, new Wallet(admin), {
  commitment: "confirmed",
});
anchor.setProvider(provider);

const vaultProgram = new Program(
  JSON.parse(fs.readFileSync("target/idl/tribe_vault.json", "utf8")),
  provider
);
const adapterProgram = new Program(
  JSON.parse(fs.readFileSync("target/idl/adapter_swap.json", "utf8")),
  provider
);

// Deterministic fake-oracle addresses, so re-runs stay idempotent.
const usdcOracle = PublicKey.findProgramAddressSync(
  [Buffer.from("fake-oracle"), Buffer.from("usdc")],
  new PublicKey("11111111111111111111111111111111")
)[0];
const solOracle = PublicKey.findProgramAddressSync(
  [Buffer.from("fake-oracle"), Buffer.from("sol")],
  new PublicKey("11111111111111111111111111111111")
)[0];

// The user — a fresh wallet, not the admin.
const user = Keypair.generate();

const txs: { step: string; sig: string }[] = [];
const record = (step: string, sig: string) => {
  txs.push({ step, sig });
  console.log(`      tx: ${sig}`);
};

async function rpc(method: string, params: any[]) {
  const r = await fetch(RPC, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  const j = await r.json();
  if ((j as any).error) throw new Error(`${method} → ${JSON.stringify((j as any).error)}`);
  return (j as any).result;
}

/** The Clock SYSVAR — the exact timestamp `Clock::get()` returns on-chain. */
async function chainNow(): Promise<number> {
  const acc = await connection.getAccountInfo(anchor.web3.SYSVAR_CLOCK_PUBKEY);
  if (!acc) throw new Error("Clock sysvar not found");
  return Number(acc.data.readBigInt64LE(32));
}

async function setPrices(solPrice: number, ts?: number) {
  const t = BigInt(ts ?? (await chainNow()));
  for (const [addr, feed, price] of [
    [usdcOracle, USDC_FEED, 1],
    [solOracle, SOL_FEED, solPrice],
  ] as [PublicKey, Buffer, number][]) {
    await rpc("surfnet_setAccount", [
      addr.toBase58(),
      {
        lamports: LAMPORTS_PER_SOL,
        data: encodePriceUpdate(healthyPrice(feed, price, t)).toString("hex"),
        owner: PYTH_RECEIVER_PROGRAM_ID.toBase58(),
        executable: false,
      },
    ]);
  }
}

/** Jump the clock, then rewrite prices. Fail loud if it did not actually move. */
async function advanceClock(seconds: number, solPrice: number) {
  const before = await chainNow();
  const slot = await connection.getSlot();
  await rpc("surfnet_timeTravel", [{ absoluteSlot: slot + Math.floor(seconds / 0.4) }]);

  const after = await chainNow();
  if (after - before < seconds * 0.9) {
    throw new Error(`clock only advanced ${after - before}s, expected ~${seconds}s`);
  }
  await setPrices(solPrice, after);
}

const balanceOf = async (addr: PublicKey): Promise<bigint> => {
  const acc = await connection.getAccountInfo(addr);
  return acc ? AccountLayout.decode(acc.data).amount : 0n;
};

async function ataFor(mint: PublicKey, owner: PublicKey, payer: Keypair) {
  const ata = getAssociatedTokenAddressSync(mint, owner, true);
  if (await connection.getAccountInfo(ata)) return ata;
  const tx = new anchor.web3.Transaction().add(
    createAssociatedTokenAccountInstruction(payer.publicKey, ata, owner, mint)
  );
  await provider.sendAndConfirm(tx, [payer]);
  return ata;
}

const usdc = (n: bigint) => (Number(n) / 1e6).toLocaleString(undefined, { maximumFractionDigits: 2 });
const sol = (n: bigint) => (Number(n) / 1e9).toFixed(6);
const shares = (n: bigint) => (Number(n) / 1e6).toLocaleString(undefined, { maximumFractionDigits: 3 });

const banner = (s: string) =>
  console.log(`\n${"═".repeat(66)}\n  ${s}\n${"═".repeat(66)}`);

// ---------------------------------------------------------------------------

describe("user journey — deposit → swap → redeem (real Jupiter)", function () {
  this.timeout(600_000);

  it("a user deposits, governance swaps, the user redeems in kind", async () => {
    const [vaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault")],
      vaultProgram.programId
    );
    const [vaultAuthority] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority"), vaultPda.toBuffer()],
      vaultProgram.programId
    );
    const assetPda = (mint: PublicKey) =>
      PublicKey.findProgramAddressSync(
        [Buffer.from("asset"), vaultPda.toBuffer(), mint.toBuffer()],
        vaultProgram.programId
      )[0];
    const adapterPda = (pid: PublicKey) =>
      PublicKey.findProgramAddressSync(
        [Buffer.from("adapter"), vaultPda.toBuffer(), pid.toBuffer()],
        vaultProgram.programId
      )[0];
    const capabilityPda = (pid: PublicKey, action: number, mint: PublicKey) =>
      PublicKey.findProgramAddressSync(
        [
          Buffer.from("capability"),
          vaultPda.toBuffer(),
          pid.toBuffer(),
          Buffer.from([action]),
          mint.toBuffer(),
        ],
        vaultProgram.programId
      )[0];

    banner("SETUP");

    console.log(`  RPC          : ${RPC} (surfnet, mainnet fork)`);
    console.log(`  vault program: ${vaultProgram.programId.toBase58()}`);
    console.log(`  adapter      : ${adapterProgram.programId.toBase58()}`);
    console.log(`  Jupiter      : ${JUPITER_PROGRAM.toBase58()}  (real)`);
    console.log(`  admin        : ${admin.publicKey.toBase58()}`);
    console.log(`  USER         : ${user.publicKey.toBase58()}  (fresh wallet)`);

    // Ask Jupiter what SOL actually costs, and price the oracle to match.
    const quote = await (
      await fetch(
        `${JUP_API}/quote?inputMint=${USDC}&outputMint=${WSOL}` +
          `&amount=1000000000&slippageBps=9900&dexes=Raydium&onlyDirectRoutes=true`
      )
    ).json();

    const solPrice = 1000 / (Number(quote.outAmount) / 1e9);
    console.log(`\n  market SOL price (from Jupiter): $${solPrice.toFixed(2)}`);
    await setPrices(solPrice);

    // Vault, assets, adapter, capability.
    if (!(await connection.getAccountInfo(vaultPda))) {
      const shareMint = Keypair.generate();
      await vaultProgram.methods
        .initializeVault()
        .accounts({
          admin: admin.publicKey,
          treasury: admin.publicKey,
          shareMint: shareMint.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([shareMint])
        .rpc();
    }
    const vault0 = await vaultProgram.account.vault.fetch(vaultPda);
    const shareMintKey: PublicKey = vault0.shareMint;

    const vaultAtas: Record<string, PublicKey> = {};
    for (const [mint, feed, oracle] of [
      [USDC, USDC_FEED, usdcOracle],
      [WSOL, SOL_FEED, solOracle],
    ] as [PublicKey, Buffer, PublicKey][]) {
      const ata = getAssociatedTokenAddressSync(mint, vaultAuthority, true);
      vaultAtas[mint.toBase58()] = ata;
      if (await connection.getAccountInfo(assetPda(mint))) continue;

      await vaultProgram.methods
        .registerAsset(Array.from(feed), new BN(0))
        .accounts({
          admin: admin.publicKey,
          mint,
          vaultTokenAccount: ata,
          oracle,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .rpc();
    }

    if (!(await connection.getAccountInfo(adapterPda(adapterProgram.programId)))) {
      await vaultProgram.methods
        .registerAdapter(adapterProgram.programId, { action: {} }, "jupiter")
        .accounts({ admin: admin.publicKey })
        .rpc();
      await advanceClock(8 * 24 * 3600, solPrice);
    }

    const capPda = capabilityPda(adapterProgram.programId, ACTION_SWAP, USDC);
    if (!(await connection.getAccountInfo(capPda))) {
      await vaultProgram.methods
        .registerCapability(ACTION_SWAP, USDC, true, PublicKey.default, new BN(0))
        .accounts({
          admin: admin.publicKey,
          adapter: adapterPda(adapterProgram.programId),
          receiptAsset: null,
          capability: capPda,
        })
        .rpc();
      await advanceClock(8 * 24 * 3600, solPrice);
    }

    console.log(`  vault        : ${vaultPda.toBase58()}`);
    console.log(`  share mint   : ${shareMintKey.toBase58()}`);

    const vaultUsdc = vaultAtas[USDC.toBase58()];
    const vaultWsol = vaultAtas[WSOL.toBase58()];

    // -----------------------------------------------------------------------

    banner("STEP 1 — the user gets 5,000 USDC");

    await rpc("surfnet_setAccount", [
      user.publicKey.toBase58(),
      { lamports: 10 * LAMPORTS_PER_SOL },
    ]);
    await rpc("surfnet_setTokenAccount", [
      user.publicKey.toBase58(),
      USDC.toBase58(),
      { amount: 5_000_000_000 },
    ]);

    const userUsdc = getAssociatedTokenAddressSync(USDC, user.publicKey);
    console.log(`   user USDC account: ${userUsdc.toBase58()}`);
    console.log(`   balance          : ${usdc(await balanceOf(userUsdc))} USDC`);

    // -----------------------------------------------------------------------

    banner("STEP 2 — the user DEPOSITS 5,000 USDC into the vault");

    const userShares = await ataFor(shareMintKey, user.publicKey, admin);

    const navAccounts = async (): Promise<AccountMeta[]> => {
      const v = await vaultProgram.account.vault.fetch(vaultPda);
      const out: AccountMeta[] = [];
      for (const mint of v.assetMints) {
        const a = await vaultProgram.account.asset.fetch(assetPda(mint));
        out.push({ pubkey: assetPda(mint), isSigner: false, isWritable: false });
        out.push({ pubkey: a.tokenAccount, isSigner: false, isWritable: false });
        out.push({ pubkey: a.oracle, isSigner: false, isWritable: false });
      }
      return out;
    };

    await setPrices(solPrice); // must be fresh at the moment the instruction runs

    const depositSig = await vaultProgram.methods
      .deposit(new BN(5_000_000_000))
      .accounts({
        depositor: user.publicKey,
        depositMint: USDC,
        depositorTokenAccount: userUsdc,
        vaultTokenAccount: vaultUsdc,
        shareMint: shareMintKey,
        depositorShareAccount: userShares,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .remainingAccounts(await navAccounts())
      .signers([user])
      .rpc();

    const gotShares = await balanceOf(userShares);
    const vault1 = await vaultProgram.account.vault.fetch(vaultPda);

    console.log(`   user paid    : 5,000 USDC`);
    console.log(`   user received: ${shares(gotShares)} shares`);
    console.log(`   vault total  : ${shares(BigInt(vault1.totalShares.toString()))} shares`);
    console.log(`   difference   : ${Number(vault1.totalShares) - Number(gotShares)} = MINIMUM_LIQUIDITY, locked forever`);
    console.log(`   vault holds  : ${usdc(await balanceOf(vaultUsdc))} USDC`);
    record("deposit", depositSig);

    // -----------------------------------------------------------------------

    banner("STEP 3 — governance SWAPS 1,000 vault USDC → WSOL via real Jupiter");

    const AMOUNT_IN = 1_000_000_000n;

    const swapIx = await (
      await fetch(`${JUP_API}/swap-instructions`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          quoteResponse: quote,
          userPublicKey: vaultAuthority.toBase58(), // the vault's PDA IS the Jupiter user
          wrapAndUnwrapSol: false,
        }),
      })
    ).json();

    if (swapIx.error) throw new Error(`Jupiter: ${swapIx.error}`);

    // Strip every signer flag. A PDA cannot sign a transaction — it signs a CPI, from
    // inside the program that owns it. The VAULT re-adds the flag, for vault_authority
    // and nothing else. A client cannot smuggle a signature through.
    const jupAccounts: AccountMeta[] = swapIx.swapInstruction.accounts.map((a: any) => ({
      pubkey: new PublicKey(a.pubkey),
      isSigner: false,
      isWritable: a.isWritable,
    }));
    const jupData = Buffer.from(swapIx.swapInstruction.data, "base64");

    const adapterAccounts: AccountMeta[] = [
      { pubkey: vaultAuthority, isSigner: false, isWritable: false },
      { pubkey: vaultWsol, isSigner: false, isWritable: true },
      { pubkey: JUPITER_PROGRAM, isSigner: false, isWritable: false },
      ...jupAccounts,
    ];

    const meterAccounts = async (): Promise<AccountMeta[]> => {
      const v = await vaultProgram.account.vault.fetch(vaultPda);
      const out: AccountMeta[] = [];
      for (const mint of v.assetMints) {
        const a = await vaultProgram.account.asset.fetch(assetPda(mint));
        out.push({ pubkey: assetPda(mint), isSigner: false, isWritable: false });
        out.push({ pubkey: a.tokenAccount, isSigner: false, isWritable: false });
      }
      out.push({ pubkey: usdcOracle, isSigner: false, isWritable: false });
      out.push({ pubkey: solOracle, isSigner: false, isWritable: false });
      return out;
    };

    // adapter_swap::swap(amount_in, min_out, dex_payload)
    const disc = createHash("sha256").update("global:swap").digest().subarray(0, 8);
    const head = Buffer.alloc(16);
    head.writeBigUInt64LE(AMOUNT_IN, 0);
    head.writeBigUInt64LE(0n, 8); // min_out = 0 — let the VAULT's guard be the one that bites
    const len = Buffer.alloc(4);
    len.writeUInt32LE(jupData.length, 0);
    const adapterPayload = Buffer.concat([disc, head, len, jupData]);

    await setPrices(solPrice);

    const usdcBefore = await balanceOf(vaultUsdc);
    const wsolBefore = await balanceOf(vaultWsol);

    const ix = await vaultProgram.methods
      .executeAction(
        ACTION_SWAP,
        new BN(AMOUNT_IN.toString()),
        Array.from(SOL_FEED),
        adapterPayload,
      )
      .accounts({
        authority: admin.publicKey, // = vault.executor. Tier 3: a governance PDA.
        capability: capPda,
        adapter: adapterPda(adapterProgram.programId),
        adapterProgram: adapterProgram.programId,
        assetIn: assetPda(USDC),
        assetOut: assetPda(WSOL),
        outMint: WSOL,
        vaultOutTokenAccount: getAssociatedTokenAddressSync(WSOL, vaultAuthority, true),
        outOracle: solOracle,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .remainingAccounts([...(await meterAccounts()), ...adapterAccounts])
      .instruction();

    // Versioned transaction + Address Lookup Tables. A legacy transaction does not fit:
    // measured 1237 bytes against a 1232-byte limit.
    const luts = (
      await Promise.all(
        ((swapIx.addressLookupTableAddresses ?? []) as string[]).map((a) =>
          connection.getAddressLookupTable(new PublicKey(a))
        )
      )
    )
      .map((r) => r.value)
      .filter((t): t is anchor.web3.AddressLookupTableAccount => t !== null);

    const { blockhash } = await connection.getLatestBlockhash();
    const vtx = new anchor.web3.VersionedTransaction(
      new anchor.web3.TransactionMessage({
        payerKey: admin.publicKey,
        recentBlockhash: blockhash,
        instructions: [
          anchor.web3.ComputeBudgetProgram.setComputeUnitLimit({ units: 1_000_000 }),
          ix,
        ],
      }).compileToV0Message(luts)
    );
    vtx.sign([admin]);

    const swapSig = await connection.sendTransaction(vtx, { skipPreflight: true });
    await connection.confirmTransaction(swapSig, "confirmed");

    const usdcAfter = await balanceOf(vaultUsdc);
    const wsolAfter = await balanceOf(vaultWsol);
    const spent = usdcBefore - usdcAfter;
    const received = wsolAfter - wsolBefore;

    const swapTx = await connection.getTransaction(swapSig, {
      maxSupportedTransactionVersion: 0,
    });

    console.log(`   route        : ${quote.routePlan.map((r: any) => r.swapInfo.label).join(" + ")}`);
    console.log(`   vault USDC   : ${usdc(usdcBefore)} → ${usdc(usdcAfter)}   (spent ${usdc(spent)})`);
    console.log(`   vault WSOL   : ${sol(wsolBefore)} → ${sol(wsolAfter)}   (received ${sol(received)})`);
    console.log(`   fill price   : $${((Number(spent) / 1e6) / (Number(received) / 1e9)).toFixed(2)}/SOL  (oracle: $${solPrice.toFixed(2)})`);
    console.log(`   compute      : ${swapTx?.meta?.computeUnitsConsumed?.toLocaleString()} CU`);
    console.log(`   tx size      : ${vtx.serialize().length} bytes`);
    record("execute_action (swap via Jupiter)", swapSig);

    assert.isAtMost(Number(spent), Number(AMOUNT_IN), "must not spend more than amount_in");
    assert.isAbove(Number(received), 0, "must have received WSOL");

    // -----------------------------------------------------------------------

    banner("STEP 4 — the user REDEEMS everything, in kind");

    // The vault now holds BOTH assets. The user gets a pro-rata slice of EACH — not a
    // cash-out. That is what "in-kind redemption" means.

    const redeemAccounts = async (): Promise<AccountMeta[]> => {
      const v = await vaultProgram.account.vault.fetch(vaultPda);
      const out: AccountMeta[] = [];
      for (const mint of v.assetMints) {
        const a = await vaultProgram.account.asset.fetch(assetPda(mint));
        out.push({ pubkey: assetPda(mint), isSigner: false, isWritable: true });
        out.push({ pubkey: a.tokenAccount, isSigner: false, isWritable: false });
      }
      return out;
    };

    const vaultBefore = await vaultProgram.account.vault.fetch(vaultPda);
    const ticketId = BigInt(vaultBefore.redeemTicketCounter.toString());

    // NOTE: redeem needs NO oracle. It is pure pro-rata arithmetic.
    const redeemSig = await vaultProgram.methods
      .redeemRequest(new BN(gotShares.toString()))
      .accounts({
        owner: user.publicKey,
        shareMint: shareMintKey,
        ownerShareAccount: userShares,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .remainingAccounts(await redeemAccounts())
      .signers([user])
      .rpc();

    const [ticketPda] = PublicKey.findProgramAddressSync(
      [
        Buffer.from("redeem_ticket"),
        vaultPda.toBuffer(),
        user.publicKey.toBuffer(),
        new BN(ticketId.toString()).toArrayLike(Buffer, "le", 8),
      ],
      vaultProgram.programId
    );
    const ticket = await vaultProgram.account.redeemTicket.fetch(ticketPda);

    console.log(`   burned       : ${shares(gotShares)} shares`);
    console.log(`   ticket owes  : ${usdc(BigInt(ticket.amounts[0].toString()))} USDC + ${sol(BigInt(ticket.amounts[1].toString()))} WSOL`);
    record("redeem_request (burn shares, lock in what is owed)", redeemSig);

    // `reserved` now locks those amounts — no adapter can spend them.
    for (const [i, mint] of [USDC, WSOL].entries()) {
      const a = await vaultProgram.account.asset.fetch(assetPda(mint));
      console.log(`   reserved[${i}]  : ${a.reserved.toString()} (locked against execute_action)`);
    }

    // Claim each asset.
    const userWsol = await ataFor(WSOL, user.publicKey, admin);
    const userUsdcBefore = await balanceOf(userUsdc);

    for (const [i, mint] of [USDC, WSOL].entries()) {
      const amt = BigInt(ticket.amounts[i].toString());
      if (amt === 0n) continue;

      const claimSig = await vaultProgram.methods
        .claimAsset(i)
        .accounts({
          owner: user.publicKey,
          ticket: ticketPda,
          mint,
          vaultTokenAccount: vaultAtas[mint.toBase58()],
          ownerTokenAccount: mint.equals(USDC) ? userUsdc : userWsol,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([user])
        .rpc();

      console.log(`   claimed asset ${i} (${mint.equals(USDC) ? "USDC" : "WSOL"})`);
      record(`claim_asset(${i})`, claimSig);
    }

    const closeSig = await vaultProgram.methods
      .closeTicket()
      .accounts({ owner: user.publicKey, ticket: ticketPda })
      .signers([user])
      .rpc();
    record("close_ticket (rent reclaimed)", closeSig);

    // `reserved` must be back to zero, or it would creep up and slowly freeze the vault.
    for (const mint of [USDC, WSOL]) {
      const a = await vaultProgram.account.asset.fetch(assetPda(mint));
      assert.equal(Number(a.reserved), 0, `reserved must return to 0 for ${mint.toBase58()}`);
    }

    // -----------------------------------------------------------------------

    banner("RESULT");

    const finalUsdc = (await balanceOf(userUsdc)) - userUsdcBefore;
    const finalWsol = await balanceOf(userWsol);
    const valueOut = Number(finalUsdc) / 1e6 + (Number(finalWsol) / 1e9) * solPrice;

    console.log(`  The user deposited 5,000 USDC and got back, IN KIND:`);
    console.log(`     ${usdc(finalUsdc)} USDC`);
    console.log(`   + ${sol(finalWsol)} WSOL   (worth ~$${((Number(finalWsol) / 1e9) * solPrice).toFixed(2)})`);
    console.log(`   ────────────────────────────`);
    console.log(`     ≈ $${valueOut.toFixed(2)} total\n`);
    console.log(`  Not a cash-out — a pro-rata slice of EVERY asset the vault held.`);
    console.log(`  The vault had swapped part of its USDC into SOL, so the user's share`);
    console.log(`  came back as both.`);

    console.log(`\n${"─".repeat(66)}\n  TRANSACTIONS\n${"─".repeat(66)}`);
    for (const { step, sig } of txs) {
      console.log(`\n  ${step}`);
      console.log(`  ${sig}`);
    }
    console.log();
  });
});
