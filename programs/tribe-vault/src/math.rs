use anchor_lang::prelude::*;

use crate::errors::TribeError;

/// Quy ước làm tròn của protocol
///
/// Mọi phép chia đều làm tròn XUỐNG, và luôn theo hướng CÓ LỢI CHO VAULT:
///
///   - deposit: share nhận về làm tròn xuống  -> người deposit chịu phần lẻ
///   - redeem:  tài sản trả ra làm tròn xuống -> người redeem chịu phần lẻ
///
/// Phần lẻ ở lại vault, tức là thuộc về những người còn nắm share. Không bao
/// giờ để phần lẻ chảy ra ngoài, vì đó là con đường rút ruột vault bằng cách
/// lặp lại hàng triệu lệnh nhỏ để gom sai số làm tròn.
///
/// Mọi phép nhân đều nâng lên u128 trước khi chia, để tích trung gian không tràn.

/// Quy giá trị một lượng token về đơn vị kế toán (USDC, 6 decimals).
///
/// Pyth trả giá dạng `price * 10^expo` với `expo` thường âm (ví dụ -8).
/// Giá trị = amount / 10^decimals * price * 10^expo * 10^quote_decimals
pub fn asset_value_in_quote(
    amount: u64,
    price: u64,
    price_expo: i32,
    asset_decimals: u8,
    quote_decimals: u8,
) -> Result<u64> {
    if amount == 0 || price == 0 {
        return Ok(0);
    }

    let value = (amount as u128)
        .checked_mul(price as u128)
        .ok_or(TribeError::MathOverflow)?;

    // Số mũ ròng: 10^(expo + quote_decimals - asset_decimals)
    let net_exp = (price_expo)
        .checked_add(quote_decimals as i32)
        .ok_or(TribeError::MathOverflow)?
        .checked_sub(asset_decimals as i32)
        .ok_or(TribeError::MathOverflow)?;

    let scaled = apply_exponent(value, net_exp)?;

    u64::try_from(scaled).map_err(|_| TribeError::MathOverflow.into())
}

/// Nhân/chia cho 10^exp, dùng u128 để không tràn.
fn apply_exponent(value: u128, exp: i32) -> Result<u128> {
    // Ngoài khoảng này thì kết quả hoặc tràn u128, hoặc chắc chắn về 0.
    require!((-38..=38).contains(&exp), TribeError::InvalidOracleExponent);

    if exp == 0 {
        return Ok(value);
    }

    let factor = 10u128
        .checked_pow(exp.unsigned_abs())
        .ok_or(TribeError::MathOverflow)?;

    if exp > 0 {
        value.checked_mul(factor).ok_or(TribeError::MathOverflow.into())
    } else {
        // Chia làm tròn xuống — có lợi cho vault.
        Ok(value / factor)
    }
}

/// Số share được mint cho một khoản deposit.
///
///   vault rỗng: shares = deposit_value            (tỷ giá khởi điểm 1:1)
///   vault có tiền: shares = deposit_value * total_shares / nav_truoc_deposit
///
/// `nav_before` PHẢI là NAV *trước khi* token của người deposit vào vault. Nếu
/// lỡ tính NAV sau khi token đã chuyển vào, người deposit sẽ được mint share
/// dựa trên chính tiền của họ — một lỗi tự pha loãng, và ai cũng khai thác được.
pub fn shares_for_deposit(
    deposit_value: u64,
    nav_before: u64,
    total_shares: u64,
) -> Result<u64> {
    if total_shares == 0 {
        return Ok(deposit_value);
    }

    require!(nav_before > 0, TribeError::InvalidVaultState);

    let shares = (deposit_value as u128)
        .checked_mul(total_shares as u128)
        .ok_or(TribeError::MathOverflow)?
        .checked_div(nav_before as u128)
        .ok_or(TribeError::MathOverflow)?;

    u64::try_from(shares).map_err(|_| TribeError::MathOverflow.into())
}

/// Phần một asset mà người redeem được nhận, theo đúng tỷ lệ share.
///
///   amount = vault_balance * shares_burned / total_shares
///
/// Làm tròn xuống: phần lẻ ở lại vault cho những người còn nắm share.
pub fn pro_rata_amount(
    vault_balance: u64,
    shares_burned: u64,
    total_shares: u64,
) -> Result<u64> {
    require!(total_shares > 0, TribeError::InvalidVaultState);

    if vault_balance == 0 || shares_burned == 0 {
        return Ok(0);
    }

    let amount = (vault_balance as u128)
        .checked_mul(shares_burned as u128)
        .ok_or(TribeError::MathOverflow)?
        .checked_div(total_shares as u128)
        .ok_or(TribeError::MathOverflow)?;

    u64::try_from(amount).map_err(|_| TribeError::MathOverflow.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- asset_value_in_quote ---

    #[test]
    fn value_sol_at_100_usd() {
        // 1 SOL (9 decimals), giá 100 USD (expo -8) -> 100 USDC (6 decimals)
        let v = asset_value_in_quote(1_000_000_000, 100 * 100_000_000, -8, 9, 6).unwrap();
        assert_eq!(v, 100_000_000);
    }

    #[test]
    fn value_usdc_is_identity() {
        // 1 USDC, giá 1 USD -> 1 USDC
        let v = asset_value_in_quote(1_000_000, 100_000_000, -8, 6, 6).unwrap();
        assert_eq!(v, 1_000_000);
    }

    #[test]
    fn value_zero_amount() {
        assert_eq!(asset_value_in_quote(0, 100_000_000, -8, 9, 6).unwrap(), 0);
    }

    #[test]
    fn value_rounds_down_never_up() {
        // Lượng cực nhỏ, giá trị thật < 1 unit USDC -> phải ra 0, không được làm tròn lên.
        let v = asset_value_in_quote(1, 100_000_000, -8, 9, 6).unwrap();
        assert_eq!(v, 0);
    }

    #[test]
    fn value_rejects_absurd_exponent() {
        assert!(asset_value_in_quote(1_000_000, 100_000_000, -100, 6, 6).is_err());
    }

    #[test]
    fn value_large_holding_does_not_overflow() {
        // 10 triệu SOL ở giá 250 USD — tích trung gian vượt xa u64, phải sống nhờ u128.
        let v = asset_value_in_quote(
            10_000_000 * 1_000_000_000,
            250 * 100_000_000,
            -8,
            9,
            6,
        )
        .unwrap();
        assert_eq!(v, 2_500_000_000 * 1_000_000);
    }

    // --- shares_for_deposit ---

    #[test]
    fn first_deposit_mints_one_to_one() {
        assert_eq!(shares_for_deposit(1_000_000_000, 0, 0).unwrap(), 1_000_000_000);
    }

    #[test]
    fn second_deposit_is_proportional() {
        // Vault: NAV 1000 USDC, 1000 share. Nạp thêm 500 -> nhận 500 share.
        let s = shares_for_deposit(500_000_000, 1_000_000_000, 1_000_000_000).unwrap();
        assert_eq!(s, 500_000_000);
    }

    #[test]
    fn deposit_after_vault_doubles_in_value() {
        // Vault: 1000 share, NAV tăng lên 2000 USDC. Nạp 1000 -> chỉ nhận 500 share.
        // Người mới không được ăn ké lợi nhuận của người cũ.
        let s = shares_for_deposit(1_000_000_000, 2_000_000_000, 1_000_000_000).unwrap();
        assert_eq!(s, 500_000_000);
    }

    #[test]
    fn deposit_after_vault_loses_value() {
        // Vault: 1000 share, NAV còn 500. Nạp 500 -> nhận 1000 share.
        let s = shares_for_deposit(500_000_000, 500_000_000, 1_000_000_000).unwrap();
        assert_eq!(s, 1_000_000_000);
    }

    #[test]
    fn deposit_rejects_zero_nav_with_live_shares() {
        // Có share lưu hành mà NAV = 0 là trạng thái hỏng, phải chặn chứ không chia cho 0.
        assert!(shares_for_deposit(1_000_000, 0, 1_000_000).is_err());
    }

    #[test]
    fn deposit_shares_round_down() {
        // NAV 3, total 1 -> deposit 1 chỉ được 0 share (1*1/3 = 0.33 -> 0).
        assert_eq!(shares_for_deposit(1, 3, 1).unwrap(), 0);
    }

    // --- pro_rata_amount ---

    #[test]
    fn redeem_half_the_vault() {
        assert_eq!(pro_rata_amount(1_000_000, 500, 1_000).unwrap(), 500_000);
    }

    #[test]
    fn redeem_everything() {
        assert_eq!(pro_rata_amount(1_000_000, 1_000, 1_000).unwrap(), 1_000_000);
    }

    #[test]
    fn redeem_rounds_down_dust_stays_in_vault() {
        // 10 token, redeem 1/3 -> nhận 3, không phải 3.33. Phần lẻ ở lại vault.
        assert_eq!(pro_rata_amount(10, 1, 3).unwrap(), 3);
    }

    #[test]
    fn redeem_from_empty_asset_gives_zero() {
        assert_eq!(pro_rata_amount(0, 500, 1_000).unwrap(), 0);
    }

    #[test]
    fn redeem_rejects_zero_total_shares() {
        assert!(pro_rata_amount(1_000, 100, 0).is_err());
    }

    #[test]
    fn redeem_large_balance_does_not_overflow() {
        // balance * shares vượt u64 -> chỉ sống được nhờ u128.
        let a = pro_rata_amount(u64::MAX, 1, 2).unwrap();
        assert_eq!(a, u64::MAX / 2);
    }

    /// Bất biến quan trọng nhất: gộp nhiều lần redeem nhỏ KHÔNG BAO GIỜ rút được
    /// nhiều hơn một lần redeem lớn. Nếu sai, kẻ tấn công chẻ nhỏ lệnh để bòn
    /// rút vault qua sai số làm tròn.
    #[test]
    fn splitting_redeem_never_extracts_more() {
        let balance = 1_000_000u64;
        let total = 10_000u64;

        let one_shot = pro_rata_amount(balance, 300, total).unwrap();

        let mut split_total = 0u64;
        let mut running_balance = balance;
        let mut running_shares = total;
        for _ in 0..3 {
            let got = pro_rata_amount(running_balance, 100, running_shares).unwrap();
            split_total += got;
            running_balance -= got;
            running_shares -= 100;
        }

        assert!(
            split_total <= one_shot,
            "chẻ nhỏ rút được {split_total} > rút một lần {one_shot} — vault bị bòn rút"
        );
    }
}
