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

/// `@solana/spl-token` already ships createMint/mintTo, but they require a real
/// `Connection` (they call sendTransaction). BankrunProvider has none — it runs against
/// an in-memory ledger. So we have to build the raw instructions and send them through
/// the provider.

export async function createMint(
  provider: BankrunProvider,
  payer: Keypair,
  decimals: number
): Promise<PublicKey> {
  const mint = Keypair.generate();
  const rent = 1_461_600; // rent-exempt for MINT_SIZE (82 bytes)

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
  // allowOwnerOffCurve = true: the owner may be a PDA (vault_authority, pool_authority),
  // not just an ordinary wallet. PDAs lie off the ed25519 curve, so this function rejects
  // them by default.
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

/// Read a token account's balance straight from bankrun's ledger.
export async function tokenBalance(
  provider: BankrunProvider,
  address: PublicKey
): Promise<bigint> {
  const acc = await provider.context.banksClient.getAccount(address);
  if (!acc) throw new Error(`token account does not exist: ${address}`);
  return AccountLayout.decode(Buffer.from(acc.data)).amount;
}

export function ata(mint: PublicKey, owner: PublicKey): PublicKey {
  return getAssociatedTokenAddressSync(mint, owner, true);
}
