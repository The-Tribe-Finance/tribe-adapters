use anchor_lang::prelude::*;

use crate::errors::TribeError;

/// The protocol's rounding rule
///
/// Every division rounds DOWN, and always IN THE VAULT'S FAVOR:
///
///   - deposit: shares minted round down  -> the depositor eats the remainder
///   - redeem:  assets paid out round down -> the redeemer eats the remainder
///
/// The remainder stays in the vault, i.e. it belongs to everyone still holding shares.
/// Never let it leak out: leaking it is exactly how you bleed a vault dry, by repeating
/// millions of tiny operations to harvest rounding error.
///
/// Every multiplication is widened to u128 before dividing, so the intermediate product
/// cannot overflow.

/// Convert a token amount into the unit of account (USDC, 6 decimals).
///
/// Pyth reports prices as `price * 10^expo`, where `expo` is usually negative (e.g. -8).
/// value = amount / 10^decimals * price * 10^expo * 10^quote_decimals
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

    // Net exponent: 10^(expo + quote_decimals - asset_decimals)
    let net_exp = (price_expo)
        .checked_add(quote_decimals as i32)
        .ok_or(TribeError::MathOverflow)?
        .checked_sub(asset_decimals as i32)
        .ok_or(TribeError::MathOverflow)?;

    let scaled = apply_exponent(value, net_exp)?;

    u64::try_from(scaled).map_err(|_| TribeError::MathOverflow.into())
}

/// Multiply/divide by 10^exp in u128 so nothing overflows.
fn apply_exponent(value: u128, exp: i32) -> Result<u128> {
    // Outside this range the result either overflows u128 or is certainly zero.
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
        // Division rounds down — in the vault's favor.
        Ok(value / factor)
    }
}

/// Shares minted for a deposit.
///
///   empty vault: shares = deposit_value                          (1:1 to start)
///   funded vault: shares = deposit_value * total_shares / nav_before_deposit
///
/// `nav_before` MUST be the NAV from *before* the depositor's tokens entered the vault.
/// Compute it after the transfer and the depositor gets minted shares against their own
/// money — a self-dilution bug that anyone can exploit.
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

/// The share of one asset a redeemer receives, pro rata to their shares.
///
///   amount = vault_balance * shares_burned / total_shares
///
/// Rounds down: the remainder stays in the vault, for everyone still holding shares.
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
        // 1 SOL (9 decimals) at $100 (expo -8) -> 100 USDC (6 decimals)
        let v = asset_value_in_quote(1_000_000_000, 100 * 100_000_000, -8, 9, 6).unwrap();
        assert_eq!(v, 100_000_000);
    }

    #[test]
    fn value_usdc_is_identity() {
        // 1 USDC at $1 -> 1 USDC
        let v = asset_value_in_quote(1_000_000, 100_000_000, -8, 6, 6).unwrap();
        assert_eq!(v, 1_000_000);
    }

    #[test]
    fn value_zero_amount() {
        assert_eq!(asset_value_in_quote(0, 100_000_000, -8, 9, 6).unwrap(), 0);
    }

    #[test]
    fn value_rounds_down_never_up() {
        // A dust amount worth < 1 USDC unit must yield 0 — never round up.
        let v = asset_value_in_quote(1, 100_000_000, -8, 9, 6).unwrap();
        assert_eq!(v, 0);
    }

    #[test]
    fn value_rejects_absurd_exponent() {
        assert!(asset_value_in_quote(1_000_000, 100_000_000, -100, 6, 6).is_err());
    }

    #[test]
    fn value_large_holding_does_not_overflow() {
        // 10M SOL at $250 — the intermediate product far exceeds u64; only u128 survives.
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
        // Vault: NAV 1000 USDC, 1000 shares. Deposit 500 -> receive 500 shares.
        let s = shares_for_deposit(500_000_000, 1_000_000_000, 1_000_000_000).unwrap();
        assert_eq!(s, 500_000_000);
    }

    #[test]
    fn deposit_after_vault_doubles_in_value() {
        // Vault: 1000 shares, NAV has grown to 2000 USDC. Deposit 1000 -> only 500
        // shares. Newcomers must not free-ride on existing holders' gains.
        let s = shares_for_deposit(1_000_000_000, 2_000_000_000, 1_000_000_000).unwrap();
        assert_eq!(s, 500_000_000);
    }

    #[test]
    fn deposit_after_vault_loses_value() {
        // Vault: 1000 shares, NAV has fallen to 500. Deposit 500 -> receive 1000 shares.
        let s = shares_for_deposit(500_000_000, 500_000_000, 1_000_000_000).unwrap();
        assert_eq!(s, 1_000_000_000);
    }

    #[test]
    fn deposit_rejects_zero_nav_with_live_shares() {
        // Shares outstanding with NAV = 0 is a broken state. Reject it; never divide by zero.
        assert!(shares_for_deposit(1_000_000, 0, 1_000_000).is_err());
    }

    #[test]
    fn deposit_shares_round_down() {
        // NAV 3, total 1 -> depositing 1 yields 0 shares (1*1/3 = 0.33 -> 0).
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
        // 10 tokens, redeeming 1/3 -> receive 3, not 3.33. The remainder stays in the vault.
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
        // balance * shares exceeds u64 -> only survivable in u128.
        let a = pro_rata_amount(u64::MAX, 1, 2).unwrap();
        assert_eq!(a, u64::MAX / 2);
    }

    /// The most important invariant: many small redemptions must NEVER extract more
    /// than one large one. If they could, an attacker would split their withdrawal to
    /// bleed the vault through rounding error.
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
            "split withdrawal took {split_total} > single withdrawal {one_shot} — vault is bleeding"
        );
    }
}
