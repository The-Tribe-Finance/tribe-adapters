import { PublicKey } from "@solana/web3.js";

/// The Pyth Solana Receiver's program id. Anchor's `Account<PriceUpdateV2>` checks the
/// owner, so the fake account MUST be owned by this program — otherwise
/// `Account::try_from` fails, exactly as it should on mainnet.
export const PYTH_RECEIVER_PROGRAM_ID = new PublicKey(
  "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
);

/// The first 8 bytes of sha256("account:PriceUpdateV2").
const DISCRIMINATOR = Buffer.from([34, 241, 35, 99, 157, 126, 244, 205]);

/// The account size, matching `PriceUpdateV2::LEN` in pyth-solana-receiver-sdk.
const LEN = 8 + 32 + 2 + 32 + 8 + 8 + 4 + 8 + 8 + 8 + 8 + 8;

export interface PriceConfig {
  feedId: Buffer; // 32 bytes
  price: bigint; // raw price, already scaled by 10^(-expo)
  conf: bigint; // confidence interval
  expo: number; // usually negative, e.g. -8
  publishTime: bigint; // unix seconds
}

/// Build the binary data of a `PriceUpdateV2`, in exactly the Borsh layout the program
/// reads.
///
/// We have to build it by hand because localnet has no Pyth. One wrong byte and the test
/// goes "green" while the real program would fail — so the layout here is cross-checked
/// directly against pyth-solana-receiver-sdk 0.6.1 / pythnet-sdk 2.3.1:
///
///   PriceUpdateV2 { write_authority: Pubkey, verification_level: enum,
///                   price_message: PriceFeedMessage, posted_slot: u64 }
///   PriceFeedMessage { feed_id: [u8;32], price: i64, conf: u64, exponent: i32,
///                      publish_time: i64, prev_publish_time: i64,
///                      ema_price: i64, ema_conf: u64 }
export function encodePriceUpdate(cfg: PriceConfig): Buffer {
  if (cfg.feedId.length !== 32) {
    throw new Error(`feedId must be exactly 32 bytes, got ${cfg.feedId.length}`);
  }

  const buf = Buffer.alloc(LEN);
  let o = 0;

  DISCRIMINATOR.copy(buf, o);
  o += 8;

  // write_authority — never read on the pricing path, leave it as zeros.
  o += 32;

  // verification_level: a Borsh enum.
  //
  //   variant 0 = Partial { num_signatures: u8 }  -> 1 byte tag + 1 byte payload
  //   variant 1 = Full                            -> 1 byte tag, NO payload
  //
  // PriceUpdateV2::LEN allocates 2 bytes (sized for the largest variant), but Borsh only
  // WRITES OUT 1 byte if it is Full. Use Full and every subsequent field is off by
  // exactly 1 byte, so feed_id reads back as garbage -> OracleFeedMismatch.
  //
  // So use Partial: it occupies exactly 2 bytes, matching both LEN and the way Borsh
  // writes it. The program does not read this field, so its value does not matter.
  buf.writeUInt8(0, o); // tag = Partial
  o += 1;
  buf.writeUInt8(5, o); // num_signatures
  o += 1;

  // --- price_message ---
  cfg.feedId.copy(buf, o);
  o += 32;
  buf.writeBigInt64LE(cfg.price, o);
  o += 8;
  buf.writeBigUInt64LE(cfg.conf, o);
  o += 8;
  buf.writeInt32LE(cfg.expo, o);
  o += 4;
  buf.writeBigInt64LE(cfg.publishTime, o);
  o += 8;
  buf.writeBigInt64LE(cfg.publishTime, o); // prev_publish_time
  o += 8;
  buf.writeBigInt64LE(cfg.price, o); // ema_price
  o += 8;
  buf.writeBigUInt64LE(cfg.conf, o); // ema_conf
  o += 8;

  // posted_slot — never read.
  o += 8;

  return buf;
}

/// A "healthy" price: fresh, with a narrow conf (0.1% of the price — below oracle.rs's
/// 2% threshold).
export function healthyPrice(
  feedId: Buffer,
  priceUsd: number,
  publishTime: bigint
): PriceConfig {
  const price = BigInt(Math.round(priceUsd * 1e8));
  return {
    feedId,
    price,
    conf: price / 1000n,
    expo: -8,
    publishTime,
  };
}

/// A deterministic feed id derived from a label, so tests stay readable.
export function feedIdFor(label: string): Buffer {
  const b = Buffer.alloc(32);
  Buffer.from(label).copy(b);
  return b;
}
