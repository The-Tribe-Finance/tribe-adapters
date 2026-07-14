use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("E88wFPQJPYv7PoeV2JbwEw76u6oiRS4m2fZSqqhaG2JA");

/// # test-adapter — TESTS ONLY, never deployed to a live network
///
/// A stand-in for a real Action adapter (Jupiter, Kamino…), built to answer the one
/// question a real, well-behaved DEX never can:
///
///   **Do the vault's guards actually stop an adapter that misbehaves?**
///
/// Jupiter will not take your money and return nothing. It will not blow past the
/// slippage allowance. It will not spend tokens reserved for a pending redemption. So a
/// test that only ever runs a real, honest swap proves that the happy path works — and
/// says nothing about security.
///
/// This program does all of those things *on command*. The caller supplies `amount_in`
/// and `amount_out` directly, and the adapter simply moves those amounts. The vault, which
/// measured balances before the CPI and measures them again after, has to catch every
/// abuse and revert.
///
/// The account layout is defined by THIS program. The vault imposes no ordering — it
/// forwards the account list the client built, and signs only for `vault_authority`.
#[program]
pub mod test_adapter {
    use super::*;

    /// Move `amount_in` out of the vault and `amount_out` back in.
    ///
    /// Both numbers come from the CALLER — that is the whole point. The vault cannot know
    /// what an adapter will do before it does it; it only trusts the balances afterwards.
    /// Feeding these two numbers lets a test drive every abuse: `amount_out = 0` (took the
    /// money, returned nothing), a value below the slippage allowance, or an `amount_in`
    /// that dips into the reserved balance.
    pub fn swap(ctx: Context<Swap>, amount_in: u64, amount_out: u64) -> Result<()> {
        // Pull tokens out of the vault. `vault_authority` signs — the signature was
        // granted by the vault via invoke_signed.
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

        // Return tokens to the vault, signing with the pool's own authority.
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
    /// The VAULT's PDA. It is a signer — that authority was granted by the vault via
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
