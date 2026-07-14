use anchor_lang::prelude::*;
use pyth_solana_receiver_sdk::price_update::PriceUpdateV2;

use crate::constants::MAX_PRICE_AGE_SECONDS;
use crate::errors::TribeError;

/// Giá đã được xác thực, quy về dạng không âm.
pub struct ValidatedPrice {
    pub price: u64,
    pub expo: i32,
}

/// Đọc giá từ một Pyth price update và bắt nó phải vượt qua mọi cửa kiểm tra.
///
/// Đây là ranh giới tin cậy của protocol. Mọi thứ phía sau — mint bao nhiêu
/// share, trả bao nhiêu tài sản — đều dựa trên con số ra từ hàm này. Nên ở đây
/// thà chặn nhầm còn hơn bỏ sót:
///
/// 1. feed_id phải khớp asset  — chặn việc tráo feed SOL vào chỗ của BTC
/// 2. giá phải còn tươi (60s)  — chặn dùng giá cũ để mint rẻ / redeem đắt
/// 3. giá phải dương           — giá âm hoặc 0 là oracle hỏng
/// 4. độ tin cậy phải hẹp      — conf rộng nghĩa là thị trường loạn, giá không đáng tin
pub fn get_validated_price(
    price_update: &Account<PriceUpdateV2>,
    expected_feed_id: &[u8; 32],
    clock: &Clock,
) -> Result<ValidatedPrice> {
    // Chặn tráo feed: account có thể là một PriceUpdateV2 hợp lệ nhưng của tài
    // sản khác. Không kiểm tra thì kẻ tấn công đưa feed của token rẻ vào chỗ
    // token đắt để làm lệch NAV.
    let price_message = &price_update.price_message;
    require!(
        price_message.feed_id == *expected_feed_id,
        TribeError::OracleFeedMismatch
    );

    let age = clock
        .unix_timestamp
        .checked_sub(price_message.publish_time)
        .ok_or(TribeError::MathOverflow)?;

    // Giá từ tương lai cũng là bất thường -> coi như hỏng.
    require!(age >= 0, TribeError::StalePrice);
    require!(
        (age as u64) <= MAX_PRICE_AGE_SECONDS,
        TribeError::StalePrice
    );

    require!(price_message.price > 0, TribeError::InvalidPrice);

    let price = price_message.price as u64;
    let conf = price_message.conf;

    // Khoảng tin cậy rộng hơn 2% giá -> oracle đang không chắc chắn (thị trường
    // loạn, thanh khoản cạn). Định giá vault lúc này là mời gọi bị lợi dụng.
    let max_conf = price
        .checked_div(50)
        .ok_or(TribeError::MathOverflow)?;
    require!(conf <= max_conf, TribeError::PriceConfidenceTooWide);

    Ok(ValidatedPrice {
        price,
        expo: price_message.exponent,
    })
}
