use anchor_lang::prelude::*;
use pyth_solana_receiver_sdk::price_update::PriceUpdateV2;

use crate::constants::MAX_PRICE_AGE_SECONDS;
use crate::errors::TribeError;

/// A validated price, normalized to a non-negative form.
pub struct ValidatedPrice {
    pub price: u64,
    pub expo: i32,
}

/// Read a price from a Pyth update and force it through every gate.
///
/// This is the protocol's trust boundary. Everything downstream — how many shares get
/// minted, how many assets get paid out — rests on the number this function returns.
/// So here, a false reject beats a false accept:
///
/// 1. feed_id must match the asset — blocks swapping a SOL feed in where BTC belongs
/// 2. price must be fresh (60s)    — blocks using a stale price to mint cheap / redeem rich
/// 3. price must be positive       — a negative or zero price means a broken oracle
/// 4. confidence must be tight     — a wide band means a chaotic market and an untrustworthy price
pub fn get_validated_price(
    price_update: &Account<PriceUpdateV2>,
    expected_feed_id: &[u8; 32],
    clock: &Clock,
) -> Result<ValidatedPrice> {
    // Block feed substitution: the account may be a perfectly valid PriceUpdateV2 —
    // just for a different asset. Without this check an attacker feeds a cheap token's
    // price where an expensive one belongs, and skews NAV.
    let price_message = &price_update.price_message;
    require!(
        price_message.feed_id == *expected_feed_id,
        TribeError::OracleFeedMismatch
    );

    let age = clock
        .unix_timestamp
        .checked_sub(price_message.publish_time)
        .ok_or(TribeError::MathOverflow)?;

    // A price from the future is just as abnormal -> treat it as broken.
    require!(age >= 0, TribeError::StalePrice);
    require!(
        (age as u64) <= MAX_PRICE_AGE_SECONDS,
        TribeError::StalePrice
    );

    require!(price_message.price > 0, TribeError::InvalidPrice);

    let price = price_message.price as u64;
    let conf = price_message.conf;

    // A confidence band wider than 2% of the price means the oracle is unsure (chaotic
    // market, drained liquidity). Valuing the vault in that moment is an open invitation.
    let max_conf = price
        .checked_div(50)
        .ok_or(TribeError::MathOverflow)?;
    require!(conf <= max_conf, TribeError::PriceConfidenceTooWide);

    Ok(ValidatedPrice {
        price,
        expo: price_message.exponent,
    })
}
