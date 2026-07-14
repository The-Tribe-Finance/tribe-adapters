import { PublicKey } from "@solana/web3.js";

/// Program id của Pyth Solana Receiver. `Account<PriceUpdateV2>` của Anchor kiểm
/// tra owner, nên account giả BẮT BUỘC do program này sở hữu — nếu không,
/// `Account::try_from` sẽ fail đúng như nó phải fail trên mainnet.
export const PYTH_RECEIVER_PROGRAM_ID = new PublicKey(
  "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
);

/// 8 byte đầu của sha256("account:PriceUpdateV2").
const DISCRIMINATOR = Buffer.from([34, 241, 35, 99, 157, 126, 244, 205]);

/// Kích thước account, khớp `PriceUpdateV2::LEN` trong pyth-solana-receiver-sdk.
const LEN = 8 + 32 + 2 + 32 + 8 + 8 + 4 + 8 + 8 + 8 + 8 + 8;

export interface PriceConfig {
  feedId: Buffer; // 32 byte
  price: bigint; // giá thô, đã nhân 10^(-expo)
  conf: bigint; // khoảng tin cậy
  expo: number; // thường âm, ví dụ -8
  publishTime: bigint; // unix seconds
}

/// Dựng dữ liệu binary của một `PriceUpdateV2`, đúng layout Borsh mà program đọc.
///
/// Phải dựng thủ công vì localnet không có Pyth. Sai một byte là test "xanh" trong
/// khi program thật sẽ fail — nên layout ở đây được đối chiếu trực tiếp với
/// pyth-solana-receiver-sdk 0.6.1 / pythnet-sdk 2.3.1:
///
///   PriceUpdateV2 { write_authority: Pubkey, verification_level: enum,
///                   price_message: PriceFeedMessage, posted_slot: u64 }
///   PriceFeedMessage { feed_id: [u8;32], price: i64, conf: u64, exponent: i32,
///                      publish_time: i64, prev_publish_time: i64,
///                      ema_price: i64, ema_conf: u64 }
export function encodePriceUpdate(cfg: PriceConfig): Buffer {
  if (cfg.feedId.length !== 32) {
    throw new Error(`feedId phải đúng 32 byte, nhận được ${cfg.feedId.length}`);
  }

  const buf = Buffer.alloc(LEN);
  let o = 0;

  DISCRIMINATOR.copy(buf, o);
  o += 8;

  // write_authority — không được đọc trong đường tính giá, để 0.
  o += 32;

  // verification_level: enum Borsh.
  //
  //   variant 0 = Partial { num_signatures: u8 }  -> 1 byte tag + 1 byte payload
  //   variant 1 = Full                            -> 1 byte tag, KHÔNG payload
  //
  // PriceUpdateV2::LEN cấp phát 2 byte (theo variant lớn nhất), nhưng Borsh chỉ
  // GHI RA 1 byte nếu là Full. Dùng Full thì mọi field sau bị lệch đúng 1 byte và
  // feed_id đọc ra rác -> OracleFeedMismatch.
  //
  // Nên dùng Partial: nó chiếm đúng 2 byte, khớp cả LEN lẫn cách Borsh ghi.
  // Program không đọc field này nên giá trị không quan trọng.
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

  // posted_slot — không được đọc.
  o += 8;

  return buf;
}

/// Giá "khoẻ mạnh": tươi, conf hẹp (0.1% giá — dưới ngưỡng 2% của oracle.rs).
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

/// Feed id tất định từ một nhãn, để test đọc được.
export function feedIdFor(label: string): Buffer {
  const b = Buffer.alloc(32);
  Buffer.from(label).copy(b);
  return b;
}
