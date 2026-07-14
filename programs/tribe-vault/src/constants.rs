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

/// Delay before a newly registered adapter becomes usable: 7 days.
///
/// Deliberately much longer than a regular trade timelock (24h). An adapter is
/// granted authority over vault funds, so the community needs time to inspect it —
/// and to redeem out if it looks suspicious.
pub const ADAPTER_TIMELOCK_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Delay before a newly registered capability becomes usable: 7 days, same as adapters.
///
/// Registering a capability opens a NEW PATH for funds to leave the vault. That is as
/// dangerous as adding an adapter, so it waits just as long. Removal takes effect
/// immediately — "opening doors is slow, closing them is instant".
pub const CAPABILITY_TIMELOCK_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Maximum number of assets the vault can hold.
pub const MAX_ASSETS: usize = 24;

/// A Pyth price older than this (seconds) is rejected.
pub const MAX_PRICE_AGE_SECONDS: u64 = 60;

/// The protocol's unit of account: USDC, 6 decimals.
pub const QUOTE_DECIMALS: u8 = 6;
pub const SHARE_DECIMALS: u8 = 6;

/// Minimum deposit. Blocks rounding exploits from dust-sized deposits.
pub const MIN_DEPOSIT: u64 = 1_000_000; // 1 USDC

/// Shares permanently locked on the very first deposit.
///
/// Defends against the inflation attack: without it, an attacker deposits 1 unit
/// (receiving 1 share), then transfers a large amount of tokens directly into the
/// vault to inflate the share price. The next depositor's shares round down to zero,
/// and their money lands in the attacker's pocket. Burning a small amount of the first
/// shares makes that attack unprofitable.
pub const MINIMUM_LIQUIDITY: u64 = 1_000;

/// Scale used for every percentage computation (1_000_000 = 100%).
pub const BPS_SCALE: u64 = 1_000_000;

/// Maximum value the vault may lose on a single action: 1% of the value sent in.
///
/// This is enforced by the vault via value-delta, NOT by trusting anything the adapter
/// says. Note it is anchored to the TRADE VALUE, not to total NAV — anchoring to NAV
/// would widen the allowance as the vault grows, letting a single trade drain more the
/// richer the vault gets. See `execute.rs`.
pub const MAX_SLIPPAGE_BPS: u64 = 10_000; // 1% of 1_000_000
