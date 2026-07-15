use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::constants::*;
use crate::errors::TribeError;
use crate::nav::{available_balances, available_balances_with_fresh, compute_nav, value_from_balance, value_of};
use crate::state::{ActionId, Adapter, AdapterKind, AssetKind, Asset, Capability, Vault};

/// # execute_action — where money leaves the vault
///
/// The most dangerous instruction in the protocol. It CPIs into an external program
/// and hands that program the vault's PDA signing authority so it can move the
/// vault's assets.
///
/// ## The vault does NOT understand the semantics of an action
///
/// This is the central principle, and it is a sharp break from the previous version
/// of this file.
///
/// The vault does not know whether `action_id = 0` means "swap" or "lend" or "stake".
/// It does not compute `min_out`, it does not know what slippage on a swap looks
/// like, it does not know what a kToken is. All of the semantics live in the
/// **adapter**.
///
/// Why it must be this way: if the vault understood swaps, then adding lending would
/// mean modifying the vault — that is, **upgrading the program that holds the money**.
/// Exactly what this whole architecture exists to avoid. Adding a new action must be:
/// deploy a new adapter + governance writes one line into the registry. The vault
/// does not get a single line of code touched.
///
/// ## The vault only verifies the RESULT
///
/// Every action — swap, lend, stake, LP — ultimately is just **a shift across the set
/// of assets**: some assets go down, some go up. The vault does not need to understand
/// why; it only needs to know whether the result is acceptable:
///
/// ```text
///   1. balance of every OTHER asset unchanged   adapter did not secretly touch a third asset
///   2. spent <= amount_in                       did not spend more than allowed
///   3. value(in+out) lost no more than slippage the vault did not get poorer
///   4. balance >= reserved                      did not touch money promised to redeemers
/// ```
///
/// These four checks are **agnostic** — they hold for every action, today and in the
/// future.
///
/// ## The compute budget dictates the shape of these measurements
///
/// Jupiter *requests* `computeUnitLimit = 1_400_000` — **the absolute ceiling on
/// Solana** — regardless of how complex the route actually is. What it *consumes* is
/// far less (measured by simulating on mainnet: ~508k CU for a real 3-hop swap, ~72k
/// CU for a 1-hop). So there is real compute left over — but not an unbounded amount,
/// and pricing every asset twice scales with the asset count while the compute budget
/// does not.
///
/// So the measurements are split by their PRICE:
///
/// ```text
///   BALANCE (just read the token account, cheap)  -> ALL assets
///   VALUE   (must decode Pyth, expensive)         -> only asset_in + asset_out
/// ```
///
/// We still **detect** an adapter touching a third asset (its balance changes); all we
/// give up is **pricing** that asset — which we do not need anyway, because if the
/// balance did not change then its value cancels out of both sides of the comparison.
///
/// The **exposure** cap needs the full NAV, so it **cannot** be checked here — it is
/// split out into `assert_exposure`, a separate instruction with its own 200k CU,
/// bundled into the same transaction. See that function.
///
/// ## The adapter is UNTRUSTED
///
/// Precisely because everything can be verified from balances + value-delta, the vault
/// **does not need to trust** the adapter. The adapter may have a bug, may be
/// malicious, or the target protocol may be under attack — all of it gets caught by
/// steps 1–4, and the transaction reverts.
///
/// (A pricing adapter is the opposite: it feeds straight into NAV, there is nothing to
/// measure against, so it is TRUSTED and must be audited like core. See `AdapterKind`.)
pub fn execute_action<'info>(
    ctx: Context<'_, '_, 'info, 'info, ExecuteAction<'info>>,
    action_id: ActionId,
    amount_in: u64,
    // Pyth feed id for the received asset — used ONLY when lazily opening a fresh
    // slot, to bind its oracle. Ignored for an already-held asset (which has its own).
    out_feed_id: [u8; 32],
    payload: Vec<u8>,
) -> Result<()> {
    require!(amount_in > 0, TribeError::ZeroAmount);

    let clock = Clock::get()?;
    let vault_key = ctx.accounts.vault.key();

    // --- The adapter must be of kind ACTION, enabled, and past its timelock ---

    let adapter = &ctx.accounts.adapter;

    // A pricing adapter must NEVER be used to execute. It is TRUSTED (it feeds
    // straight into NAV) — giving it the power to move money collapses two trust
    // levels into one, and destroys the very reason an Action adapter is safe.
    require!(
        adapter.kind == AdapterKind::Action,
        TribeError::WrongAdapterKind
    );
    require!(adapter.enabled, TribeError::AdapterDisabled);
    require!(
        clock.unix_timestamp >= adapter.active_at,
        TribeError::AdapterNotActive
    );
    // CPI only into the exact program that was registered. Without this check the
    // caller passes in any program at all and the vault becomes a proxy for malicious
    // code.
    require_keys_eq!(
        adapter.program_id,
        ctx.accounts.adapter_program.key(),
        TribeError::AdapterProgramMismatch
    );

    // --- Capability: is the triple (adapter × action × asset) whitelisted? ---
    //
    // This is the guard that a boolean flag on Asset simply cannot provide. An Asset
    // only says "may this be held"; a capability says "WHAT may be done with it, and
    // WHERE".
    //
    //   stake × jito × ETH -> that capability was never registered -> the PDA does not
    //                         exist -> Anchor fails right at seed resolution.
    let capability = &ctx.accounts.capability;
    require_keys_eq!(
        capability.vault,
        vault_key,
        TribeError::CapabilityNotRegistered
    );
    require_keys_eq!(
        capability.adapter,
        adapter.program_id,
        TribeError::CapabilityMismatch
    );
    require!(
        capability.action_id == action_id,
        TribeError::CapabilityMismatch
    );
    require_keys_eq!(
        capability.mint,
        ctx.accounts.asset_in.mint,
        TribeError::CapabilityMismatch
    );

    // --- Pause and capability-disabled block the ENTRY side ONLY ---
    //
    // The vault does not know whether this action is a "buy" or an "unlend" — it reads
    // the `is_entry` flag that governance set when the capability was registered.
    //
    // The EXIT side never closes: if the vault has already lent ETH into Kamino and
    // then the protocol gets paused (or that capability gets disabled), and Unlend were
    // blocked along with it, the money would be STUCK FOREVER inside Kamino. Same
    // philosophy as "redeem is never pausable".
    if capability.is_entry {
        require!(!ctx.accounts.vault.paused, TribeError::VaultPaused);
        require!(capability.enabled, TribeError::CapabilityDisabled);
        require!(
            clock.unix_timestamp >= capability.active_at,
            TribeError::CapabilityNotActive
        );
    }

    // --- Lazy-open the received-asset slot (guarded re-init) ---
    //
    // `asset_out` is `init_if_needed`. Two cases, distinguished by whether its `vault`
    // field is still zero (a brand-new account) or already set (a held position):
    //
    //   FRESH  -> populate it as a new held slot: assign the next index, push the mint,
    //             bump asset_count. This is the ONLY place a slot is opened, so the
    //             held-set stays under governance's control (execute is executor-gated).
    //   EXISTS -> leave it; just re-validate below like any held asset.
    //
    // The guard is what makes init_if_needed safe here: a re-init cannot overwrite an
    // existing slot's accounting, because we only write the fields when vault == default.
    // Index of a slot opened THIS instruction, if any — its Asset account data is not
    // flushed yet, so the balance meter must read it specially (see nav.rs).
    let mut fresh_index: Option<usize> = None;
    {
        let out = &mut ctx.accounts.asset_out;
        if out.vault == Pubkey::default() {
            // A fresh position must be an ordinary SPL token — it is priced by an oracle.
            // Receipt positions (lend/stake) are opened by register_asset with a pricing
            // adapter, never lazily here.
            let held = ctx.accounts.vault.asset_count as usize;
            require!(held < MAX_ASSETS, TribeError::TooManyPositions);

            out.vault = vault_key;
            out.mint = ctx.accounts.out_mint.key();
            out.token_account = ctx.accounts.vault_out_token_account.key();
            out.kind = AssetKind::SplToken;
            out.oracle = ctx.accounts.out_oracle.key();
            out.feed_id = out_feed_id;
            out.pricing_adapter = Pubkey::default();
            out.decimals = ctx.accounts.out_mint.decimals;
            out.index = ctx.accounts.vault.asset_count;
            out.enabled = true;
            out.max_exposure_bps = 0;
            out.reserved = 0;
            out.bump = ctx.bumps.asset_out;

            fresh_index = Some(ctx.accounts.vault.asset_count as usize);

            let vault = &mut ctx.accounts.vault;
            vault.asset_mints.push(ctx.accounts.out_mint.key());
            vault.asset_count = vault
                .asset_count
                .checked_add(1)
                .ok_or(TribeError::MathOverflow)?;

        } else {
            // Existing slot: it must belong to this vault and be enabled (buying INTO a
            // disabled asset is blocked; selling OUT is not, but that path uses a
            // different asset as asset_out).
            require_keys_eq!(out.vault, vault_key, TribeError::AssetNotRegistered);
            require!(out.enabled, TribeError::AssetDisabled);
            require_keys_eq!(
                out.mint,
                ctx.accounts.out_mint.key(),
                TribeError::AssetNotRegistered
            );
        }
    }

    // The vault's ATA for the received asset — used to bind the fresh slot's balance
    // read (its Asset account data is not flushed yet, so we cannot read it from there).
    let out_token_account_key = ctx.accounts.vault_out_token_account.key();

    // --- Split remaining_accounts into THREE regions ---
    //
    // [0, 2N)      -> the (Asset, token account) pair for EVERY asset. BALANCE measurement — cheap.
    // [2N, 2N+2)   -> the oracles for asset_in and asset_out. VALUE measurement — expensive.
    // [2N+2, ..)   -> the ADAPTER's accounts (pool, tick array...). The vault does not understand these.
    //
    // # Why we do not price ALL assets (as the original design did)
    //
    // Jupiter *requests* `computeUnitLimit = 1_400_000` — the ABSOLUTE CEILING on
    // Solana — regardless of route complexity. But that is only the request: what a
    // swap actually *consumes* is far less (measured by simulating on mainnet: ~508k CU
    // for a real 3-hop route, ~72k CU for a 1-hop). So there is compute left over.
    //
    // What there is not, is compute that grows with the number of assets. A full NAV
    // costs ~6k CU per asset per pass, and we need two passes (before and after):
    // × 24 assets × 2 passes = ~288k CU, and that bill scales with the asset count
    // while the transaction's compute budget stays fixed. Pricing every asset twice is
    // the one cost that gets worse as the vault grows.
    //
    // So the measurements are split by their PRICE:
    //
    //   BALANCE — just read the token account, ~10× cheaper -> ALL assets
    //   VALUE   — must decode Pyth, expensive               -> only asset_in + asset_out
    //
    // We still catch an adapter secretly touching a third asset (its balance changes);
    // all we give up is PRICING that asset — we do not give up DETECTING it.
    let n = ctx.accounts.vault.asset_count as usize;
    let bal_len = n.checked_mul(2).ok_or(TribeError::MathOverflow)?;
    let meter_len = bal_len.checked_add(2).ok_or(TribeError::MathOverflow)?;

    require!(
        ctx.remaining_accounts.len() > meter_len,
        TribeError::IncompleteAssetSet
    );

    let bal_accounts = &ctx.remaining_accounts[..bal_len];
    let oracle_in = &ctx.remaining_accounts[bal_len];
    let oracle_out = &ctx.remaining_accounts[bal_len + 1];
    let adapter_accounts = &ctx.remaining_accounts[meter_len..];

    // --- BALANCES BEFORE: all assets (cheap) ---
    //
    // This is what catches an adapter touching an asset it never declared.
    let bal_before = available_balances_with_fresh(
        &ctx.accounts.vault,
        &vault_key,
        bal_accounts,
        fresh_index.map(|i| (i, &out_token_account_key)),
    )?;

    // --- VALUES BEFORE: only the two assets involved (expensive) ---

    let idx_in = ctx.accounts.asset_in.index as usize;
    let idx_out = ctx.accounts.asset_out.index as usize;
    require!(idx_in < MAX_ASSETS, TribeError::AssetNotRegistered);
    require!(idx_out < MAX_ASSETS, TribeError::AssetNotRegistered);
    // Swapping an asset into itself is meaningless, and it corrupts the measurement
    // (one token account would be both the source and the destination).
    require!(idx_in != idx_out, TribeError::DuplicateAsset);

    let (_, val_in_before) = value_of(
        &ctx.accounts.asset_in,
        &bal_accounts[idx_in * 2 + 1],
        oracle_in,
        &clock,
    )?;
    // A freshly-opened slot is empty (balance 0), and its token account did not exist
    // before this instruction — so there is nothing to read, and its value-before is 0.
    // Reading `bal_accounts[idx_out*2+1]` here would hit the not-yet-created ATA (a
    // system-owned empty account) and fail the TokenAccount deserialize.
    let val_out_before = if fresh_index == Some(idx_out) {
        0u64
    } else {
        value_of(
            &ctx.accounts.asset_out,
            &bal_accounts[idx_out * 2 + 1],
            oracle_out,
            &clock,
        )?
        .1
    };

    // --- The adapter may only spend the AVAILABLE portion ---
    //
    // `reserved` is the portion already promised to people who have burned their shares
    // but have not finished claiming. Without this check, an execute could swap away
    // exactly the tokens that were promised — and by the time they come to claim, the
    // vault no longer has enough: their shares are already burned and their assets are
    // stuck in a worthless claim ticket.
    //
    // `bal_before[]` ALREADY has reserved subtracted out, so it *is* the available
    // balance.
    let balance_in = bal_before[idx_in];
    require!(
        balance_in >= amount_in,
        TribeError::InsufficientAvailableBalance
    );
    require!(balance_in > 0, TribeError::InsufficientAvailableBalance);

    // --- Notional cap: a single order must not be able to sweep the whole vault ---
    //
    // Defense in depth. The value-delta check (below) catches "this trade destroyed
    // value", but it does NOT catch "this trade is perfectly valid, but amount_in is
    // the entire vault". If the adapter has a bug, or the target protocol gets hacked,
    // the maximum damage is capped at this number instead of at the whole NAV.
    //
    // Derived from the measurement we just took — we do NOT read the oracle a second
    // time (two reads within one instruction opens the door to an exploitable
    // discrepancy between them).
    let value_in = (val_in_before as u128)
        .checked_mul(amount_in as u128)
        .ok_or(TribeError::MathOverflow)?
        .checked_div(balance_in as u128)
        .ok_or(TribeError::MathOverflow)? as u64;

    if capability.max_notional > 0 {
        require!(
            value_in <= capability.max_notional,
            TribeError::NotionalExceeded
        );
    }

    // --- Build the account list for the CPI ---
    //
    // The vault does NOT impose an ordering. Every protocol has its own layout — the
    // vault cannot guess it, and does not need to: the client builds the list (it
    // already called the protocol's API to get the route, so it has one), and the vault
    // just forwards it verbatim.
    //
    // But the vault keeps EXACTLY ONE right, and this is the guard it will not
    // compromise on: it decides for itself which account gets signed for.
    let authority_key = ctx.accounts.vault_authority.key();
    let mut authority_present = false;

    let mut metas: Vec<AccountMeta> = Vec::with_capacity(adapter_accounts.len());
    let mut infos: Vec<AccountInfo<'info>> = Vec::with_capacity(adapter_accounts.len());

    for acc in adapter_accounts.iter() {
        let key = acc.key();

        // The vault's signing authority is granted to the vault_authority and NOTHING
        // else. Every other account always gets is_signer = false, no matter what the
        // client declared.
        //
        // Without this check, the client could hand in any account at all with the
        // signer flag set, and the vault would sign on its behalf — that is, lend out
        // its own authority to do anything, anywhere.
        let is_signer = key == authority_key;
        if is_signer {
            authority_present = true;
        }

        metas.push(AccountMeta {
            pubkey: key,
            is_signer,
            is_writable: acc.is_writable,
        });
        infos.push(acc.clone());
    }

    require!(authority_present, TribeError::MissingVaultAuthority);

    // --- CPI: the money leaves the vault ---

    let authority_bump = ctx.accounts.vault.vault_authority_bump;
    let authority_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        vault_key.as_ref(),
        &[authority_bump],
    ];

    let ix = Instruction {
        program_id: ctx.accounts.adapter_program.key(),
        accounts: metas,
        data: payload, // opaque — the vault does NOT parse it
    };

    invoke_signed(&ix, &infos, &[authority_seeds])?;

    // =====================================================================
    // AFTER THE CPI — this is where the real protection lives.
    // Everything above can be fooled; real balances and real values cannot.
    // =====================================================================

    // --- BALANCES AFTER: all assets (cheap) ---
    let bal_after = available_balances_with_fresh(
        &ctx.accounts.vault,
        &vault_key,
        bal_accounts,
        fresh_index.map(|i| (i, &out_token_account_key)),
    )?;

    // (1) The adapter must NOT touch any asset other than the two it declared.
    //
    // This is what "only measure asset_in/asset_out" would miss entirely: the adapter
    // has been handed the vault's signature, and it can use that signature to drain a
    // THIRD token account.
    //
    // This measurement is cheap (just read the token account, no Pyth decoding), so we
    // can afford it across ALL assets — even when there is not enough compute to price
    // them all.
    for i in 0..n {
        if i == idx_in || i == idx_out {
            continue;
        }
        require!(
            bal_after[i] == bal_before[i],
            TribeError::UnexpectedBalanceChange
        );
    }

    // (2) Do not spend more than `amount_in` of the input asset.
    let spent = bal_before[idx_in]
        .checked_sub(bal_after[idx_in])
        .ok_or(TribeError::UnexpectedBalanceChange)?;
    require!(spent <= amount_in, TribeError::ExcessiveSpend);

    // --- VALUES AFTER: only the two assets involved (expensive) ---
    //
    // Only these two assets changed balance (proven in (1)), so only they need pricing.
    let (_, val_in_after) = value_of(
        &ctx.accounts.asset_in,
        &bal_accounts[idx_in * 2 + 1],
        oracle_in,
        &clock,
    )?;
    // For a freshly-opened asset, read the received balance from the named
    // `vault_out_token_account` (Anchor keeps it current). Its `bal_accounts` snapshot
    // can be stale because the ATA was created by init_if_needed during this same
    // instruction. For an already-held asset, the snapshot is fine.
    let val_out_after = if fresh_index == Some(idx_out) {
        // The cached `.amount` was loaded at account resolution (before the CPI), so it
        // is stale after the swap paid tokens in — reload it from the live account data.
        ctx.accounts.vault_out_token_account.reload()?;
        // reserved == 0 for a fresh slot, so the full balance counts.
        let bal = ctx.accounts.vault_out_token_account.amount;
        value_from_balance(&ctx.accounts.asset_out, bal, oracle_out, &clock)?.1
    } else {
        value_of(
            &ctx.accounts.asset_out,
            &bal_accounts[idx_out * 2 + 1],
            oracle_out,
            &clock,
        )?
        .1
    };

    // (3) The trade must not lose more than `max_slippage` OF THE VALUE IT PUT IN.
    //
    // This is the AGNOSTIC check that replaces `min_out`: the vault does not need to
    // know whether this is a swap or a lend, only that it did not get poorer.
    //
    // # Why this is anchored to `value_in`, and NOT to total NAV
    //
    // The obvious formulation is `NAV_after >= NAV_before × (1 − slippage)`. But that is
    // WRONG, and wrong in a dangerous direction: the threshold LOOSENS as the vault
    // grows.
    //
    //   vault of 10,000 USDC, swap 1,000 -> 1% of NAV = 100 USDC
    //   => a 1,000 USDC trade is allowed to lose up to 10% and still be "valid"
    //
    //   vault of 1,000,000 USDC, the exact same trade
    //   => allowed to lose 10,000 USDC = a 1000% LOSS on the value of the trade
    //
    // The bigger the vault, the easier it is for a single trade to drain it. Exactly the
    // opposite of what we want.
    //
    // So we anchor to the value of the TRADE. Now 1% always means 1% of the amount put
    // in.
    //
    // We compare only the combined value of the TWO assets involved — every other asset
    // has an unchanged balance (proven in (1)), so they cancel out of both sides.
    let pair_before = (val_in_before as u128)
        .checked_add(val_out_before as u128)
        .ok_or(TribeError::MathOverflow)?;
    let pair_after = (val_in_after as u128)
        .checked_add(val_out_after as u128)
        .ok_or(TribeError::MathOverflow)?;

    let max_loss = (value_in as u128)
        .checked_mul(MAX_SLIPPAGE_BPS as u128)
        .ok_or(TribeError::MathOverflow)?
        .checked_div(BPS_SCALE as u128)
        .ok_or(TribeError::MathOverflow)?;

    require!(
        pair_after >= pair_before.saturating_sub(max_loss),
        TribeError::ValueLost
    );

    // (4) `reserved` is still intact — the REAL balance must not fall below what has
    //     been promised to redeemers.
    //
    // This has to read the raw token balance, NOT the available balance. Both
    // `available_balances` and `value_of` apply `saturating_sub(reserved)`, so an adapter
    // that eats into the reserved portion just bottoms those readings out at 0 — which
    // silently HIDES the violation rather than exposing it.
    //
    // Only asset_in can be drained (asset_out only receives), but check both: an adapter
    // handed the vault's signing authority is exactly the thing we refuse to assume good
    // behavior from.
    for idx in [idx_in, idx_out] {
        // A freshly-opened slot has reserved == 0 (nothing was ever promised against a
        // position that did not exist a moment ago), and its Asset-PDA meter slot is a
        // placeholder — so read its raw balance from the named account and use 0 reserved.
        let (raw_balance, reserved) = if fresh_index == Some(idx) {
            ctx.accounts.vault_out_token_account.reload()?;
            (ctx.accounts.vault_out_token_account.amount, 0u64)
        } else {
            let asset: Account<Asset> = Account::try_from(&bal_accounts[idx * 2])?;
            let token: anchor_spl::token_interface::TokenAccount =
                anchor_spl::token_interface::TokenAccount::try_deserialize(
                    &mut &bal_accounts[idx * 2 + 1].try_borrow_data()?[..],
                )?;
            (token.amount, asset.reserved)
        };

        require!(raw_balance >= reserved, TribeError::ReservedViolated);
    }

    // --- Exposure cap: it CANNOT be checked here, and that is a deliberate trade-off ---
    //
    // The exposure cap ("no asset may exceed X% of NAV") needs the full NAV as its
    // DENOMINATOR — which means pricing EVERY asset. But pricing every asset twice is
    // precisely the compute cost we chose not to pay above.
    //
    // Three options, and why we chose the third:
    //
    //   1. Compute the full NAV here -> the cost scales with the asset count while the
    //      compute budget does not, and Jupiter's swap has already spent a large slice
    //      of the transaction's budget. Not a bill we want on the critical path.
    //
    //   2. Let the caller pass the NAV in -> NO. That number decides whether the trade
    //      is valid at all; trusting the caller here opens exactly the door that
    //      `min_out` used to open.
    //
    //   3. Split it into a SEPARATE instruction (`assert_exposure`), called in the SAME
    //      transaction, after execute_action. It gets a full 200k CU of its own to
    //      compute the complete NAV. If the cap is exceeded -> revert -> the whole
    //      transaction (swap included) rolls back.
    //
    // Option 3 preserves atomicity without spending execute_action's compute. The price:
    // the client has to remember to bundle the two instructions. Governance should make
    // that mandatory in the proposal template.
    //
    // See `assert_exposure` below.

    emit!(ExecuteEvent {
        adapter: adapter.program_id,
        action_id,
        mint_in: ctx.accounts.asset_in.mint,
        mint_out: ctx.accounts.asset_out.mint,
        amount_in: spent,
        value_in,
        value_out: val_out_after.saturating_sub(val_out_before),
    });

    Ok(())
}

/// Enforce that every asset sits within its exposure cap.
///
/// # Why this is a SEPARATE instruction
///
/// The exposure cap needs the full NAV as its denominator → it must price EVERY asset
/// (~6k CU/asset). But `execute_action` cannot afford that: Jupiter *requests* 1.4M CU —
/// **the absolute ceiling on Solana** — for any route, and although what it actually
/// *consumes* is far less (~72k CU for a 1-hop, ~508k CU for a 3-hop, measured on
/// mainnet), the cost of pricing every asset grows with the asset count while the
/// transaction's compute budget does not.
///
/// So it is split out. This instruction gets **its own 200k CU**, enough to compute the
/// full NAV.
///
/// # How to use it: BUNDLE IT IN THE SAME TRANSACTION
///
/// ```text
///   [ix 0]  execute_action(...)      <- the swap
///   [ix 1]  assert_exposure()        <- the check, afterwards
/// ```
///
/// Same transaction → **still atomic**. If exposure exceeds the cap, this instruction
/// reverts, and **the swap rolls back along with it**. No half-finished state.
///
/// And because it computes against the FINAL NAV, it also blocks the trick of evading
/// the cap by splitting one order into many: the Nth order — the one that crosses the
/// cap — reverts, no matter how finely the trades were sliced beforehand.
///
/// # Whose job is the bundling
///
/// The client's. Governance should make it mandatory in the proposal template — an
/// `execute_action` proposal without an `assert_exposure` is a proposal with a missing
/// guardrail.
///
/// (It cannot be enforced on-chain from inside execute_action itself: Solana does not let
/// one instruction see the other instructions in the same transaction, unless it reads
/// the `Instructions` sysvar — which is doable, but costs exactly the compute we are
/// trying to conserve.)
pub fn assert_exposure<'info>(
    ctx: Context<'_, '_, 'info, 'info, AssertExposure<'info>>,
) -> Result<()> {
    let clock = Clock::get()?;
    let vault_key = ctx.accounts.vault.key();

    // Full NAV — the triple (Asset, token, oracle) for EVERY asset.
    let nav = compute_nav(
        &ctx.accounts.vault,
        &vault_key,
        ctx.remaining_accounts,
        &clock,
    )?;

    if nav.total_value == 0 {
        return Ok(());
    }

    for i in 0..(ctx.accounts.vault.asset_count as usize) {
        let asset: Account<Asset> = Account::try_from(&ctx.remaining_accounts[i * 3])?;
        if asset.max_exposure_bps == 0 {
            continue; // 0 = no limit
        }

        // value_i / nav > max_bps / SCALE  <=>  value_i × SCALE > nav × max_bps
        // Cross-multiply so we lose no precision to integer division (rounding).
        let lhs = (nav.values[i] as u128)
            .checked_mul(BPS_SCALE as u128)
            .ok_or(TribeError::MathOverflow)?;
        let rhs = (nav.total_value as u128)
            .checked_mul(asset.max_exposure_bps as u128)
            .ok_or(TribeError::MathOverflow)?;

        require!(lhs <= rhs, TribeError::ExposureExceeded);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/// No signer needed — this is a CHECK, it changes no state.
/// Anyone may call it; it either reverts or it does not.
#[derive(Accounts)]
pub struct AssertExposure<'info> {
    #[account(
        seeds = [VAULT_SEED],
        bump = vault.bump,
    )]
    pub vault: Box<Account<'info, Vault>>,
}

#[derive(Accounts)]
#[instruction(action_id: ActionId)]
pub struct ExecuteAction<'info> {
    /// Who is allowed to hit execute — read from `vault.executor`, NOT `vault.admin`.
    ///
    /// MVP: executor = admin. Later: a governance PDA, and from then on "allowed to
    /// execute" means "the proposal passed + the timelock elapsed". Exactly one place to
    /// change. Mutable because it pays rent when a fresh held-slot is opened.
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        mut, // lazy-open writes asset_mints/asset_count when buying a never-held token
        seeds = [VAULT_SEED],
        bump = vault.bump,
        constraint = vault.executor == authority.key() @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,

    /// CHECK: the PDA that signs for the adapter. This is the power being granted — and
    /// it is the reason everything in this function has to be checked so tightly.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, vault.key().as_ref()],
        bump = vault.vault_authority_bump,
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        seeds = [ADAPTER_SEED, vault.key().as_ref(), adapter.program_id.as_ref()],
        bump = adapter.bump,
        constraint = adapter.vault == vault.key() @ TribeError::AdapterNotRegistered,
    )]
    pub adapter: Box<Account<'info, Adapter>>,

    /// CHECK: verified against `adapter.program_id` inside the function.
    pub adapter_program: UncheckedAccount<'info>,

    /// The triple (adapter × action_id × asset_in) must already be whitelisted.
    ///
    /// This account DOES NOT EXIST for an unregistered triple, so Anchor fails right at
    /// seed resolution. That is exactly how `stake × jito × ETH` is blocked: nobody can
    /// register that capability, so nobody can execute it.
    #[account(
        seeds = [
            CAPABILITY_SEED,
            vault.key().as_ref(),
            adapter.program_id.as_ref(),
            &[action_id],
            asset_in.mint.as_ref(),
        ],
        bump = capability.bump,
    )]
    pub capability: Box<Account<'info, Capability>>,

    /// The asset the vault spends. This is the only one the vault strictly needs to know
    /// — every other asset (including the one received) is verified via the NAV-delta,
    /// with no need to declare it up front.
    #[account(
        seeds = [ASSET_SEED, vault.key().as_ref(), asset_in.mint.as_ref()],
        bump = asset_in.bump,
        constraint = asset_in.vault == vault.key() @ TribeError::AssetNotRegistered,
    )]
    pub asset_in: Box<Account<'info, Asset>>,

    /// The asset the vault receives.
    ///
    /// The vault needs to know this one in order to PRICE it (expensive, which is why we
    /// only do it for 2 assets). Every other asset only gets its BALANCE checked (cheap)
    /// — enough to catch an adapter secretly touching them.
    ///
    /// `init_if_needed`: the FIRST time the vault buys a token, this opens a held-slot
    /// for it. Re-init is guarded in the function body — a fresh Asset (vault field
    /// still zero) is populated once and registered as a slot; an existing one is used
    /// as-is. `out_mint` and `out_oracle` are needed to populate a fresh slot.
    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + Asset::INIT_SPACE,
        seeds = [ASSET_SEED, vault.key().as_ref(), out_mint.key().as_ref()],
        bump,
    )]
    pub asset_out: Box<Account<'info, Asset>>,

    /// Mint of the received asset. Named (not derived from `asset_out`) because on a
    /// fresh init `asset_out.mint` is still zero.
    pub out_mint: Box<InterfaceAccount<'info, Mint>>,

    /// The vault's ATA for the received asset — created on a fresh open. Must be the
    /// canonical ATA of vault_authority (every DEX derives this address).
    #[account(
        init_if_needed,
        payer = authority,
        associated_token::mint = out_mint,
        associated_token::authority = vault_authority,
    )]
    pub vault_out_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: Pyth price update for the received asset; validated on read.
    pub out_oracle: UncheckedAccount<'info>,

    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// Register an adapter. Governance vote + a 7-day timelock.
#[derive(Accounts)]
#[instruction(program_id: Pubkey)]
pub struct RegisterAdapter<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        seeds = [VAULT_SEED],
        bump = vault.bump,
        has_one = admin @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,

    #[account(
        init,
        payer = admin,
        space = 8 + Adapter::INIT_SPACE,
        seeds = [ADAPTER_SEED, vault.key().as_ref(), program_id.as_ref()],
        bump
    )]
    pub adapter: Box<Account<'info, Adapter>>,

    pub system_program: Program<'info, System>,
}

/// Remove an adapter. NO timelock — stopping the bleeding has to be fast.
/// Opening a door is slow (7 days); closing one is instant.
#[derive(Accounts)]
pub struct DisableAdapter<'info> {
    pub admin: Signer<'info>,

    #[account(
        seeds = [VAULT_SEED],
        bump = vault.bump,
        has_one = admin @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,

    #[account(
        mut,
        seeds = [ADAPTER_SEED, vault.key().as_ref(), adapter.program_id.as_ref()],
        bump = adapter.bump,
        constraint = adapter.vault == vault.key() @ TribeError::AdapterNotRegistered,
    )]
    pub adapter: Box<Account<'info, Adapter>>,
}

#[event]
pub struct ExecuteEvent {
    pub adapter: Pubkey,
    pub action_id: ActionId,
    pub mint_in: Pubkey,
    pub mint_out: Pubkey,
    /// The amount ACTUALLY spent (measured from the balances; we do not trust what the
    /// adapter claims).
    pub amount_in: u64,
    pub value_in: u64,
    pub value_out: u64,
}
