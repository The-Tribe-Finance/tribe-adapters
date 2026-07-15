use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{
    self, Mint, TokenAccount, TokenInterface, TransferChecked,
};

pub mod constants;
pub mod errors;
pub mod execute;
pub mod math;
pub mod nav;
pub mod oracle;
pub mod state;

use constants::*;
use errors::TribeError;
use execute::*;
use math::{pro_rata_amount, shares_for_deposit};
use nav::{add_reserved, available_balances, compute_nav};
use state::*;

declare_id!("7JVBNNDs9uKgYYuJ3wPqdBSjtnNgV6s3pjxZ83QMmhVs");

/// # The Tribe — Community Vault
///
/// The vault holds the community's money. This program deliberately does as
/// little as possible: take money in, pay money out, count shares, compute NAV.
/// Governance and execution live in other programs, so that the place holding
/// the money has to be upgraded as rarely as possible.
///
/// Three invariants that must NEVER be broken:
///
///   1. NAV is always computed from EVERY asset. If even one is missing, fail.
///   2. Rounding always favors the vault. The dust belongs to whoever still holds shares.
///   3. Redeem is never pausable. Users can always get their money out.
#[program]
pub mod tribe_vault {
    use super::*;

    /// Initialize the vault. Can only be called once.
    pub fn initialize_vault(ctx: Context<InitializeVault>) -> Result<()> {
        let vault = &mut ctx.accounts.vault;

        vault.admin = ctx.accounts.admin.key();
        // MVP: the admin presses execute themselves. Tier 3 switches this to the
        // tribe-governance PDA via `set_executor` — exactly one place to change,
        // which is why execution was split out from governance from the start.
        vault.executor = ctx.accounts.admin.key();
        vault.treasury = ctx.accounts.treasury.key();
        vault.share_mint = ctx.accounts.share_mint.key();
        vault.vault_authority = ctx.accounts.vault_authority.key();
        vault.total_shares = 0;
        vault.asset_count = 0;
        vault.paused = false;
        vault.vault_authority_bump = ctx.bumps.vault_authority;
        vault.bump = ctx.bumps.vault;
        vault.asset_mints = Vec::new();
        vault.redeem_ticket_counter = 0;

        Ok(())
    }

    /// Add an asset to the whitelist.
    ///
    /// This is a dangerous power: registering a junk token with a fake oracle is
    /// enough to drain the vault. In the MVP the admin holds it; it moves to
    /// governance as the protocol matures.
    pub fn register_asset(
        ctx: Context<RegisterAsset>,
        feed_id: [u8; 32],
        max_exposure_bps: u64,
    ) -> Result<()> {
        require!(
            max_exposure_bps <= BPS_SCALE,
            TribeError::ExposureExceeded
        );

        let vault = &mut ctx.accounts.vault;

        require!(
            (vault.asset_count as usize) < MAX_ASSETS,
            TribeError::TooManyAssets
        );

        let mint_key = ctx.accounts.mint.key();
        require!(
            !vault.asset_mints.contains(&mint_key),
            TribeError::AssetAlreadyRegistered
        );

        let index = vault.asset_count;

        let asset = &mut ctx.accounts.asset;
        asset.vault = vault.key();
        asset.mint = mint_key;
        asset.token_account = ctx.accounts.vault_token_account.key();
        asset.kind = AssetKind::SplToken;
        asset.oracle = ctx.accounts.oracle.key();
        asset.feed_id = feed_id;
        asset.pricing_adapter = Pubkey::default();
        asset.decimals = ctx.accounts.mint.decimals;
        asset.index = index;
        asset.enabled = true;
        asset.max_exposure_bps = max_exposure_bps;
        // Nothing has been promised to anyone yet. Write it explicitly rather than
        // relying on Anchor's zero-fill — an accounting field with a wrong initial
        // value is the kind of bug that is very hard to track down later.
        asset.reserved = 0;
        asset.bump = ctx.bumps.asset;

        vault.asset_mints.push(mint_key);
        vault.asset_count = index.checked_add(1).ok_or(TribeError::MathOverflow)?;

        Ok(())
    }

    /// Close a held position, freeing its slot so a different asset can be opened.
    ///
    /// A held slot is scarce (`MAX_ASSETS`). Once the vault has fully exited a
    /// position — its token balance AND its reserved amount are both zero — the
    /// slot serves no purpose and should be freed. This is the counterpart to
    /// registering an asset; together they make the held set a small, revolving
    /// window over a possibly huge whitelist.
    ///
    /// # Swap-remove keeps indices dense
    ///
    /// The asset being closed is at some index `k`. To avoid leaving a hole (which
    /// would break every `for i in 0..asset_count` loop and the redeem bitmap),
    /// the LAST held asset is moved into slot `k`: its `index` is rewritten to `k`
    /// and `asset_mints[k]` is overwritten, then the vec is popped.
    ///
    /// This is safe for any outstanding redeem ticket: a ticket snapshots
    /// `asset_mints` and `amounts` at burn time and claims against ITS OWN arrays
    /// and bitmap — never the live vault index — so reordering vault slots cannot
    /// corrupt it.
    pub fn close_position(ctx: Context<ClosePosition>) -> Result<()> {
        // Never strand money: the position must be fully empty. The token account
        // is read live (not trusted from any cached field), and `reserved` must be
        // zero so we are not closing a slot still owed to a pending redeemer.
        require!(
            ctx.accounts.vault_token_account.amount == 0,
            TribeError::AssetNotEmpty
        );
        require!(
            ctx.accounts.asset.reserved == 0,
            TribeError::AssetNotEmpty
        );

        let closing_index = ctx.accounts.asset.index;
        let vault = &mut ctx.accounts.vault;
        let last_index = vault
            .asset_count
            .checked_sub(1)
            .ok_or(TribeError::InvalidVaultState)?;

        require!(
            (closing_index as usize) < vault.asset_mints.len(),
            TribeError::AssetNotRegistered
        );

        if closing_index != last_index {
            // Move the last asset into the freed slot. The caller must pass the
            // last-index asset account so we can rewrite its stored index.
            let moved = ctx
                .accounts
                .last_asset
                .as_mut()
                .ok_or(TribeError::MissingLastAsset)?;
            require!(
                moved.index == last_index,
                TribeError::AssetNotRegistered
            );
            require_keys_eq!(
                vault.asset_mints[last_index as usize],
                moved.mint,
                TribeError::AssetNotRegistered
            );

            moved.index = closing_index;
            vault.asset_mints[closing_index as usize] = moved.mint;
        }

        vault.asset_mints.pop();
        vault.asset_count = last_index;

        Ok(())
    }

    /// Turn pause on/off.
    ///
    /// Blocks deposits and ENTRY actions (buy/lend/stake). It does NOT block redeem,
    /// and it does NOT block EXIT actions (sell/unlend/unstake) — if it did, assets
    /// already lent into Kamino would be permanently stuck there.
    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.vault.paused = paused;
        Ok(())
    }

    /// Hand the execute right over to another address.
    ///
    /// This is the one and only bridge from MVP to governance. MVP: executor = admin.
    /// Tier 3: call this once, pointing executor at the tribe-governance PDA — from
    /// then on "allowed to execute" means "the proposal passed + the 24h timelock has
    /// elapsed", and the admin can no longer move money on their own say-so.
    pub fn set_executor(ctx: Context<AdminOnly>, executor: Pubkey) -> Result<()> {
        ctx.accounts.vault.executor = executor;
        Ok(())
    }

    /// Register a permitted triple (adapter × action × asset).
    ///
    /// See `state::Capability` for why "permitted" cannot be a boolean flag on
    /// Asset.
    ///
    /// MVP: the admin registers these. But a capability OPENS A NEW MONEY PATH out
    /// of the vault — just as dangerous as adding an adapter — so it should probably
    /// move to governance as early as Tier 3, not wait for Tier 4.
    pub fn register_capability(
        ctx: Context<RegisterCapability>,
        action_id: ActionId,
        mint: Pubkey,
        is_entry: bool,
        venue: Pubkey,
        max_notional: u64,
    ) -> Result<()> {
        let clock = Clock::get()?;

        // The adapter must be of kind ACTION — a Pricing adapter does not move money.
        let adapter = &ctx.accounts.adapter;
        require!(
            adapter.kind == AdapterKind::Action,
            TribeError::WrongAdapterKind
        );

        // The adapter must genuinely be live: enabled and past its own 7-day timelock.
        // Do NOT allow granting rights to an adapter that is not yet in effect.
        require!(adapter.enabled, TribeError::AdapterDisabled);
        require!(
            clock.unix_timestamp >= adapter.active_at,
            TribeError::AdapterNotActive
        );

        // The input asset is identified by `mint` alone — it need NOT be held yet.
        // Whitelisting "may buy AAPLx" does not require the vault to already own AAPLx.

        // --- Life-or-death check: the receipt token MUST be visible to NAV ---
        //
        // The vault CANNOT infer from `action_id` whether this action produces a
        // receipt token — it does not understand the semantics. So governance must
        // DECLARE it: pass in `receipt_asset` if the action mints a kToken/LST.
        //
        // And if it is declared, the vault cross-checks it very strictly. Without this
        // check, the vault lends out 1M USDC, gets back a kToken that NAV knows nothing
        // about — NAV drops by exactly 1M, later depositors mint shares at an artificially
        // cheap price, and everyone currently holding shares is diluted by exactly 1M. A
        // lend that is perfectly valid at the CPI level still drains the vault, purely
        // because the accounting goes blind.
        let receipt_mint = match ctx.accounts.receipt_asset.as_ref() {
            Some(receipt) => {
                require_keys_eq!(
                    receipt.vault,
                    ctx.accounts.vault.key(),
                    TribeError::ReceiptAssetNotRegistered
                );

                // A receipt token is a POSITION, not an ordinary SPL token — if it were
                // an SplToken it would already have a Pyth feed and need no pricing adapter.
                require!(
                    receipt.kind != AssetKind::SplToken,
                    TribeError::ReceiptKindMismatch
                );

                // A position MUST have a pricing adapter, otherwise NAV fails outright the
                // moment it meets one — and the vault freezes right after the first lend.
                require!(
                    receipt.pricing_adapter != Pubkey::default(),
                    TribeError::PricingAdapterMismatch
                );

                receipt.mint
            }
            // A swap produces no new receipt — the vault still holds ordinary tokens.
            None => Pubkey::default(),
        };

        let capability = &mut ctx.accounts.capability;
        capability.vault = ctx.accounts.vault.key();
        capability.adapter = adapter.program_id;
        capability.action_id = action_id;
        capability.is_entry = is_entry;
        capability.mint = mint;
        capability.venue = venue;
        capability.receipt_mint = receipt_mint;
        capability.max_notional = max_notional;
        capability.enabled = true;
        capability.active_at = clock
            .unix_timestamp
            .checked_add(CAPABILITY_TIMELOCK_SECONDS)
            .ok_or(TribeError::MathOverflow)?;
        capability.bump = ctx.bumps.capability;

        emit!(CapabilityRegisteredEvent {
            adapter: adapter.program_id,
            action_id,
            is_entry,
            mint: capability.mint,
            venue,
            receipt_mint,
            max_notional,
            active_at: capability.active_at,
        });

        Ok(())
    }

    /// Disable a capability. Takes effect IMMEDIATELY, no timelock.
    ///
    /// Closing a money path out of the vault has to be fast. But note: disabling an
    /// `is_entry = true` capability (lend) does NOT lock up money already lent — the
    /// exit capability (`is_entry = false`, unlend) still runs, because the exit is
    /// never closed.
    pub fn disable_capability(ctx: Context<DisableCapability>) -> Result<()> {
        ctx.accounts.capability.enabled = false;
        Ok(())
    }

    /// Register a new adapter (Jupiter, Kamino, ...).
    ///
    /// An adapter is NOT usable right away: it must wait out a 7-day timelock. That is
    /// much longer than the ordinary transaction timelock (24h), and deliberately so —
    /// an adapter is granted control over the vault's money, so the community needs time
    /// to scrutinize it and redeem their way out if it looks suspicious.
    ///
    /// MVP: the admin registers these. Later: a governance vote.
    pub fn register_adapter(
        ctx: Context<RegisterAdapter>,
        program_id: Pubkey,
        kind: AdapterKind,
        label: String,
    ) -> Result<()> {
        require!(label.len() <= 32, TribeError::MathOverflow);

        let clock = Clock::get()?;
        let adapter = &mut ctx.accounts.adapter;

        adapter.vault = ctx.accounts.vault.key();
        adapter.program_id = program_id;
        // Action (untrusted, verifiable via NAV-delta) or Pricing (trusted, feeds straight
        // into NAV). Two fundamentally different trust levels — see AdapterKind.
        adapter.kind = kind;
        adapter.label = label;
        adapter.enabled = true;
        adapter.active_at = clock
            .unix_timestamp
            .checked_add(ADAPTER_TIMELOCK_SECONDS)
            .ok_or(TribeError::MathOverflow)?;
        adapter.bump = ctx.bumps.adapter;

        Ok(())
    }

    /// Disable an adapter. Takes effect IMMEDIATELY, no timelock.
    ///
    /// A deliberate asymmetry: opening the door is slow (7 days), closing it is instant.
    /// When a broken adapter is discovered, you have to stop the bleeding fast.
    pub fn disable_adapter(ctx: Context<DisableAdapter>) -> Result<()> {
        ctx.accounts.adapter.enabled = false;
        Ok(())
    }

    /// Execute an action on the vault's assets, through an adapter.
    ///
    /// ONE single gate for EVERY action — swap, lend, stake, LP, and even actions that
    /// do not exist yet. The vault does NOT understand the semantics of `action_id`; it
    /// only verifies the result via NAV-delta.
    ///
    /// That is why adding a new action does not require upgrading the vault: deploy a new
    /// adapter + have governance write one line into the registry, done.
    ///
    /// See `execute.rs`.
    pub fn execute_action<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteAction<'info>>,
        action_id: ActionId,
        amount_in: u64,
        out_feed_id: [u8; 32],
        payload: Vec<u8>,
    ) -> Result<()> {
        execute::execute_action(ctx, action_id, amount_in, out_feed_id, payload)
    }

    /// Enforce that every asset stays within its exposure cap.
    ///
    /// BUNDLE IT IN THE SAME TRANSACTION as `execute_action`:
    ///
    /// ```text
    ///   [ix 0]  execute_action(...)     <- swap
    ///   [ix 1]  assert_exposure()       <- check afterwards
    /// ```
    ///
    /// Same transaction → still atomic: if the cap is breached, the swap rolls back too.
    ///
    /// It has to be a separate instruction because exposure needs the full NAV (pricing
    /// EVERY asset), and `execute_action` has already run out of compute — Jupiter alone
    /// requests the full 1.4M CU ceiling. This instruction gets its own 200k CU.
    pub fn assert_exposure<'info>(
        ctx: Context<'_, '_, 'info, 'info, AssertExposure<'info>>,
    ) -> Result<()> {
        execute::assert_exposure(ctx)
    }

    /// Deposit assets into the vault, receive shares.
    ///
    /// remaining_accounts: the triple [Asset, vault token account, Pyth price] for EVERY
    /// registered asset, in exactly the order of `Vault::asset_mints`.
    pub fn deposit<'info>(
        ctx: Context<'_, '_, 'info, 'info, Deposit<'info>>,
        amount: u64,
    ) -> Result<()> {
        require!(!ctx.accounts.vault.paused, TribeError::VaultPaused);
        require!(amount >= MIN_DEPOSIT, TribeError::DepositTooSmall);

        let clock = Clock::get()?;

        // NAV MUST be computed BEFORE the depositor's tokens touch the vault.
        //
        // If it were computed afterwards, their money would already be inside NAV, so they
        // would be minted shares based on the very amount they just sent — diluting
        // themselves and everyone else. The ordering here is mandatory, not a matter of
        // style.
        let vault_key = ctx.accounts.vault.key();
        let snapshot = compute_nav(
            &ctx.accounts.vault,
            &vault_key,
            ctx.remaining_accounts,
            &clock,
        )?;
        let nav_before = snapshot.total_value;
        let total_shares = ctx.accounts.vault.total_shares;

        // Price the amount actually being deposited, using the very price validated above.
        let deposit_value = deposit_value_in_quote(
            &ctx.accounts.vault,
            &ctx.accounts.asset,
            ctx.remaining_accounts,
            amount,
            &clock,
        )?;

        let mut shares = shares_for_deposit(deposit_value, nav_before, total_shares)?;

        // First deposit: permanently lock away a small amount of shares.
        //
        // This blocks the inflation attack: without it, an attacker deposits 1 unit
        // (receiving 1 share), then transfers a large amount of tokens directly into the
        // vault to inflate the share price. The next depositor gets rounded down to 0
        // shares, and their money falls into the attacker's hands. Hard-locking
        // MINIMUM_LIQUIDITY makes the attack unprofitable.
        if total_shares == 0 {
            require!(shares > MINIMUM_LIQUIDITY, TribeError::DepositTooSmall);
            shares = shares
                .checked_sub(MINIMUM_LIQUIDITY)
                .ok_or(TribeError::MathOverflow)?;
            ctx.accounts.vault.total_shares = MINIMUM_LIQUIDITY;
        }

        require!(shares > 0, TribeError::ZeroSharesMinted);

        // Transfer the tokens into the vault first, mint shares afterwards.
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.depositor_token_account.to_account_info(),
                    mint: ctx.accounts.deposit_mint.to_account_info(),
                    to: ctx.accounts.vault_token_account.to_account_info(),
                    authority: ctx.accounts.depositor.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.deposit_mint.decimals,
        )?;

        let authority_bump = ctx.accounts.vault.vault_authority_bump;
        let authority_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            vault_key.as_ref(),
            &[authority_bump],
        ];

        token_interface::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                token_interface::MintTo {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    to: ctx.accounts.depositor_share_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
                &[authority_seeds],
            ),
            shares,
        )?;

        let vault = &mut ctx.accounts.vault;
        vault.total_shares = vault
            .total_shares
            .checked_add(shares)
            .ok_or(TribeError::MathOverflow)?;

        emit!(DepositEvent {
            depositor: ctx.accounts.depositor.key(),
            mint: ctx.accounts.deposit_mint.key(),
            amount,
            value: deposit_value,
            shares_minted: shares,
            nav_before,
            total_shares: vault.total_shares,
        });

        Ok(())
    }

    /// Step 1/3 of redeem: burn shares, lock in the assets to be received.
    ///
    /// NOT blocked by pause — users can always get their money out.
    ///
    /// The vault holds up to 24 assets, so it cannot pay them all out in one transaction.
    /// This instruction burns the shares and writes out an immutable "claim ticket". The
    /// amounts are locked in here and do not depend on later NAV: whatever prices do
    /// afterwards, the redeemer still receives exactly their pro-rata share of the assets
    /// as of the moment of the burn.
    pub fn redeem_request<'info>(
        ctx: Context<'_, '_, 'info, 'info, RedeemRequest<'info>>,
        shares: u64,
    ) -> Result<()> {
        require!(shares > 0, TribeError::ZeroRedeem);

        let clock = Clock::get()?;
        let vault_key = ctx.accounts.vault.key();
        let total_shares = ctx.accounts.vault.total_shares;
        let asset_count = ctx.accounts.vault.asset_count as usize;

        require!(total_shares > 0, TribeError::InvalidVaultState);
        require!(
            ctx.accounts.owner_share_account.amount >= shares,
            TribeError::InsufficientShares
        );

        // The AVAILABLE balance, read from the real token accounts, minus whatever has
        // already been promised to earlier redeemers.
        //
        // Do NOT use the oracle here. Redeem is a pure pro-rata division — price does not
        // appear anywhere in the formula. Calling the oracle would make redeem fail when
        // prices go stale, i.e. exactly when the market is in chaos and users need to get
        // their money out most. See `nav::available_balances`.
        let balances = available_balances(
            &ctx.accounts.vault,
            &vault_key,
            ctx.remaining_accounts,
        )?;

        // Lock in the amount owed for each asset, rounding down. The dust stays in the
        // vault for whoever still holds shares.
        let mut amounts = Vec::with_capacity(asset_count);
        let mut remaining_count: u8 = 0;
        for i in 0..asset_count {
            let amount = pro_rata_amount(balances[i], shares, total_shares)?;
            if amount > 0 {
                remaining_count = remaining_count
                    .checked_add(1)
                    .ok_or(TribeError::MathOverflow)?;

                // LOCK this portion away. From now until the user claims it, nobody may
                // touch it: NAV does not count it, and `execute` cannot spend it.
                //
                // Without this step, `execute` could swap away the exact tokens that were
                // just promised — by claim time the vault no longer has enough, the shares
                // are already burned, and the assets are stuck inside a worthless ticket.
                add_reserved(&ctx.remaining_accounts[i * 2], amount)?;
            }
            amounts.push(amount);
        }

        token_interface::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                token_interface::Burn {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    from: ctx.accounts.owner_share_account.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            shares,
        )?;

        let ticket_id = ctx.accounts.vault.redeem_ticket_counter;
        let asset_mints = ctx.accounts.vault.asset_mints.clone();

        let ticket = &mut ctx.accounts.ticket;
        ticket.vault = vault_key;
        ticket.owner = ctx.accounts.owner.key();
        ticket.ticket_id = ticket_id;
        ticket.shares_burned = shares;
        ticket.amounts = amounts;
        ticket.asset_mints = asset_mints;
        ticket.claimed_mask = 0;
        ticket.remaining_count = remaining_count;
        ticket.created_at = clock.unix_timestamp;
        ticket.bump = ctx.bumps.ticket;

        let vault = &mut ctx.accounts.vault;
        vault.total_shares = vault
            .total_shares
            .checked_sub(shares)
            .ok_or(TribeError::MathOverflow)?;
        vault.redeem_ticket_counter = vault
            .redeem_ticket_counter
            .checked_add(1)
            .ok_or(TribeError::MathOverflow)?;

        emit!(RedeemRequestEvent {
            owner: ctx.accounts.owner.key(),
            ticket_id,
            shares_burned: shares,
            asset_count: remaining_count,
        });

        Ok(())
    }

    /// Step 2/3: withdraw one asset from the ticket. Call repeatedly until none are left.
    ///
    /// Double-claim is prevented by the bitmap in the ticket. This is the single most
    /// important guard in redeem: break it and one ticket can withdraw the same asset
    /// multiple times.
    pub fn claim_asset(ctx: Context<ClaimAsset>, asset_index: u8) -> Result<()> {
        let idx = asset_index as usize;

        require!(
            idx < ctx.accounts.ticket.amounts.len(),
            TribeError::TicketAssetMismatch
        );
        require!(
            !ctx.accounts.ticket.is_claimed(asset_index),
            TribeError::AssetAlreadyClaimed
        );

        // The asset must match the SNAPSHOT taken when the ticket was created, not the
        // vault's current list. Even if the admin adds/removes assets in the meantime, the
        // ticket still pays out exactly what was promised when the shares were burned.
        require!(
            ctx.accounts.asset.mint == ctx.accounts.ticket.asset_mints[idx],
            TribeError::TicketAssetMismatch
        );

        let amount = ctx.accounts.ticket.amounts[idx];

        let vault_key = ctx.accounts.vault.key();
        let authority_bump = ctx.accounts.vault.vault_authority_bump;
        let authority_seeds: &[&[u8]] = &[
            VAULT_AUTHORITY_SEED,
            vault_key.as_ref(),
            &[authority_bump],
        ];

        if amount > 0 {
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.vault_token_account.to_account_info(),
                        mint: ctx.accounts.mint.to_account_info(),
                        to: ctx.accounts.owner_token_account.to_account_info(),
                        authority: ctx.accounts.vault_authority.to_account_info(),
                    },
                    &[authority_seeds],
                ),
                amount,
                ctx.accounts.mint.decimals,
            )?;

            // Release the lock: the tokens have left the vault, so the "promised but not
            // yet paid" amount goes down.
            //
            // This MUST match EXACTLY the amount added during redeem_request. If we
            // subtract too little, `reserved` inflates permanently and gradually locks the
            // whole vault dead — NAV drifts downward, and adapters can no longer spend any
            // assets at all.
            let asset = &mut ctx.accounts.asset;
            asset.reserved = asset
                .reserved
                .checked_sub(amount)
                .ok_or(TribeError::MathOverflow)?;
        }

        let ticket = &mut ctx.accounts.ticket;
        ticket.mark_claimed(asset_index);
        if amount > 0 {
            ticket.remaining_count = ticket
                .remaining_count
                .checked_sub(1)
                .ok_or(TribeError::MathOverflow)?;
        }

        emit!(ClaimAssetEvent {
            owner: ticket.owner,
            ticket_id: ticket.ticket_id,
            mint: ctx.accounts.mint.key(),
            amount,
            remaining: ticket.remaining_count,
        });

        Ok(())
    }

    /// Step 3/3: close a fully claimed ticket, refunding the rent to its owner.
    pub fn close_ticket(ctx: Context<CloseTicket>) -> Result<()> {
        require!(
            ctx.accounts.ticket.remaining_count == 0,
            TribeError::TicketNotFullyClaimed
        );
        Ok(())
    }
}

/// Price exactly the amount of tokens being deposited.
///
/// Reuses the very oracle already validated in `compute_nav` — it does not read a separate
/// price, so that the price used for NAV and the price used for the deposit are always one
/// and the same. Taking two different prices within a single instruction opens the door to
/// an exploitable discrepancy.
fn deposit_value_in_quote<'info>(
    vault: &Account<'info, Vault>,
    asset: &Account<'info, Asset>,
    remaining: &'info [AccountInfo<'info>],
    amount: u64,
    clock: &Clock,
) -> Result<u64> {
    let idx = asset.index as usize;
    require!(
        idx < vault.asset_count as usize,
        TribeError::AssetNotRegistered
    );

    // This asset's oracle sits in the 3rd slot of its triple.
    let oracle_info: &AccountInfo<'info> = &remaining[idx * 3 + 2];
    require_keys_eq!(
        asset.oracle,
        oracle_info.key(),
        TribeError::OracleFeedMismatch
    );

    let price_update: Account<pyth_solana_receiver_sdk::price_update::PriceUpdateV2> =
        Account::try_from(oracle_info)?;
    let price = oracle::get_validated_price(&price_update, &asset.feed_id, clock)?;

    math::asset_value_in_quote(
        amount,
        price.price,
        price.expo,
        asset.decimals,
        QUOTE_DECIMALS,
    )
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + Vault::INIT_SPACE,
        seeds = [VAULT_SEED],
        bump
    )]
    pub vault: Box<Account<'info, Vault>>,

    /// The PDA that owns every token account of the vault. Nobody holds its private key.
    /// CHECK: a pure PDA, used only to sign CPIs.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, vault.key().as_ref()],
        bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    /// The share mint. Mint authority lives on vault_authority — if it lived anywhere
    /// else, someone could print shares out of thin air and drain the vault.
    #[account(
        init,
        payer = admin,
        mint::decimals = SHARE_DECIMALS,
        mint::authority = vault_authority,
    )]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    /// CHECK: we only store the fee-recipient address, we never read its data.
    pub treasury: UncheckedAccount<'info>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterAsset<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED],
        bump = vault.bump,
        has_one = admin @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,

    /// CHECK: PDA, used as the authority of the token account.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    pub mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        init,
        payer = admin,
        space = 8 + Asset::INIT_SPACE,
        seeds = [ASSET_SEED, vault.key().as_ref(), mint.key().as_ref()],
        bump
    )]
    pub asset: Box<Account<'info, Asset>>,

    /// The vault's token account for this asset.
    ///
    /// It MUST be the canonical **Associated Token Account** of vault_authority. Two
    /// separate reasons, and both are load-bearing:
    ///
    /// 1. **The authority must be vault_authority** — otherwise the money sits under
    ///    someone else's control.
    ///
    /// 2. **The address must be the ATA** — otherwise the vault cannot trade on any real
    ///    DEX. Jupiter (and every other DEX) derives the user's token accounts as ATAs
    ///    and bakes those exact addresses into the instruction it hands back. Give it a
    ///    token account at some other address and it fails account validation before a
    ///    single lamport moves.
    ///
    ///    This was found the hard way: registering assets with freshly generated keypair
    ///    accounts worked perfectly against a mock DEX (which used whatever accounts it
    ///    was given) and then failed against the real Jupiter with error 0x1789.
    ///
    /// A vault that wants to route through real venues does not get to choose where its
    /// token accounts live. They have to be where the rest of Solana looks for them.
    #[account(
        init,
        payer = admin,
        associated_token::mint = mint,
        associated_token::authority = vault_authority,
    )]
    pub vault_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: Pyth price update. Fully validated on every price read (feed_id, staleness,
    /// confidence) in `oracle::get_validated_price`.
    pub oracle: UncheckedAccount<'info>,

    pub associated_token_program: Program<'info, AssociatedToken>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED],
        bump = vault.bump,
        has_one = admin @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,

    /// The position being closed. Rent is refunded to the admin. Its seeds bind it
    /// to this vault and mint, and `close` reclaims the account.
    #[account(
        mut,
        close = admin,
        seeds = [ASSET_SEED, vault.key().as_ref(), asset.mint.as_ref()],
        bump = asset.bump,
        constraint = asset.vault == vault.key() @ TribeError::AssetNotRegistered,
    )]
    pub asset: Box<Account<'info, Asset>>,

    /// The vault's token account for this asset — read to prove the balance is 0.
    #[account(
        address = asset.token_account @ TribeError::AssetNotRegistered,
    )]
    pub vault_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The asset currently at the LAST index. Required only when the asset being
    /// closed is not itself the last one — it gets moved into the freed slot, so
    /// its stored index must be rewritten. Optional so closing the last asset
    /// needs no extra account.
    #[account(
        mut,
        seeds = [ASSET_SEED, vault.key().as_ref(), last_asset.mint.as_ref()],
        bump = last_asset.bump,
        constraint = last_asset.vault == vault.key() @ TribeError::AssetNotRegistered,
    )]
    pub last_asset: Option<Box<Account<'info, Asset>>>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED],
        bump = vault.bump,
        has_one = admin @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,
}

#[derive(Accounts)]
#[instruction(action_id: ActionId, mint: Pubkey)]
pub struct RegisterCapability<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        seeds = [VAULT_SEED],
        bump = vault.bump,
        has_one = admin @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,

    #[account(
        seeds = [ADAPTER_SEED, vault.key().as_ref(), adapter.program_id.as_ref()],
        bump = adapter.bump,
        constraint = adapter.vault == vault.key() @ TribeError::AdapterNotRegistered,
    )]
    pub adapter: Box<Account<'info, Adapter>>,

    // NOTE: the input asset is identified by the `mint` PARAMETER, not by a
    // pre-existing Asset account. A capability is the WHITELIST — permission to
    // trade a mint — and must be registrable for thousands of mints the vault
    // does not (yet) hold. The Asset (held slot) is opened later, when the vault
    // first acquires the position. See DESIGN-WHITELIST-VS-HELD.md.

    /// The Asset of the receipt token (kToken, LST). It MUST be present when the action is
    /// Lend or Stake. A receipt token is a HELD position by nature (it must be priced by a
    /// pricing adapter), so its Asset slot must already exist — unlike the input mint.
    /// See `Capability::receipt_mint` — without it, NAV goes blind and the vault is diluted
    /// by exactly the amount taken out to lend.
    #[account(
        seeds = [ASSET_SEED, vault.key().as_ref(), receipt_asset.mint.as_ref()],
        bump = receipt_asset.bump,
    )]
    pub receipt_asset: Option<Box<Account<'info, Asset>>>,

    #[account(
        init,
        payer = admin,
        space = 8 + Capability::INIT_SPACE,
        seeds = [
            CAPABILITY_SEED,
            vault.key().as_ref(),
            adapter.program_id.as_ref(),
            &[action_id],
            mint.as_ref(),
        ],
        bump
    )]
    pub capability: Box<Account<'info, Capability>>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DisableCapability<'info> {
    pub admin: Signer<'info>,

    #[account(
        seeds = [VAULT_SEED],
        bump = vault.bump,
        has_one = admin @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,

    #[account(
        mut,
        seeds = [
            CAPABILITY_SEED,
            vault.key().as_ref(),
            capability.adapter.as_ref(),
            &[capability.action_id],
            capability.mint.as_ref(),
        ],
        bump = capability.bump,
        constraint = capability.vault == vault.key() @ TribeError::CapabilityNotRegistered,
    )]
    pub capability: Box<Account<'info, Capability>>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED],
        bump = vault.bump,
    )]
    pub vault: Box<Account<'info, Vault>>,

    /// CHECK: the PDA that signs for minting shares.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        seeds = [ASSET_SEED, vault.key().as_ref(), deposit_mint.key().as_ref()],
        bump = asset.bump,
        constraint = asset.vault == vault.key() @ TribeError::AssetNotRegistered,
        constraint = asset.enabled @ TribeError::AssetDisabled,
    )]
    pub asset: Box<Account<'info, Asset>>,

    pub deposit_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        constraint = depositor_token_account.mint == deposit_mint.key(),
        constraint = depositor_token_account.owner == depositor.key(),
    )]
    pub depositor_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        address = asset.token_account @ TribeError::AssetNotRegistered,
    )]
    pub vault_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut, address = vault.share_mint)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        constraint = depositor_share_account.mint == vault.share_mint,
        constraint = depositor_share_account.owner == depositor.key(),
    )]
    pub depositor_share_account: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct RedeemRequest<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    // Note: there is NO `paused` check here. That is deliberate.
    // Redeem must keep working even when the protocol is frozen.
    #[account(
        mut,
        seeds = [VAULT_SEED],
        bump = vault.bump,
    )]
    pub vault: Box<Account<'info, Vault>>,

    #[account(
        init,
        payer = owner,
        space = 8 + RedeemTicket::INIT_SPACE,
        seeds = [
            REDEEM_TICKET_SEED,
            vault.key().as_ref(),
            owner.key().as_ref(),
            &vault.redeem_ticket_counter.to_le_bytes(),
        ],
        bump
    )]
    pub ticket: Box<Account<'info, RedeemTicket>>,

    #[account(mut, address = vault.share_mint)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        constraint = owner_share_account.mint == vault.share_mint,
        constraint = owner_share_account.owner == owner.key(),
    )]
    pub owner_share_account: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimAsset<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        seeds = [VAULT_SEED],
        bump = vault.bump,
    )]
    pub vault: Box<Account<'info, Vault>>,

    /// CHECK: the PDA that signs for transferring assets out.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        has_one = owner @ TribeError::Unauthorized,
        has_one = vault @ TribeError::Unauthorized,
        seeds = [
            REDEEM_TICKET_SEED,
            vault.key().as_ref(),
            owner.key().as_ref(),
            &ticket.ticket_id.to_le_bytes(),
        ],
        bump = ticket.bump,
    )]
    pub ticket: Box<Account<'info, RedeemTicket>>,

    /// `mut` because claim_asset decrements `asset.reserved` — releasing the lock on the
    /// portion just paid out to the user.
    #[account(
        mut,
        seeds = [ASSET_SEED, vault.key().as_ref(), mint.key().as_ref()],
        bump = asset.bump,
        constraint = asset.vault == vault.key() @ TribeError::AssetNotRegistered,
    )]
    pub asset: Box<Account<'info, Asset>>,

    pub mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        address = asset.token_account @ TribeError::AssetNotRegistered,
    )]
    pub vault_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = owner_token_account.mint == mint.key(),
        constraint = owner_token_account.owner == owner.key(),
    )]
    pub owner_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CloseTicket<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        seeds = [VAULT_SEED],
        bump = vault.bump,
    )]
    pub vault: Box<Account<'info, Vault>>,

    #[account(
        mut,
        close = owner,
        has_one = owner @ TribeError::Unauthorized,
        has_one = vault @ TribeError::Unauthorized,
        seeds = [
            REDEEM_TICKET_SEED,
            vault.key().as_ref(),
            owner.key().as_ref(),
            &ticket.ticket_id.to_le_bytes(),
        ],
        bump = ticket.bump,
    )]
    pub ticket: Box<Account<'info, RedeemTicket>>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[event]
pub struct CapabilityRegisteredEvent {
    pub adapter: Pubkey,
    pub action_id: ActionId,
    pub is_entry: bool,
    pub mint: Pubkey,
    pub venue: Pubkey,
    pub receipt_mint: Pubkey,
    pub max_notional: u64,
    pub active_at: i64,
}

#[event]
pub struct DepositEvent {
    pub depositor: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub value: u64,
    pub shares_minted: u64,
    pub nav_before: u64,
    pub total_shares: u64,
}

#[event]
pub struct RedeemRequestEvent {
    pub owner: Pubkey,
    pub ticket_id: u64,
    pub shares_burned: u64,
    pub asset_count: u8,
}

#[event]
pub struct ClaimAssetEvent {
    pub owner: Pubkey,
    pub ticket_id: u64,
    pub mint: Pubkey,
    pub amount: u64,
    pub remaining: u8,
}
