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
import { MockDex } from "../target/types/mock_dex";
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

/// action_id của adapter-swap. Vault KHÔNG hiểu con số này nghĩa là gì — nó chỉ
/// dùng nó làm seed của Capability. Ngữ nghĩa "0 = swap" là quy ước của ADAPTER.
const ACTION_SWAP = 0;

/**
 * Test cho `execute_action` — nơi tiền rời khỏi vault.
 *
 * Chuỗi CPI thật của kiến trúc:
 *
 *     tribe-vault  ──►  adapter-swap  ──►  mock-dex
 *     (kiểm chứng)      (biết swap)        (đóng vai Jupiter)
 *
 * Câu hỏi mà file này trả lời:
 *
 *   1. Vault có kiểm chứng được kết quả MÀ KHÔNG cần hiểu swap không?
 *   2. Lớp NAV-delta có chặn được adapter gian lận không?
 *
 * `mock-dex` cố ý làm được những việc xấu — tiêu quá phần được phép, trả về ít hơn
 * cam kết, lấy tiền không trả gì. Mỗi test là một đòn tấn công cụ thể.
 */
describe("execute_action — vault → adapter → dex", () => {
  let ctx: ProgramTestContext;
  let provider: BankrunProvider;
  let vaultProgram: Program<TribeVault>;
  let dexProgram: Program<MockDex>;
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

  /// Bộ ba (Asset, token account, oracle) cho MỌI asset — vùng ĐẦU của
  /// remaining_accounts. Vault dùng để tính NAV trước VÀ sau CPI.
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

  /// CẶP (Asset, token account) — redeem không cần oracle.
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
   * Account list mà VAULT gửi cho ADAPTER.
   *
   * Ba account đầu là của adapter (`Swap` struct), phần còn lại adapter chuyển tiếp
   * xuống DEX. Vault không hiểu và không áp đặt thứ tự này — client dựng, vault
   * forward nguyên vẹn, và chỉ ký cho đúng vault_authority.
   */
  const adapterAccounts = async (): Promise<AccountMeta[]> => [
    // --- Swap struct của adapter ---
    { pubkey: vaultAuthority, isSigner: false, isWritable: false },
    { pubkey: await vaultTokenAccount(solMint), isSigner: false, isWritable: true },
    { pubkey: dexProgram.programId, isSigner: false, isWritable: false },
    // --- remaining_accounts của adapter = account list của DEX ---
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

  /// Payload cho mock_dex::swap(amount_in, amount_out).
  const dexPayload = (amountIn: bigint, amountOut: bigint): Buffer => {
    const buf = Buffer.alloc(16);
    buf.writeBigUInt64LE(amountIn, 0);
    buf.writeBigUInt64LE(amountOut, 8);
    return Buffer.concat([disc("swap"), buf]);
  };

  /// Payload cho adapter_swap::swap(amount_in, min_out, dex_payload).
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
   * Gọi vault.execute_action.
   *
   * `dexIn`/`dexOut` là những gì DEX THẬT SỰ làm — cho phép mock gian lận.
   * `minOut` là những gì adapter YÊU CẦU.
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
      })
      .remainingAccounts([...(await navAccounts()), ...(await adapterAccounts())])
      .signers([admin])
      .rpc();

  /// Swap "trung thực" theo giá Pyth: 1000 USDC ($1000) → 10 SOL ($100/SOL).
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
    dexProgram = anchor.workspace.mockDex as Program<MockDex>;
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
      const vaultAta = Keypair.generate();
      await vaultProgram.methods
        .registerAsset(Array.from(feed), new BN(0))
        .accounts({
          admin: admin.publicKey,
          mint,
          vaultTokenAccount: vaultAta.publicKey,
          oracle,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([admin, vaultAta])
        .rpc();
    }

    // Nạp 10k USDC vào vault.
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

    // Pool thanh khoản của DEX giả.
    [poolAuthority] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool")],
      dexProgram.programId
    );
    poolUsdc = await createAta(provider, admin, usdcMint, poolAuthority);
    poolSol = await createAta(provider, admin, solMint, poolAuthority);
    await mintTo(provider, admin, solMint, poolSol, 1_000_000_000_000n);

    // --- Đăng ký ADAPTER-SWAP (không phải DEX!) làm Action adapter ---
    //
    // Vault CPI vào adapter, adapter CPI vào DEX. Vault không biết DEX nào tồn tại.
    await vaultProgram.methods
      .registerAdapter(adapterProgram.programId, { action: {} }, "swap")
      .accounts({ admin: admin.publicKey })
      .signers([admin])
      .rpc();

    await advanceClock(BigInt(8 * 24 * 3600));

    // is_entry = true: swap là cửa VÀO (mua asset mới) → pause chặn được.
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

  it("swap thành công qua chuỗi vault → adapter → dex", async () => {
    const vaultUsdc = await vaultTokenAccount(usdcMint);
    const vaultSol = await vaultTokenAccount(solMint);
    const usdcBefore = await tokenBalance(provider, vaultUsdc);
    const solBefore = await tokenBalance(provider, vaultSol);

    const amountIn = 1_000_000_000n; // 1000 USDC
    const out = fairOut(amountIn); // 10 SOL theo giá Pyth

    await doExecute(amountIn, out, amountIn, out);

    assert.equal(await tokenBalance(provider, vaultUsdc), usdcBefore - amountIn);
    assert.equal(await tokenBalance(provider, vaultSol), solBefore + out);
  });

  it("🛡 NAV-delta chặn adapter LẤY TIỀN KHÔNG TRẢ GÌ", async () => {
    // DEX lấy 1000 USDC, trả về 0 SOL. Vault mất giá trị → NAV tụt → revert.
    //
    // Điểm mấu chốt: vault KHÔNG cần biết đây là swap để bắt được. Nó chỉ thấy
    // "giá trị của tôi giảm quá mức cho phép" — và đó là kiểm tra AGNOSTIC, đúng
    // cho mọi action, kể cả những action chưa tồn tại.
    const vaultUsdc = await vaultTokenAccount(usdcMint);
    const before = await tokenBalance(provider, vaultUsdc);

    let rejected = false;
    try {
      await doExecute(1_000_000_000n, 0n, 1_000_000_000n, 0n);
    } catch {
      rejected = true;
    }

    assert.isTrue(rejected, "phải revert — vault mất giá trị");
    assert.equal(
      await tokenBalance(provider, vaultUsdc),
      before,
      "số dư KHÔNG đổi — transaction revert toàn bộ"
    );
  });

  it("🛡 NAV-delta chặn swap LỖ QUÁ SLIPPAGE (>1%)", async () => {
    const vaultUsdc = await vaultTokenAccount(usdcMint);
    const before = await tokenBalance(provider, vaultUsdc);

    const amountIn = 1_000_000_000n;
    const fair = fairOut(amountIn);
    const bad = (fair * 95n) / 100n; // chỉ trả 95% → lỗ 5%

    let rejected = false;
    try {
      await doExecute(amountIn, 0n, amountIn, bad); // min_out = 0: adapter không chặn
    } catch {
      rejected = true;
    }

    assert.isTrue(
      rejected,
      "phải revert vì ValueLost — dù ADAPTER cho qua (min_out = 0)"
    );
    assert.equal(await tokenBalance(provider, vaultUsdc), before);
  });

  it("swap trong hạn slippage (<1%) vẫn qua", async () => {
    const vaultSol = await vaultTokenAccount(solMint);
    const before = await tokenBalance(provider, vaultSol);

    const amountIn = 1_000_000_000n;
    const fair = fairOut(amountIn);
    const ok = (fair * 995n) / 1000n; // lỗ 0.5% — trong hạn 1%

    await doExecute(amountIn, ok, amountIn, ok);
    assert.equal(await tokenBalance(provider, vaultSol), before + ok);
  });

  it("🛡 adapter tự chặn khi DEX trả ít hơn min_out của nó", async () => {
    // Lớp phòng thủ của ADAPTER (không phải vault). Nó thừa về mặt an toàn — vault
    // sẽ bắt bằng NAV-delta — nhưng cho thông báo lỗi đúng chỗ.
    const amountIn = 1_000_000_000n;
    const fair = fairOut(amountIn);

    let rejected = false;
    try {
      // adapter đòi `fair`, DEX chỉ trả 90%.
      await doExecute(amountIn, fair, amountIn, (fair * 90n) / 100n);
    } catch {
      rejected = true;
    }

    assert.isTrue(rejected, "adapter phải revert vì SlippageExceeded");
  });

  it("🔒 reserved: adapter KHÔNG tiêu được phần đã hứa cho người redeem", async () => {
    // Lỗ hổng nghiêm trọng nhất từng có trong code này.
    //
    // Kịch bản (trước khi có `reserved`):
    //   1. Alice redeem_request → burn share, vault chốt "nợ Alice X USDC"
    //   2. Alice chưa kịp claim (24 asset cần nhiều transaction)
    //   3. execute swap đi TOÀN BỘ USDC — "hợp lệ" theo mọi kiểm tra khác
    //   4. Alice claim → vault không còn đủ → FAIL. Share đã burn mất rồi.

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
    assert.isAbove(Number(reserved), 0, "redeem_request phải khoá phần đã hứa");

    // Token vẫn còn NGUYÊN trong vault — chưa ai claim.
    assert.equal(
      await tokenBalance(provider, vaultUsdc),
      usdcBalance,
      "token chưa rời vault, chỉ mới bị khoá trên sổ sách"
    );

    const available = usdcBalance - reserved;

    // ĐÒN TẤN CÔNG: cố swap nhiều hơn phần khả dụng.
    // Số dư VẬT LÝ vẫn đủ — chỉ có `reserved` chặn.
    let rejected = false;
    try {
      const amt = available + 1n;
      await doExecute(amt, fairOut(amt), amt, fairOut(amt));
    } catch {
      rejected = true;
    }

    assert.isTrue(
      rejected,
      "phải revert vì InsufficientAvailableBalance — dù số dư vật lý đủ"
    );

    // Nhưng swap trong phần khả dụng VẪN CHẠY — reserved không đóng băng vault.
    const half = available / 2n;
    await doExecute(half, fairOut(half), half, fairOut(half));

    // Và Alice VẪN claim được đúng phần của mình — đó là toàn bộ mục đích.
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
      "Alice nhận đúng phần đã hứa — kể cả sau khi vault đã swap"
    );

    const after = await vaultProgram.account.asset.fetch(assetPda(usdcMint));
    assert.equal(
      BigInt(after.reserved.toString()),
      0n,
      "reserved về 0 sau claim — nếu không nó phình lên và khoá chết vault"
    );
  });

  it("🛡 pause chặn cửa VÀO (is_entry = true)", async () => {
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

    // Vault không biết action 0 là "swap". Nó đọc cờ `is_entry` mà governance đã đặt
    // lúc đăng ký capability — đó là cách nó biết đây là cửa vào mà không cần hiểu
    // ngữ nghĩa.
    assert.include(err, "VaultPaused", "cửa vào phải bị pause chặn");

    await vaultProgram.methods
      .setPaused(false)
      .accounts({ admin: admin.publicKey })
      .signers([admin])
      .rpc();
  });
});
