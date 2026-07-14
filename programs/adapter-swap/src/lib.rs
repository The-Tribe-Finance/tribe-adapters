use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};
use anchor_spl::token_interface::TokenAccount;

declare_id!("3wQCqUNGMBZL3Pe1v1iyqvonKMNMHjgByEw7iBTGHwyN");

/// # Action adapter — SWAP
///
/// **One program. One action. Deploy once. Set immutable.**
///
/// This is the whole adapter philosophy, and it is the opposite of "one
/// general-purpose adapter that gets upgraded over time":
///
/// ```text
/// DON'T                            DO
/// ─────                            ──
/// 1 "general-purpose" adapter      each action = its own adapter
/// add lending -> upgrade adapter   add lending -> deploy a NEW adapter
///   -> the old swap code is at risk  -> this file is never touched
///   -> 1 bad upgrade = everything    -> a buggy new adapter only reverts
///      breaks                           itself
/// upgrade authority = attack surface  set immutable (upgrade = None)
/// ```
///
/// Adding staking later: write `adapter-stake`, deploy it, have governance
/// whitelist it. **Not a single line of this file changes.** Same for the vault.
///
/// ## The adapter is UNTRUSTED — and that is deliberate
///
/// The vault does NOT trust what this adapter does. After the CPI, the vault
/// recomputes NAV itself and enforces `NAV_after >= NAV_before × (1 − slippage)`.
/// A buggy adapter, a hijacked adapter, or a compromised target DEX — all of them
/// make NAV drop, and all of them get reverted by the vault.
///
/// Precisely because it is verifiable, the adapter is allowed to do the "hard"
/// work without being audited at core level: it computes `min_out`, it understands
/// Jupiter's layout, it builds the route. If it gets it wrong, the vault catches it.
///
/// (Contrast: a **Pricing adapter** feeds straight into NAV — there is nothing to
/// measure it against — so it is TRUSTED and must be audited like core.)
///
/// ## Why min_out lives HERE, not in the vault
///
/// `min_out` is a swap-specific concept. Lending has no `min_out` (it has
/// `min_receipt`), and neither does staking. If the vault computed `min_out`, the
/// vault would have to *understand* swaps — and adding lending would mean changing
/// the vault.
///
/// So the vault only verifies the AGNOSTIC thing (value must not drop), while the
/// logic specific to each action lives in its corresponding adapter.
#[program]
pub mod adapter_swap {
    use super::*;

    /// This adapter's action id. The vault only uses this number as a seed for the
    /// `Capability` — it does not understand, and does not need to understand, that
    /// 0 means "swap".
    pub const ACTION_SWAP: u8 = 0;

    /// Swap `amount_in` of the input token for the output token, through a DEX.
    ///
    /// The vault has already signed with its PDA (`vault_authority`) before calling
    /// in here, so the adapter has the right to move the vault's tokens — but ONLY
    /// within this transaction, and only with a result the vault finds acceptable.
    ///
    /// remaining_accounts: the DEX's accounts, forwarded verbatim by the adapter.
    pub fn swap<'info>(
        ctx: Context<'_, '_, 'info, 'info, Swap<'info>>,
        amount_in: u64,
        min_out: u64,
        dex_payload: Vec<u8>,
    ) -> Result<()> {
        require!(amount_in > 0, SwapError::ZeroAmount);

        // Measure first — the adapter protects itself, it does not just leave it to
        // the vault.
        //
        // The vault WILL check again via NAV-delta, so this layer is redundant from a
        // safety standpoint. But it produces the error message in the RIGHT PLACE:
        // "the DEX returned less than min_out" is far easier to understand than "the
        // vault's NAV dropped 3%".
        let balance_out_before = ctx.accounts.vault_token_out.amount;

        // --- CPI into the DEX ---
        //
        // The adapter does not impose an account ordering — each DEX has its own
        // layout. The client builds the list (it already has it, since it calls the
        // DEX's API to get the route), and the adapter forwards it verbatim.
        //
        // The vault_authority's signature is forwarded DOWN to the DEX: it was already
        // a signer when the vault did invoke_signed into the adapter, and the adapter
        // passes it along. That is how the DEX is able to pull tokens out of the
        // vault's token account.
        let authority_key = ctx.accounts.vault_authority.key();

        let mut metas: Vec<AccountMeta> = Vec::with_capacity(ctx.remaining_accounts.len());
        let mut infos: Vec<AccountInfo<'info>> =
            Vec::with_capacity(ctx.remaining_accounts.len());

        for acc in ctx.remaining_accounts.iter() {
            let key = acc.key();
            metas.push(AccountMeta {
                pubkey: key,
                // Only vault_authority is allowed to sign. NEVER lend the signature
                // to an unknown account — the same principle the vault applies to the
                // adapter.
                is_signer: key == authority_key,
                is_writable: acc.is_writable,
            });
            infos.push(acc.clone());
        }

        let ix = Instruction {
            program_id: ctx.accounts.dex_program.key(),
            accounts: metas,
            data: dex_payload, // opaque
        };

        // The adapter has NO PDA of its own to sign with. The signature being carried
        // here belongs to the vault, and is passed down via invoke (not invoke_signed)
        // — because the adapter owns no seeds.
        anchor_lang::solana_program::program::invoke(&ix, &infos)?;

        // --- Check the result ---

        ctx.accounts.vault_token_out.reload()?;
        let received = ctx
            .accounts
            .vault_token_out
            .amount
            .checked_sub(balance_out_before)
            .ok_or(SwapError::BalanceDecreased)?;

        require!(received >= min_out, SwapError::SlippageExceeded);

        emit!(SwapEvent {
            amount_in,
            amount_out: received,
            min_out,
        });

        Ok(())
    }
}

#[derive(Accounts)]
pub struct Swap<'info> {
    /// The VAULT's PDA — a signer; the signature is granted by the vault via
    /// invoke_signed.
    ///
    /// The adapter merely BORROWS this signature for the current transaction. It
    /// cannot keep it, and it cannot reuse it.
    /// CHECK: the vault already verified this is its own PDA before doing the CPI here.
    pub vault_authority: Signer<'info>,

    /// The vault's destination token account. The adapter measures it to know how much
    /// it received back.
    #[account(mut)]
    pub vault_token_out: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: the DEX to CPI into (Jupiter, Orca, ...). The vault has whitelisted this
    /// adapter; the adapter is responsible for the DEX it calls.
    pub dex_program: UncheckedAccount<'info>,
}

#[event]
pub struct SwapEvent {
    pub amount_in: u64,
    pub amount_out: u64,
    pub min_out: u64,
}

#[error_code]
pub enum SwapError {
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("DEX returned less than min_out")]
    SlippageExceeded,
    #[msg("Output balance decreased")]
    BalanceDecreased,
}
