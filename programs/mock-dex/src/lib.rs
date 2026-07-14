use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("Dex1111111111111111111111111111111111111111");

/// # DEX giả — CHỈ dùng cho test
///
/// Đóng vai Jupiter trong test tích hợp. Nó tồn tại để trả lời một câu hỏi mà
/// không test nào khác trả lời được:
///
///   **Lớp "chỉ tin số dư" của vault có thật sự chặn được một adapter gian lận không?**
///
/// Nên program này CỐ Ý làm được những việc xấu — tiêu quá phần được phép, trả về
/// ít hơn cam kết, không trả gì cả. Vault phải bắt được tất cả.
///
/// Layout account ở đây do CHÍNH NÓ quy định (giống Jupiter tự quy định layout của
/// `shared_accounts_route`). Vault không áp đặt thứ tự — nó chỉ chuyển tiếp danh
/// sách account mà client dựng, và chỉ ký cho đúng `vault_authority`.
#[program]
pub mod mock_dex {
    use super::*;

    /// Swap: lấy `amount_in` từ vault, trả `amount_out` về cho vault.
    ///
    /// Cả hai con số đều do NGƯỜI GỌI truyền vào — đó chính là điểm mấu chốt.
    /// Một DEX thật cũng vậy: vault không có cách nào biết nó sẽ làm gì trước khi
    /// nó làm. Vault chỉ đo số dư sau đó.
    pub fn swap(ctx: Context<Swap>, amount_in: u64, amount_out: u64) -> Result<()> {
        // Lấy token từ vault. `vault_authority` ký cho việc này — chữ ký do vault
        // cấp qua invoke_signed.
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

        // Trả token về cho vault, ký bằng authority của pool.
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
    /// PDA của VAULT. Nó là signer — quyền lực này do vault cấp qua invoke_signed.
    /// CHECK: mock, không đọc dữ liệu.
    pub vault_authority: Signer<'info>,

    #[account(mut)]
    pub vault_token_in: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault_token_out: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: PDA giữ thanh khoản của pool giả.
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
