use anchor_lang::prelude::*;
use anchor_spl::token_interface::TokenAccount;
use pyth_solana_receiver_sdk::price_update::PriceUpdateV2;

use crate::constants::{MAX_ASSETS, QUOTE_DECIMALS};
use crate::errors::TribeError;
use crate::math::asset_value_in_quote;
use crate::oracle::get_validated_price;
use crate::state::{Asset, AssetKind, Vault};

/// Per-asset balances of the vault, plus total NAV.
pub struct NavSnapshot {
    /// NAV in the unit of account (USDC, 6 decimals).
    pub total_value: u64,
    /// Raw balance of each asset, in Vault::asset_mints order.
    /// Used for in-kind redemption: pay out the actual token, no conversion.
    pub balances: [u64; MAX_ASSETS],
    /// Value (in USDC) of each asset, same order.
    /// Used for the exposure check: asset i's weight within NAV.
    pub values: [u64; MAX_ASSETS],
}

/// Compute NAV from remaining_accounts.
///
/// Expects a triple of accounts for EVERY registered asset, in Vault::asset_mints order:
///
///   [Asset PDA, the vault's token account, Pyth price update]
///
/// # Why every single one is mandatory
///
/// NAV decides how many shares get minted. If the caller could omit assets, they would
/// omit precisely the expensive ones to depress NAV, deposit while shares are
/// artificially cheap, and pocket the difference at everyone else's expense.
/// So: a missing asset fails, wrong order fails, a duplicate fails.
pub fn compute_nav<'info>(
    vault: &Vault,
    vault_key: &Pubkey,
    remaining: &'info [AccountInfo<'info>],
    clock: &Clock,
) -> Result<NavSnapshot> {
    let asset_count = vault.asset_count as usize;

    // Empty vault: NAV = 0, no oracle reads needed.
    if asset_count == 0 {
        return Ok(NavSnapshot {
            total_value: 0,
            balances: [0u64; MAX_ASSETS],
            values: [0u64; MAX_ASSETS],
        });
    }

    require!(
        remaining.len() == asset_count * 3,
        TribeError::IncompleteAssetSet
    );

    let mut balances = [0u64; MAX_ASSETS];
    let mut values = [0u64; MAX_ASSETS];
    let mut total_value: u64 = 0;

    for i in 0..asset_count {
        let asset_info: &AccountInfo<'info> = &remaining[i * 3];
        let token_info: &AccountInfo<'info> = &remaining[i * 3 + 1];
        let oracle_info: &AccountInfo<'info> = &remaining[i * 3 + 2];

        // Account<T> checks owner + discriminator for us. Never UncheckedAccount here:
        // otherwise an attacker forges an account with the same layout and inflates the
        // price at will.
        let asset: Account<Asset> = Account::try_from(asset_info)?;

        // The Asset PDA must be the one the vault registered, at exactly index i.
        // This also blocks passing the same asset several times.
        require_keys_eq!(asset.vault, *vault_key, TribeError::AssetNotRegistered);
        require!(
            asset.mint == vault.asset_mints[i],
            TribeError::AssetNotRegistered
        );
        require!(asset.index as usize == i, TribeError::AssetNotRegistered);

        // The token account must be the one the vault owns for this asset.
        require_keys_eq!(
            asset.token_account,
            token_info.key(),
            TribeError::AssetNotRegistered
        );
        let token_account: InterfaceAccount<TokenAccount> =
            InterfaceAccount::try_from(token_info)?;

        // NAV only counts what the vault ACTUALLY still owns.
        //
        // `reserved` is owed to users who burned shares but have not finished claiming.
        // That money no longer belongs to current shareholders — it is on its way out.
        // Counting it would inflate NAV, and later depositors would overpay per share.
        //
        // saturating_sub rather than checked_sub: if reserved somehow exceeded balance
        // (it should not), treat available as 0 instead of panicking. NAV reading low is
        // better than a frozen vault — rounding always favors the vault.
        let balance = token_account.amount.saturating_sub(asset.reserved);
        balances[i] = balance;

        // Price according to the asset kind.
        //
        // The MVP only supports SplToken. Lend/stake positions have no Pyth feed — their
        // value must be asked of the protocol itself (Kamino, Marinade...) through an
        // adapter. Until that exists, FAIL OUTRIGHT; never guess. A NAV that is even
        // slightly wrong is enough for someone to mint cheap shares and drain the vault.
        // Freezing beats mispricing.
        let value = match asset.kind {
            AssetKind::SplToken => {
                require_keys_eq!(
                    asset.oracle,
                    oracle_info.key(),
                    TribeError::OracleFeedMismatch
                );
                let price_update: Account<PriceUpdateV2> = Account::try_from(oracle_info)?;
                let price = get_validated_price(&price_update, &asset.feed_id, clock)?;

                asset_value_in_quote(
                    balance,
                    price.price,
                    price.expo,
                    asset.decimals,
                    QUOTE_DECIMALS,
                )?
            }
            AssetKind::LendPosition | AssetKind::StakePosition => {
                return err!(TribeError::PricingKindNotSupported);
            }
        };

        values[i] = value;
        total_value = total_value
            .checked_add(value)
            .ok_or(TribeError::MathOverflow)?;
    }

    Ok(NavSnapshot {
        total_value,
        balances,
        values,
    })
}

/// Read each asset's AVAILABLE balance — WITHOUT touching any oracle.
///
/// # Why redemption must not use an oracle
///
/// Redemption is pure pro-rata arithmetic:
///
///   owed[i] = (balance[i] − reserved[i]) × shares_burned / total_shares
///
/// Price does not appear in that formula. A redeemer receives their exact fraction of
/// EACH asset (in-kind), not a converted dollar amount — so the vault does not need to
/// know what any asset is worth.
///
/// Calling an oracle here is not merely redundant, it is DANGEROUS: redemption would
/// fail whenever a price is stale or its confidence too wide — i.e. exactly when markets
/// are in chaos and users most need to get out. That breaks the protocol's most important
/// invariant:
///
///   **Redemption is never blocked. Users can always get their money out.**
///
/// Removing the oracle from redemption makes it STRONGER, not weaker.
///
/// Expects a PAIR of accounts per asset (not a triple like `compute_nav`):
///
///   [Asset PDA, the vault's token account]
pub fn available_balances<'info>(
    vault: &Vault,
    vault_key: &Pubkey,
    remaining: &'info [AccountInfo<'info>],
) -> Result<[u64; MAX_ASSETS]> {
    let asset_count = vault.asset_count as usize;

    let mut balances = [0u64; MAX_ASSETS];
    if asset_count == 0 {
        return Ok(balances);
    }

    // Every asset is still mandatory. Omit one and the redeemer never receives their
    // share of it — and that share stays in the vault forever.
    require!(
        remaining.len() == asset_count * 2,
        TribeError::IncompleteAssetSet
    );

    for i in 0..asset_count {
        let asset_info: &AccountInfo<'info> = &remaining[i * 2];
        let token_info: &AccountInfo<'info> = &remaining[i * 2 + 1];

        // Account<T> checks owner + discriminator — never UncheckedAccount, or an
        // attacker forges a look-alike account and overstates the balance.
        let asset: Account<Asset> = Account::try_from(asset_info)?;

        require_keys_eq!(asset.vault, *vault_key, TribeError::AssetNotRegistered);
        require!(
            asset.mint == vault.asset_mints[i],
            TribeError::AssetNotRegistered
        );
        require!(asset.index as usize == i, TribeError::AssetNotRegistered);
        require_keys_eq!(
            asset.token_account,
            token_info.key(),
            TribeError::AssetNotRegistered
        );

        let token_account: InterfaceAccount<TokenAccount> =
            InterfaceAccount::try_from(token_info)?;

        // Only divide up what has not been promised. `reserved` belongs to people who
        // already burned shares and are waiting to claim — it must not be re-divided.
        balances[i] = token_account.amount.saturating_sub(asset.reserved);
    }

    Ok(balances)
}

/// Price a SINGLE asset — used by `execute_action`, where the compute budget is tight.
///
/// # Why `execute_action` does not call `compute_nav`
///
/// Pricing every asset twice (before and after the CPI) is the obvious design, but it
/// scales with the asset count while the compute budget does not. A multi-hop Jupiter
/// route already consumes a large slice of it.
///
/// So execution splits its measurements by cost:
///
/// ```text
///   VALUE   (expensive: must decode Pyth)   -> only asset_in + asset_out
///   BALANCE (cheap: just read token account) -> EVERY asset
/// ```
///
/// Measuring every balance still catches an adapter quietly touching a third asset —
/// which "only measure the two named assets" would miss entirely. We give up *pricing*
/// that third asset, not *detecting* that it moved. And pricing it is unnecessary: if
/// its balance did not change, its value cancels out of both sides of the comparison.
pub fn value_of<'info>(
    asset: &Asset,
    token_info: &'info AccountInfo<'info>,
    oracle_info: &'info AccountInfo<'info>,
    clock: &Clock,
) -> Result<(u64, u64)> {
    require_keys_eq!(
        asset.token_account,
        token_info.key(),
        TribeError::AssetNotRegistered
    );

    let token_account: InterfaceAccount<TokenAccount> =
        InterfaceAccount::try_from(token_info)?;

    // Only what the vault ACTUALLY still owns — minus what is promised to redeemers.
    let balance = token_account.amount.saturating_sub(asset.reserved);

    let value = match asset.kind {
        AssetKind::SplToken => {
            require_keys_eq!(
                asset.oracle,
                oracle_info.key(),
                TribeError::OracleFeedMismatch
            );
            let price_update: Account<PriceUpdateV2> = Account::try_from(oracle_info)?;
            let price = get_validated_price(&price_update, &asset.feed_id, clock)?;

            asset_value_in_quote(
                balance,
                price.price,
                price.expo,
                asset.decimals,
                QUOTE_DECIMALS,
            )?
        }
        // Positions (kToken, LST) cannot be priced yet — freezing beats guessing.
        AssetKind::LendPosition | AssetKind::StakePosition => {
            return err!(TribeError::PricingKindNotSupported);
        }
    };

    Ok((balance, value))
}

/// Add to asset `i`'s `reserved`, writing straight back to the account.
///
/// `Asset` accounts arrive via `remaining_accounts`, so Anchor does not persist them
/// automatically the way it does for accounts declared in an `Accounts` struct. We must
/// deserialize, mutate, and serialize back by hand — which is what this does.
///
/// Called from `redeem_request` (to add) and `claim_asset` (to subtract).
pub fn add_reserved<'info>(
    asset_info: &'info AccountInfo<'info>,
    delta: u64,
) -> Result<()> {
    let mut asset: Account<'info, Asset> = Account::try_from(asset_info)?;

    asset.reserved = asset
        .reserved
        .checked_add(delta)
        .ok_or(TribeError::MathOverflow)?;

    // Write back into the account data.
    //
    // Use AnchorSerialize, NOT try_serialize: try_serialize prepends the 8-byte
    // discriminator, but we are writing into the region AFTER it — so it would stamp the
    // discriminator right on top of the struct's first field.
    let mut data = asset_info.try_borrow_mut_data()?;
    let mut cursor: &mut [u8] = &mut data[8..];
    AnchorSerialize::serialize(&*asset, &mut cursor)?;

    Ok(())
}
