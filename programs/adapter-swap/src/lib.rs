use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};
use anchor_spl::token_interface::TokenAccount;

declare_id!("Swap111111111111111111111111111111111111111");

/// # Action adapter — SWAP
///
/// **Một program. Một action. Deploy một lần. Set immutable.**
///
/// Đây là toàn bộ triết lý adapter, và nó đối lập với "một adapter đa năng rồi
/// upgrade dần":
///
/// ```text
/// KHÔNG NÊN                        NÊN
/// ─────────                        ───
/// 1 adapter "đa năng"              mỗi action = 1 adapter riêng
/// thêm lending -> upgrade adapter  thêm lending -> deploy adapter MỚI
///   -> code swap cũ có rủi ro        -> file này không bị đụng
///   -> 1 upgrade lỗi = hỏng hết      -> adapter mới lỗi, chỉ nó bị revert
/// upgrade authority = attack surface  set immutable (upgrade = None)
/// ```
///
/// Thêm staking sau này: viết `adapter-stake`, deploy, governance whitelist nó.
/// **File này không sửa một dòng nào.** Vault cũng vậy.
///
/// ## Adapter là UNTRUSTED — và điều đó có chủ đích
///
/// Vault KHÔNG tin những gì adapter này làm. Sau CPI, vault tự tính lại NAV và bắt
/// buộc `NAV_sau >= NAV_trước × (1 − slippage)`. Adapter có bug, hay bị chiếm quyền,
/// hay DEX đích bị tấn công — tất cả đều làm NAV tụt, và đều bị vault revert.
///
/// Chính vì kiểm chứng được nên adapter được phép làm việc "khó" mà không cần audit
/// ở mức core: nó tính `min_out`, nó hiểu layout của Jupiter, nó dựng route. Sai thì
/// vault bắt.
///
/// (Đối lập: **Pricing adapter** feed thẳng vào NAV — không có gì để đo — nên nó là
/// TRUSTED và phải audit như core.)
///
/// ## Vì sao min_out nằm ở ĐÂY, không ở vault
///
/// `min_out` là khái niệm riêng của swap. Lend không có `min_out` (nó có
/// `min_receipt`), staking cũng vậy. Nếu vault tính `min_out`, vault phải *hiểu* swap
/// — và thêm lending sẽ phải sửa vault.
///
/// Nên vault chỉ kiểm chứng thứ AGNOSTIC (giá trị không giảm), còn logic riêng của
/// từng action nằm ở adapter tương ứng.
#[program]
pub mod adapter_swap {
    use super::*;

    /// Action id của adapter này. Vault chỉ dùng con số này làm seed của
    /// `Capability` — nó không hiểu, và không cần hiểu, rằng 0 nghĩa là "swap".
    pub const ACTION_SWAP: u8 = 0;

    /// Swap `amount_in` của token vào lấy token ra, qua một DEX.
    ///
    /// Vault đã ký bằng PDA của nó (`vault_authority`) trước khi gọi vào đây, nên
    /// adapter có quyền chuyển token của vault — nhưng CHỈ trong transaction này, và
    /// chỉ với kết quả mà vault chấp nhận được.
    ///
    /// remaining_accounts: account của DEX, adapter chuyển tiếp nguyên vẹn.
    pub fn swap<'info>(
        ctx: Context<'_, '_, 'info, 'info, Swap<'info>>,
        amount_in: u64,
        min_out: u64,
        dex_payload: Vec<u8>,
    ) -> Result<()> {
        require!(amount_in > 0, SwapError::ZeroAmount);

        // Đo trước — adapter tự bảo vệ mình, không phó mặc cho vault.
        //
        // Vault SẼ kiểm tra lại bằng NAV-delta, nên lớp này là thừa về mặt an toàn.
        // Nhưng nó cho ra thông báo lỗi ĐÚNG CHỖ: "DEX trả về ít hơn min_out" dễ
        // hiểu hơn nhiều so với "NAV của vault tụt 3%".
        let balance_out_before = ctx.accounts.vault_token_out.amount;

        // --- CPI sang DEX ---
        //
        // Adapter không áp đặt thứ tự account — DEX có layout riêng của nó. Client
        // dựng list (nó gọi API của DEX để lấy route nên có sẵn), adapter chuyển
        // tiếp nguyên vẹn.
        //
        // Chữ ký của vault_authority được chuyển tiếp XUỐNG DEX: nó đã là signer khi
        // vault invoke_signed vào adapter, và adapter forward tiếp. Đó là cách DEX
        // rút được token từ token account của vault.
        let authority_key = ctx.accounts.vault_authority.key();

        let mut metas: Vec<AccountMeta> = Vec::with_capacity(ctx.remaining_accounts.len());
        let mut infos: Vec<AccountInfo<'info>> =
            Vec::with_capacity(ctx.remaining_accounts.len());

        for acc in ctx.remaining_accounts.iter() {
            let key = acc.key();
            metas.push(AccountMeta {
                pubkey: key,
                // Chỉ vault_authority được ký. Không bao giờ cho account lạ mượn
                // chữ ký — cùng nguyên tắc mà vault áp dụng với adapter.
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

        // Adapter KHÔNG có PDA riêng để ký. Chữ ký đang mang theo là của vault, được
        // truyền xuống qua invoke (không phải invoke_signed) — vì adapter không sở
        // hữu seed nào.
        anchor_lang::solana_program::program::invoke(&ix, &infos)?;

        // --- Kiểm tra kết quả ---

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
    /// PDA của VAULT — signer, chữ ký do vault cấp qua invoke_signed.
    ///
    /// Adapter chỉ MƯỢN chữ ký này trong đúng transaction hiện tại. Nó không giữ
    /// được, không dùng lại được.
    /// CHECK: vault đã xác thực đây là PDA của nó trước khi CPI vào đây.
    pub vault_authority: Signer<'info>,

    /// Token account đích của vault. Adapter đo nó để biết nhận về bao nhiêu.
    #[account(mut)]
    pub vault_token_out: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: DEX để CPI tới (Jupiter, Orca...). Vault đã whitelist adapter này;
    /// adapter chịu trách nhiệm về DEX mà nó gọi.
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
