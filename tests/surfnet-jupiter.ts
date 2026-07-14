/**
 * `execute_action` against the REAL Jupiter, on a surfnet mainnet fork.
 *
 *     tribe-vault  ──►  adapter-swap  ──►  Jupiter (the real program)
 *     (verifies)        (knows swaps)      (real routes, real pools)
 *
 * This is the test that `test-adapter` cannot be: test-adapter is a fixture I wrote, with an
 * account layout I chose. It proves the vault's guards work, but it proves nothing about
 * whether the adapter actually fits Jupiter — I built both ends of that conversation.
 *
 * # Two things that will bite you here
 *
 * 1. **The oracle price must match the market.** The vault rejects any action that loses
 *    more than MAX_SLIPPAGE_BPS of the value sent in, measured against Pyth. If the fake
 *    Pyth says SOL is $100 while Jupiter's pools price it at $77, the vault sees a 23%
 *    loss and reverts — correctly. So the fake price is derived from Jupiter's own quote,
 *    exactly as mainnet would have it: the oracle and the DEX reflect one market.
 *
 * 2. **The quote is computed at mainnet's latest slot, but surfnet sits at an older one.**
 *    Pool state differs, so the real fill drifts from the quote and Jupiter reverts with
 *    SlippageToleranceExceeded (0x1771). We widen Jupiter's OWN slippage to absorb that.
 *    The VAULT's slippage bound is untouched — that is the guard we are actually testing.
 *
 * # Why the route is pinned to a single direct Raydium hop
 *
 * Neither constraint is about the vault. Both are properties of testing on a fork:
 *
 * - **Oracle-based AMMs reject a time-travelled clock.** HumidiFi, Quantum and BisonFi
 *   revert with 0x1771 once the clock has been jumped forward to clear the 7-day timelock,
 *   even with slippage widened to 99%. Their pool state on the fork is byte-identical to
 *   mainnet — they read the clock to price. Classic constant-product AMMs do not.
 *
 * - **Multi-hop routes touch pools the fork has not lazily fetched yet**, and Jupiter fails
 *   account validation (0x1789). `onlyDirectRoutes` keeps it to one hop.
 *
 * On mainnet neither applies — the clock is real and every account exists.
 */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import * as anchor from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  Connection,
  SystemProgram,
  LAMPORTS_PER_SOL,
  AccountMeta,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  MINT_SIZE,
  createInitializeMint2Instruction,
  createAssociatedTokenAccountInstruction,
  createMintToInstruction,
  getAssociatedTokenAddressSync,
  AccountLayout,
} from "@solana/spl-token";
import { assert } from "chai";
import { encodePriceUpdate, healthyPrice, feedIdFor, PYTH_RECEIVER_PROGRAM_ID } from "./pyth";

const { BN, AnchorProvider, Program, Wallet } = anchor;

const RPC = "http://127.0.0.1:8899";
const JUP_API = "https://lite-api.jup.ag/swap/v1";
const JUPITER_PROGRAM = new PublicKey("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

// Real mainnet mints — surfnet forks them, so Jupiter's pools recognize them.
const USDC = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const WSOL = new PublicKey("So11111111111111111111111111111111111111112");

const USDC_FEED = feedIdFor("usdc-feed");
const SOL_FEED = feedIdFor("sol-feed");
const ACTION_SWAP = 0;

const connection = new Connection(RPC, "confirmed");

/** The operator's wallet, as configured by `solana config get`. */
const KEYPAIR_PATH =
  process.env.SOLANA_KEYPAIR ??
  path.join(os.homedir(), ".config", "solana", "id.json");

const payer = Keypair.fromSecretKey(
  Uint8Array.from(JSON.parse(fs.readFileSync(KEYPAIR_PATH, "utf8")))
);
const provider = new AnchorProvider(connection, new Wallet(payer), {
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

// DETERMINISTIC oracle addresses — not random.
//
// The vault stores each asset's oracle address at registration time. Generating fresh
// keypairs each run means a re-run registers nothing (the asset already exists) but then
// passes brand-new oracle accounts, and the vault correctly rejects them with
// OracleFeedMismatch. Deriving them from a fixed seed makes re-runs idempotent.
const usdcOracle = PublicKey.findProgramAddressSync(
  [Buffer.from("fake-oracle"), Buffer.from("usdc")],
  new PublicKey("11111111111111111111111111111111")
)[0];
const solOracle = PublicKey.findProgramAddressSync(
  [Buffer.from("fake-oracle"), Buffer.from("sol")],
  new PublicKey("11111111111111111111111111111111")
)[0];

async function rpc(method: string, params: any[]) {
  const r = await fetch(RPC, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  const j = await r.json();
  if (j.error) throw new Error(`${method} → ${JSON.stringify(j.error)}`);
  return j.result;
}

/** The Clock SYSVAR — the exact timestamp `Clock::get()` returns on-chain. */
async function chainNow(): Promise<number> {
  const acc = await connection.getAccountInfo(anchor.web3.SYSVAR_CLOCK_PUBKEY);
  if (!acc) throw new Error("Clock sysvar not found");
  return Number(acc.data.readBigInt64LE(32));
}

/** Write fake Pyth accounts. `solPrice` comes from Jupiter's own quote — see the header. */
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

/**
 * Jump the clock forward, then rewrite prices.
 *
 * Travel by SLOT: `surfnet_timeTravel` accepts an `absoluteTimestamp` variant, but it
 * errors internally. `absoluteSlot` works, and `unix_timestamp` follows at ~0.4s/slot.
 *
 * Fail LOUD if the clock did not move — silently swallowing that is what made a broken
 * timelock look like a broken program for several rounds.
 */
async function advanceClock(seconds: number, solPrice: number) {
  const before = await chainNow();
  const slot = await connection.getSlot();
  await rpc("surfnet_timeTravel", [
    { absoluteSlot: slot + Math.floor(seconds / 0.4) },
  ]);

  const after = await chainNow();
  if (after - before < seconds * 0.9) {
    throw new Error(`clock only advanced ${after - before}s, expected ~${seconds}s`);
  }
  await setPrices(solPrice, after);
}

async function balanceOf(addr: PublicKey): Promise<bigint> {
  const acc = await connection.getAccountInfo(addr);
  return acc ? AccountLayout.decode(acc.data).amount : 0n;
}

async function createAta(mint: PublicKey, owner: PublicKey) {
  const ata = getAssociatedTokenAddressSync(mint, owner, true);
  if (await connection.getAccountInfo(ata)) return ata;
  const tx = new anchor.web3.Transaction().add(
    createAssociatedTokenAccountInstruction(payer.publicKey, ata, owner, mint)
  );
  await provider.sendAndConfirm(tx, []);
  return ata;
}

// ---------------------------------------------------------------------------

describe("execute_action → REAL Jupiter (surfnet mainnet fork)", function () {
  this.timeout(600_000);

  it("vault → adapter-swap → Jupiter, with the vault's guards enforced", async () => {
    console.log(`wallet : ${payer.publicKey.toBase58()}`);
    console.log(`vault  : ${vaultProgram.programId.toBase58()}`);
    console.log(`adapter: ${adapterProgram.programId.toBase58()}`);
    console.log(`jupiter: ${JUPITER_PROGRAM.toBase58()}  (real mainnet program)\n`);

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

    // --- 1. Ask Jupiter what the market actually says ---

    const AMOUNT_IN = 1_000_000_000n; // 1,000 USDC

    const quote = await (
      await fetch(
        `${JUP_API}/quote?inputMint=${USDC}&outputMint=${WSOL}` +
          `&amount=${AMOUNT_IN}&slippageBps=9900&dexes=Raydium&onlyDirectRoutes=true`
      )
    ).json();

    const solOut = Number(quote.outAmount) / 1e9;
    const solPrice = 1000 / solOut; // USD per SOL, per Jupiter's own pools

    console.log(`── Jupiter quote ──`);
    console.log(`   1,000 USDC → ${solOut.toFixed(4)} SOL`);
    console.log(`   implied SOL price: $${solPrice.toFixed(2)}`);
    console.log(`   route: ${quote.routePlan.map((r: any) => r.swapInfo.label).join(" + ")}\n`);

    // --- 2. Set up the vault, with oracles that agree with the market ---

    await setPrices(solPrice);

    let vaultAcc = await connection.getAccountInfo(vaultPda);
    let shareMintKey: PublicKey;

    if (vaultAcc) {
      const v = await vaultProgram.account.vault.fetch(vaultPda);
      shareMintKey = v.shareMint;
      console.log("vault already exists — reusing\n");
    } else {
      const shareMint = Keypair.generate();
      await vaultProgram.methods
        .initializeVault()
        .accounts({
          admin: payer.publicKey,
          treasury: payer.publicKey,
          shareMint: shareMint.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([shareMint])
        .rpc();
      shareMintKey = shareMint.publicKey;
    }

    // Register USDC and WSOL — the REAL mints, the ones Jupiter's pools trade.
    const vaultAtas: Record<string, PublicKey> = {};
    for (const [mint, feed, oracle, name] of [
      [USDC, USDC_FEED, usdcOracle, "USDC"],
      [WSOL, SOL_FEED, solOracle, "WSOL"],
    ] as [PublicKey, Buffer, PublicKey, string][]) {
      if (await connection.getAccountInfo(assetPda(mint))) {
        const a = await vaultProgram.account.asset.fetch(assetPda(mint));
        vaultAtas[mint.toBase58()] = a.tokenAccount;
        continue;
      }
      // The vault's token account MUST be the canonical ATA of vault_authority.
      //
      // This is not cosmetic. Jupiter (and every other DEX) derives the user's token
      // accounts as ATAs and hard-codes those addresses into the instruction it builds.
      // Register the asset with a random keypair account instead, and Jupiter passes
      // addresses the vault does not recognize — it fails account validation with 0x1789
      // before a single lamport moves.
      //
      // So: a vault that wants to route through real DEXes has no choice. Its token
      // accounts have to live where the rest of Solana expects to find them.
      const ata = getAssociatedTokenAddressSync(mint, vaultAuthority, true);

      // `register_asset` uses `init`, so the ATA must not exist beforehand.
      if (await connection.getAccountInfo(ata)) {
        throw new Error(`ATA ${ata.toBase58()} already exists — reset the surfnet`);
      }

      await vaultProgram.methods
        .registerAsset(Array.from(feed), new BN(0))
        .accounts({
          admin: payer.publicKey,
          mint,
          vaultTokenAccount: ata,
          oracle,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .rpc();
      vaultAtas[mint.toBase58()] = ata;
      console.log(`   registered ${name} — ATA ${ata.toBase58().slice(0, 8)}…`);
    }

    // --- 3. Fund the vault by DEPOSITING — the honest path ---
    //
    // Do NOT write tokens straight into the vault's account with a cheatcode: that would
    // leave the vault holding value with no shares backing it, which is exactly the
    // broken state the NAV math is designed to reject.

    const vaultUsdc = vaultAtas[USDC.toBase58()];
    const vaultWsol = vaultAtas[WSOL.toBase58()];

    // The vault needs total_shares > 0 — NAV math rejects a vault with outstanding value
    // but zero shares. Deposit properly: it is the only honest way to create shares, and
    // it exercises the deposit path against the real USDC mint at the same time.
    const v0 = await vaultProgram.account.vault.fetch(vaultPda);
    if (v0.totalShares.isZero()) {
      // Give the operator some USDC to deposit with.
      await rpc("surfnet_setTokenAccount", [
        payer.publicKey.toBase58(),
        USDC.toBase58(),
        { amount: 20_000_000_000 },
      ]);
      const myUsdc = getAssociatedTokenAddressSync(USDC, payer.publicKey);
      const myShares = await createAta(shareMintKey, payer.publicKey);

      await setPrices(solPrice);

      await vaultProgram.methods
        .deposit(new BN(10_000_000_000)) // 10,000 USDC
        .accounts({
          depositor: payer.publicKey,
          depositMint: USDC,
          depositorTokenAccount: myUsdc,
          vaultTokenAccount: vaultUsdc,
          shareMint: shareMintKey,
          depositorShareAccount: myShares,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .remainingAccounts(
          await (async () => {
            const v = await vaultProgram.account.vault.fetch(vaultPda);
            const out: AccountMeta[] = [];
            for (const mint of v.assetMints) {
              const a = await vaultProgram.account.asset.fetch(assetPda(mint));
              out.push({ pubkey: assetPda(mint), isSigner: false, isWritable: false });
              out.push({ pubkey: a.tokenAccount, isSigner: false, isWritable: false });
              out.push({ pubkey: a.oracle, isSigner: false, isWritable: false });
            }
            return out;
          })()
        )
        .rpc();

      const v1 = await vaultProgram.account.vault.fetch(vaultPda);
      console.log(`   deposited 10,000 USDC → ${Number(v1.totalShares) / 1e6} shares`);
    }

    // --- 4. Register Jupiter as an Action adapter, via adapter-swap ---

    if (!(await connection.getAccountInfo(adapterPda(adapterProgram.programId)))) {
      await vaultProgram.methods
        .registerAdapter(adapterProgram.programId, { action: {} }, "jupiter")
        .accounts({ admin: payer.publicKey })
        .rpc();
      await advanceClock(8 * 24 * 3600, solPrice);
      console.log("   adapter registered + timelock elapsed");
    }

    const capPda = capabilityPda(adapterProgram.programId, ACTION_SWAP, USDC);
    if (!(await connection.getAccountInfo(capPda))) {
      await vaultProgram.methods
        .registerCapability(ACTION_SWAP, true, PublicKey.default, new BN(0))
        .accounts({
          admin: payer.publicKey,
          adapter: adapterPda(adapterProgram.programId),
          asset: assetPda(USDC),
          receiptAsset: null,
          capability: capPda,
        })
        .rpc();
      await advanceClock(8 * 24 * 3600, solPrice);
      console.log("   capability registered + timelock elapsed");
    }

    // --- 5. Get Jupiter's real instruction, keyed to the VAULT's PDA ---

    const swapIx = await (
      await fetch(`${JUP_API}/swap-instructions`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          quoteResponse: quote,
          // Jupiter builds the instruction for THIS authority. It is the vault's PDA —
          // and Jupiter accepts it, requiring exactly one signer.
          userPublicKey: vaultAuthority.toBase58(),
          wrapAndUnwrapSol: false,
          useSharedAccounts: false,
        }),
      })
    ).json();

    if (swapIx.error) throw new Error(`Jupiter: ${swapIx.error}`);

    // Jupiter marks vault_authority as a signer — correct for Jupiter, but WRONG at the
    // transaction level. A PDA cannot sign a transaction; it signs a CPI, via
    // `invoke_signed`, from inside the program that owns it.
    //
    // So strip every signer flag here. The VAULT re-adds it, for vault_authority and
    // nothing else, when it builds the CPI. That is the whole point of the guard:
    //
    //     is_signer: key == authority_key
    //
    // The client cannot smuggle a signature through by claiming an account is a signer —
    // the vault decides, and it only ever decides in favor of its own PDA.
    const rawSigners = swapIx.swapInstruction.accounts.filter((a: any) => a.isSigner);

    const jupAccounts: AccountMeta[] = swapIx.swapInstruction.accounts.map((a: any) => ({
      pubkey: new PublicKey(a.pubkey),
      isSigner: false, // the vault decides who signs, not us
      isWritable: a.isWritable,
    }));
    const jupData = Buffer.from(swapIx.swapInstruction.data, "base64");

    const signers = rawSigners.map((a: any) => ({
      pubkey: new PublicKey(a.pubkey),
    }));
    console.log(`\n── Jupiter instruction ──`);
    console.log(`   accounts : ${jupAccounts.length}`);
    console.log(`   payload  : ${jupData.length} bytes`);
    console.log(`   Jupiter wants ${signers.length} signer(s): ${signers.map((s: any) => s.pubkey.toBase58().slice(0, 8)).join(", ")}`);
    console.log(
      `   and it is vault_authority? ${signers.length === 1 && signers[0].pubkey.equals(vaultAuthority) ? "✅ YES" : "❌ NO"}`
    );

    // --- 6. execute_action ---
    //
    // The account list handed to the adapter:
    //   [0..3)  adapter's own Swap struct
    //   [3..]   Jupiter's accounts, forwarded verbatim
    //
    // The vault imposes no ordering — it forwards what the client built, and signs only
    // for vault_authority.
    const adapterAccounts: AccountMeta[] = [
      { pubkey: vaultAuthority, isSigner: false, isWritable: false },
      { pubkey: vaultWsol, isSigner: false, isWritable: true },
      { pubkey: JUPITER_PROGRAM, isSigner: false, isWritable: false },
      ...jupAccounts,
    ];

    /** Pairs (Asset, token account) for all assets + the two oracles. */
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
    const { createHash } = await import("node:crypto");
    const disc = createHash("sha256").update("global:swap").digest().subarray(0, 8);
    const head = Buffer.alloc(16);
    head.writeBigUInt64LE(AMOUNT_IN, 0);
    head.writeBigUInt64LE(0n, 8); // min_out = 0: let the VAULT's guard be the one that bites
    const len = Buffer.alloc(4);
    len.writeUInt32LE(jupData.length, 0);
    const adapterPayload = Buffer.concat([disc, head, len, jupData]);

    await setPrices(solPrice); // fresh, right before the instruction runs

    const usdcBefore = await balanceOf(vaultUsdc);
    const wsolBefore = await balanceOf(vaultWsol);

    console.log(`\n── execute_action ──`);
    console.log(`   vault USDC before: ${Number(usdcBefore) / 1e6}`);
    console.log(`   vault WSOL before: ${Number(wsolBefore) / 1e9}`);

    // --- Versioned transaction + Address Lookup Tables ---
    //
    // A legacy transaction CANNOT carry this. Measured: 1237 bytes against a 1232-byte
    // limit — over by 5. The account list is simply too long: the vault's metering
    // accounts, plus the adapter's, plus Jupiter's 29.
    //
    // ALTs compress each account reference from 32 bytes to 1 byte. Jupiter hands back
    // the tables its own route needs (`addressLookupTableAddresses`), which is exactly
    // the set that blows the budget.
    //
    // This is not a workaround — it is the only way a real vault with real assets can
    // route through a real DEX. Any production client must do this.
    const ix = await vaultProgram.methods
      .executeAction(ACTION_SWAP, new BN(AMOUNT_IN.toString()), adapterPayload)
      .accounts({
        authority: payer.publicKey,
        capability: capPda,
        adapter: adapterPda(adapterProgram.programId),
        adapterProgram: adapterProgram.programId,
        assetIn: assetPda(USDC),
        assetOut: assetPda(WSOL),
      })
      .remainingAccounts([...(await meterAccounts()), ...adapterAccounts])
      .instruction();

    const altAddrs: string[] = swapIx.addressLookupTableAddresses ?? [];
    const luts = (
      await Promise.all(
        altAddrs.map((a) => connection.getAddressLookupTable(new PublicKey(a)))
      )
    )
      .map((r) => r.value)
      .filter((t): t is anchor.web3.AddressLookupTableAccount => t !== null);

    console.log(`   lookup tables: ${luts.length}`);

    const { blockhash } = await connection.getLatestBlockhash();
    const msg = new anchor.web3.TransactionMessage({
      payerKey: payer.publicKey,
      recentBlockhash: blockhash,
      instructions: [
        anchor.web3.ComputeBudgetProgram.setComputeUnitLimit({ units: 1_000_000 }),
        ix,
      ],
    }).compileToV0Message(luts);

    const vtx = new anchor.web3.VersionedTransaction(msg);
    vtx.sign([payer]);

    console.log(`   tx size: ${vtx.serialize().length} bytes (limit 1232)`);

    const sig = await connection.sendTransaction(vtx, { skipPreflight: true });
    await connection.confirmTransaction(sig, "confirmed");

    const usdcAfter = await balanceOf(vaultUsdc);
    const wsolAfter = await balanceOf(vaultWsol);

    console.log(`   vault USDC after : ${Number(usdcAfter) / 1e6}`);
    console.log(`   vault WSOL after : ${Number(wsolAfter) / 1e9}`);
    console.log(`   tx: ${sig}`);

    const tx = await connection.getTransaction(sig, { maxSupportedTransactionVersion: 0 });
    console.log(`   compute units: ${tx?.meta?.computeUnitsConsumed?.toLocaleString()}`);

    // --- Assertions ---

    const spent = usdcBefore - usdcAfter;
    const received = wsolAfter - wsolBefore;

    assert.isAbove(Number(received), 0, "the vault must have received WSOL");
    assert.isAtMost(Number(spent), Number(AMOUNT_IN), "must not spend more than amount_in");

    const impliedPrice = (Number(spent) / 1e6) / (Number(received) / 1e9);
    console.log(`\n   spent    : ${Number(spent) / 1e6} USDC`);
    console.log(`   received : ${Number(received) / 1e9} WSOL`);
    console.log(`   fill price: $${impliedPrice.toFixed(2)} / SOL  (oracle said $${solPrice.toFixed(2)})`);

    // The vault's own guard: it would have reverted had the fill lost more than 1% of the
    // value sent in, measured against the oracle. It did not — so the fill was fair.
    console.log(`\n✅ REAL Jupiter swap executed through the vault's guards`);
  });
});
