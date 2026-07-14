import {
  PublicKey,
  Keypair,
  SystemProgram,
  Transaction,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  MINT_SIZE,
  createInitializeMint2Instruction,
  createAssociatedTokenAccountInstruction,
  createMintToInstruction,
  getAssociatedTokenAddressSync,
  AccountLayout,
} from "@solana/spl-token";
import { BankrunProvider } from "anchor-bankrun";

/// `@solana/spl-token` cung cấp sẵn createMint/mintTo, nhưng chúng đòi một
/// `Connection` thật (gọi sendTransaction). BankrunProvider không có — nó chạy
/// trên một ledger trong bộ nhớ. Nên phải dựng instruction thô và gửi qua provider.

export async function createMint(
  provider: BankrunProvider,
  payer: Keypair,
  decimals: number
): Promise<PublicKey> {
  const mint = Keypair.generate();
  const rent = 1_461_600; // rent-exempt cho MINT_SIZE (82 byte)

  const tx = new Transaction().add(
    SystemProgram.createAccount({
      fromPubkey: payer.publicKey,
      newAccountPubkey: mint.publicKey,
      space: MINT_SIZE,
      lamports: rent,
      programId: TOKEN_PROGRAM_ID,
    }),
    createInitializeMint2Instruction(
      mint.publicKey,
      decimals,
      payer.publicKey, // mint authority
      null // freeze authority
    )
  );

  await provider.sendAndConfirm(tx, [payer, mint]);
  return mint.publicKey;
}

export async function createAta(
  provider: BankrunProvider,
  payer: Keypair,
  mint: PublicKey,
  owner: PublicKey
): Promise<PublicKey> {
  // allowOwnerOffCurve = true: chủ sở hữu có thể là một PDA (vault_authority,
  // pool_authority), không chỉ ví thường. PDA nằm ngoài đường cong ed25519 nên
  // hàm này mặc định từ chối chúng.
  const ata = getAssociatedTokenAddressSync(mint, owner, true);
  const tx = new Transaction().add(
    createAssociatedTokenAccountInstruction(payer.publicKey, ata, owner, mint)
  );
  await provider.sendAndConfirm(tx, [payer]);
  return ata;
}

export async function mintTo(
  provider: BankrunProvider,
  mintAuthority: Keypair,
  mint: PublicKey,
  dest: PublicKey,
  amount: bigint
): Promise<void> {
  const tx = new Transaction().add(
    createMintToInstruction(mint, dest, mintAuthority.publicKey, amount)
  );
  await provider.sendAndConfirm(tx, [mintAuthority]);
}

/// Đọc số dư một token account thẳng từ ledger của bankrun.
export async function tokenBalance(
  provider: BankrunProvider,
  address: PublicKey
): Promise<bigint> {
  const acc = await provider.context.banksClient.getAccount(address);
  if (!acc) throw new Error(`token account không tồn tại: ${address}`);
  return AccountLayout.decode(Buffer.from(acc.data)).amount;
}

export function ata(mint: PublicKey, owner: PublicKey): PublicKey {
  return getAssociatedTokenAddressSync(mint, owner, true);
}
