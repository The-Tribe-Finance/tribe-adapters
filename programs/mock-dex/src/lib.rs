use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("BeqpQDwTXfuSE3Q4kWCKJKYKYKC84sVPVYeGk4KkfxXo");

/// # Mock DEX — for TESTS ONLY
///
/// It plays the role of Jupiter in the integration tests. It exists to answer one
/// question that no other test can answer:
///
///   **Does the vault's "trust only balances" layer actually stop an adapter that
///   misbehaves?**
///
/// So this program DELIBERATELY has the ability to do bad things — spend more than
/// it is allowed to, return less than it promised, return nothing at all. The vault
/// must catch all of them.
///
/// The account layout here is defined by THIS PROGRAM ITSELF (just as Jupiter defines
/// the layout of `shared_accounts_route`). The vault does not impose an ordering — it
/// simply forwards the account list the client built, and only signs for exactly
/// `vault_authority`.
#[program]
pub mod mock_dex {
    use super::*;

    /// Swap: take `amount_in` from the vault, return `amount_out` back to the vault.
    ///
    /// Both numbers are supplied by the CALLER — and that is exactly the point.
    /// A real DEX is no different: the vault has no way of knowing what it will do
    /// before it does it. The vault only measures balances afterwards.
    pub fn swap(ctx: Context<Swap>, amount_in: u64, amount_out: u64) -> Result<()> {
        // Take tokens from the vault. `vault_authority` signs for this — the signature
        // was granted by the vault via invoke_signed.
        if amount_in > 0 {
            token_interface::transfer_checked(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.vault_token_in.to_account_info(),
                        mint: ctx.accounts.mint_in.to_account_info(),
                        to: ctx.accounts.pool_token_in.to_account_info(),
                        authority: ctx.accounts.vault_authority.to_account_info(),
                    },
                ),
                amount_in,
                ctx.accounts.mint_in.decimals,
            )?;
        }

        // Return tokens to the vault, signing with the pool's authority.
        if amount_out > 0 {
            let bump = ctx.bumps.pool_authority;
            let seeds: &[&[u8]] = &[b"pool", &[bump]];

            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.pool_token_out.to_account_info(),
                        mint: ctx.accounts.mint_out.to_account_info(),
                        to: ctx.accounts.vault_token_out.to_account_info(),
                        authority: ctx.accounts.pool_authority.to_account_info(),
                    },
                    &[seeds],
                ),
                amount_out,
                ctx.accounts.mint_out.decimals,
            )?;
        }

        Ok(())
    }
}

#[derive(Accounts)]
pub struct Swap<'info> {
    /// The VAULT's PDA. It is a signer — that power was granted by the vault via
    /// invoke_signed.
    /// CHECK: mock, no data is read.
    pub vault_authority: Signer<'info>,

    #[account(mut)]
    pub vault_token_in: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault_token_out: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: PDA holding the mock pool's liquidity.
    #[account(seeds = [b"pool"], bump)]
    pub pool_authority: UncheckedAccount<'info>,

    #[account(mut)]
    pub pool_token_in: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub pool_token_out: InterfaceAccount<'info, TokenAccount>,

    pub mint_in: InterfaceAccount<'info, Mint>,
    pub mint_out: InterfaceAccount<'info, Mint>,

    pub token_program: Interface<'info, TokenInterface>,
}
