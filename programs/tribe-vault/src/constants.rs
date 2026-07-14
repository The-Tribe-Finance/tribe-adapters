use anchor_lang::prelude::*;

#[constant]
pub const VAULT_SEED: &[u8] = b"vault";
#[constant]
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault_authority";
#[constant]
pub const ASSET_SEED: &[u8] = b"asset";
#[constant]
pub const REDEEM_TICKET_SEED: &[u8] = b"redeem_ticket";
#[constant]
pub const DEPOSIT_LOT_SEED: &[u8] = b"deposit_lot";
#[constant]
pub const ADAPTER_SEED: &[u8] = b"adapter";
#[constant]
pub const CAPABILITY_SEED: &[u8] = b"capability";

/// Timelock để một adapter mới có hiệu lực: 7 ngày.
///
/// Dài hơn hẳn timelock giao dịch thường (24h) và có chủ đích: adapter được
/// cấp quyền điều khiển tiền vault, nên cộng đồng cần đủ thời gian soi và
/// redeem thoát ra nếu thấy đáng ngờ.
pub const ADAPTER_TIMELOCK_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Timelock để một capability mới có hiệu lực: 7 ngày, bằng adapter.
///
/// Thêm một capability = mở một ĐƯỜNG TIỀN MỚI ra khỏi vault. Nguy hiểm ngang
/// việc thêm adapter, nên chịu cùng thời gian chờ. Gỡ thì có hiệu lực ngay —
/// vẫn là "mở cửa thì chậm, đóng cửa thì tức thì".
pub const CAPABILITY_TIMELOCK_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Số tài sản tối đa vault có thể nắm giữ.
pub const MAX_ASSETS: usize = 24;

/// Giá Pyth cũ hơn ngưỡng này (giây) bị coi là không hợp lệ.
pub const MAX_PRICE_AGE_SECONDS: u64 = 60;

/// Đơn vị kế toán của protocol: USDC có 6 decimals.
pub const QUOTE_DECIMALS: u8 = 6;
pub const SHARE_DECIMALS: u8 = 6;

/// Deposit tối thiểu, chặn lỗi làm tròn từ các khoản siêu nhỏ.
pub const MIN_DEPOSIT: u64 = 1_000_000; // 1 USDC

/// Share tối thiểu bị khóa vĩnh viễn trong lần deposit đầu tiên.
///
/// Chống inflation attack: kẻ tấn công deposit 1 unit rồi donate trực tiếp một
/// lượng lớn token vào vault để thổi giá share, khiến người deposit sau bị làm
/// tròn về 0 share. Khóa cứng một ít share đầu tiên làm cho đòn này không có lãi.
pub const MINIMUM_LIQUIDITY: u64 = 1_000;

/// Thang chia tỷ lệ dùng cho mọi phép tính phần trăm (1_000_000 = 100%).
pub const BPS_SCALE: u64 = 1_000_000;

/// Slippage tối đa cho mỗi lần swap: 1%.
///
/// Vault tự tính `min_out` = giá_Pyth − 1%, KHÔNG lấy từ payload của Jupiter.
/// Nếu tin min_out do người gọi cung cấp, kẻ tấn công đưa route tồi kèm
/// min_out = 0 và swap 1 triệu USDC lấy về 1 lamport — hoàn toàn "hợp lệ".
pub const MAX_SLIPPAGE_BPS: u64 = 10_000; // 1% của 1_000_000
