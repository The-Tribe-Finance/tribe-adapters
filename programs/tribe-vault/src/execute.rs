use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};

use crate::constants::*;
use crate::errors::TribeError;
use crate::nav::compute_nav;
use crate::state::{ActionId, Adapter, AdapterKind, Asset, Capability, Vault};

/// # execute_action — nơi tiền rời khỏi vault
///
/// Instruction nguy hiểm nhất của protocol. Nó CPI sang một program bên ngoài và
/// cấp quyền ký PDA để program đó di chuyển tài sản của vault.
///
/// ## Vault KHÔNG hiểu ngữ nghĩa của action
///
/// Đây là nguyên tắc trung tâm, và nó khác hẳn phiên bản trước của file này.
///
/// Vault không biết `action_id = 0` nghĩa là "swap" hay "lend" hay "stake". Nó
/// không tính `min_out`, không biết slippage của một swap trông thế nào, không
/// biết kToken là gì. Toàn bộ ngữ nghĩa nằm ở **adapter**.
///
/// Vì sao phải vậy: nếu vault hiểu swap, thì thêm lending sẽ phải sửa vault — tức
/// là **upgrade program đang giữ tiền**. Đúng cái mà cả kiến trúc này sinh ra để
/// tránh. Thêm một action mới phải là: deploy adapter mới + governance ghi một dòng
/// vào registry. Vault không đụng một dòng code nào.
///
/// ## Vault chỉ kiểm chứng KẾT QUẢ
///
/// Mọi action — swap, lend, stake, LP — rốt cuộc đều là **một chuyển dịch trên tập
/// tài sản**: vài asset giảm, vài asset tăng. Vault không cần hiểu vì sao; nó chỉ
/// cần biết kết quả có chấp nhận được không:
///
/// ```text
///   1. NAV_sau >= NAV_trước × (1 − max_slippage)     giá trị không được giảm quá mức
///   2. mọi asset tăng đều đã whitelist                không nhận về token rác
///   3. không asset nào vượt trần exposure của nó      không dồn hết vào một chỗ
///   4. không tiêu vào phần `reserved`                 không đụng tiền đã hứa cho redeemer
/// ```
///
/// Bốn kiểm tra này **agnostic** — đúng cho mọi action, hôm nay và mai sau.
///
/// ## Adapter là UNTRUSTED
///
/// Chính vì kiểm chứng được bằng NAV-delta nên vault **không cần tin** adapter.
/// Adapter có bug, có ý đồ xấu, hay protocol đích bị tấn công — tất cả đều bị bắt
/// ở bước 1–4, và transaction revert.
///
/// (Pricing adapter thì ngược lại: nó feed thẳng vào NAV, không có gì để đo, nên
/// nó là TRUSTED và phải audit như core. Xem `AdapterKind`.)
pub fn execute_action<'info>(
    ctx: Context<'_, '_, 'info, 'info, ExecuteAction<'info>>,
    action_id: ActionId,
    amount_in: u64,
    payload: Vec<u8>,
) -> Result<()> {
    require!(amount_in > 0, TribeError::ZeroAmount);

    let clock = Clock::get()?;
    let vault_key = ctx.accounts.vault.key();

    // --- Adapter phải là loại ACTION, đã bật, đã qua timelock ---

    let adapter = &ctx.accounts.adapter;

    // Pricing adapter TUYỆT ĐỐI không được dùng để execute. Nó là TRUSTED (feed
    // thẳng vào NAV) — cho nó quyền di chuyển tiền là gộp hai mức tin cậy lại làm
    // một, và mất luôn lý do khiến Action adapter an toàn.
    require!(
        adapter.kind == AdapterKind::Action,
        TribeError::WrongAdapterKind
    );
    require!(adapter.enabled, TribeError::AdapterDisabled);
    require!(
        clock.unix_timestamp >= adapter.active_at,
        TribeError::AdapterNotActive
    );
    // CPI chỉ tới đúng program đã đăng ký. Không có kiểm tra này thì người gọi
    // truyền vào một program bất kỳ và vault trở thành proxy cho code độc.
    require_keys_eq!(
        adapter.program_id,
        ctx.accounts.adapter_program.key(),
        TribeError::AdapterProgramMismatch
    );

    // --- Capability: bộ ba (adapter × action × asset) có được whitelist không ---
    //
    // Đây là chốt chặn mà một cờ boolean trên Asset không làm nổi. Asset chỉ nói
    // "có được giữ không"; capability nói "được làm GÌ với nó, Ở ĐÂU".
    //
    //   stake × jito × ETH -> capability đó chưa từng được đăng ký -> PDA không tồn
    //                         tại -> Anchor fail ngay ở khâu giải seeds.
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

    // --- Pause và capability-disabled CHỈ chặn cửa VÀO ---
    //
    // Vault không biết action này là "buy" hay "unlend" — nó đọc cờ `is_entry` mà
    // governance đã đặt lúc đăng ký capability.
    //
    // Cửa RA không bao giờ đóng: nếu vault đã lend ETH vào Kamino rồi protocol bị
    // pause (hoặc capability đó bị tắt), mà Unlend cũng bị chặn theo, thì tiền KẸT
    // VĨNH VIỄN trong Kamino. Cùng triết lý với "redeem không bao giờ bị pause".
    if capability.is_entry {
        require!(!ctx.accounts.vault.paused, TribeError::VaultPaused);
        require!(capability.enabled, TribeError::CapabilityDisabled);
        require!(
            clock.unix_timestamp >= capability.active_at,
            TribeError::CapabilityNotActive
        );
    }

    // --- Chia remaining_accounts làm hai vùng ---
    //
    // [0, nav_len)   -> của VAULT: bộ ba (Asset, token account, oracle) cho MỌI
    //                   asset. Dùng để tính NAV trước và sau CPI.
    // [nav_len, ..)  -> của ADAPTER: account protocol-specific (pool, tick array,
    //                   reserve...) mà vault không cần hiểu.
    //
    // Ranh giới phải rõ. Trộn chung rồi chuyển tiếp hết cho adapter là làm mờ ranh
    // giới tin cậy — và ranh giới mờ là chỗ lỗ hổng sinh ra.
    let nav_len = (ctx.accounts.vault.asset_count as usize)
        .checked_mul(3)
        .ok_or(TribeError::MathOverflow)?;
    require!(
        ctx.remaining_accounts.len() > nav_len,
        TribeError::IncompleteAssetSet
    );

    let nav_accounts = &ctx.remaining_accounts[..nav_len];
    let adapter_accounts = &ctx.remaining_accounts[nav_len..];

    // --- NAV TRƯỚC ---
    //
    // Tính trên TOÀN BỘ asset, không chỉ hai asset được khai. Nếu chỉ đo asset_in/
    // asset_out, adapter có thể lén đụng vào một asset thứ ba mà vault không thấy.
    let before = compute_nav(&ctx.accounts.vault, &vault_key, nav_accounts, &clock)?;

    // --- Trần notional: một lệnh đơn lẻ không được quét sạch vault ---
    //
    // Phòng thủ theo chiều sâu. Kiểm tra NAV-delta (bên dưới) bắt được "giao dịch
    // làm mất giá trị", nhưng KHÔNG bắt được "giao dịch hợp lệ nhưng amount_in là
    // toàn bộ vault". Nếu adapter có bug hoặc protocol đích bị hack, thiệt hại tối
    // đa bị chặn ở con số này thay vì bằng cả NAV.
    let idx_in = ctx.accounts.asset_in.index as usize;
    require!(idx_in < MAX_ASSETS, TribeError::AssetNotRegistered);

    // Giá trị của `amount_in`, suy từ chính snapshot vừa tính — không đọc oracle
    // lần hai (hai lần đọc trong cùng một lệnh là mở đường cho chênh lệch bị khai
    // thác).
    let balance_in = before.balances[idx_in];
    require!(balance_in > 0, TribeError::InsufficientAvailableBalance);

    let value_in = (before.values[idx_in] as u128)
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

    // --- Adapter chỉ được tiêu phần KHẢ DỤNG ---
    //
    // `reserved` là phần đã hứa cho những người đã burn share nhưng chưa claim xong.
    // Không có kiểm tra này, execute swap đi đúng số token đã hứa — tới lúc họ claim
    // thì vault không còn đủ, share đã burn mất rồi mà tài sản kẹt trong một cái
    // phiếu vô dụng.
    //
    // Lưu ý: `before.balances[]` ĐÃ trừ reserved (xem `compute_nav`), nên nó chính
    // là số khả dụng.
    require!(
        before.balances[idx_in] >= amount_in,
        TribeError::InsufficientAvailableBalance
    );

    // --- Dựng account list cho CPI ---
    //
    // Vault KHÔNG áp đặt thứ tự. Mỗi protocol có layout riêng của nó — vault không
    // thể đoán, và không cần: client dựng list (nó gọi API của protocol để lấy route
    // nên có sẵn), vault chỉ chuyển tiếp nguyên vẹn.
    //
    // Nhưng vault giữ lại ĐÚNG MỘT quyền, và đây là chốt chặn không nhân nhượng:
    // nó tự quyết định account nào được ký.
    let authority_key = ctx.accounts.vault_authority.key();
    let mut authority_present = false;

    let mut metas: Vec<AccountMeta> = Vec::with_capacity(adapter_accounts.len());
    let mut infos: Vec<AccountInfo<'info>> = Vec::with_capacity(adapter_accounts.len());

    for acc in adapter_accounts.iter() {
        let key = acc.key();

        // Quyền ký của vault CHỈ cấp cho đúng vault_authority. Mọi account khác luôn
        // is_signer = false, bất kể client khai báo gì.
        //
        // Không có kiểm tra này thì client đưa vào một account bất kỳ kèm cờ signer,
        // và vault ký thay cho nó — tức là cho mượn quyền lực của mình để làm bất cứ
        // điều gì, ở bất cứ đâu.
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

    // --- CPI: tiền rời vault ---

    let authority_bump = ctx.accounts.vault.vault_authority_bump;
    let authority_seeds: &[&[u8]] = &[
        VAULT_AUTHORITY_SEED,
        vault_key.as_ref(),
        &[authority_bump],
    ];

    let ix = Instruction {
        program_id: ctx.accounts.adapter_program.key(),
        accounts: metas,
        data: payload, // opaque — vault KHÔNG parse
    };

    invoke_signed(&ix, &infos, &[authority_seeds])?;

    // --- NAV SAU: đây là lớp bảo vệ thật sự ---
    //
    // Mọi thứ ở trên có thể bị lừa; giá trị thật của vault thì không.

    let after = compute_nav(&ctx.accounts.vault, &vault_key, nav_accounts, &clock)?;

    // (1) Giao dịch không được làm mất quá `max_slippage` của CHÍNH GIÁ TRỊ ĐEM ĐI.
    //
    // Đây là kiểm tra thay cho `min_out` của phiên bản trước — và nó AGNOSTIC: vault
    // không cần biết đây là swap hay lend, chỉ cần biết mình không nghèo đi.
    //
    // # Vì sao đo trên `value_in`, KHÔNG trên tổng NAV
    //
    // Cách hiển nhiên là `NAV_sau >= NAV_trước × (1 − slippage)`. Nhưng nó SAI, và
    // sai theo hướng nguy hiểm: ngưỡng nới ra theo kích thước vault.
    //
    //   vault 10.000 USDC, swap 1.000, slippage 1% của NAV = 100 USDC
    //   -> một giao dịch 1.000 USDC được phép lỗ tới 10% (!) mà vẫn "hợp lệ"
    //
    //   vault 1.000.000 USDC, cùng giao dịch đó
    //   -> được phép lỗ 10.000 USDC = LỖ 1000% giá trị giao dịch
    //
    // Vault càng lớn, một giao dịch càng dễ rút ruột. Đúng ngược với cái ta muốn.
    //
    // Nên ngưỡng phải neo vào giá trị GIAO DỊCH, không vào tổng tài sản:
    //
    //   NAV_sau >= NAV_trước − value_in × slippage
    //
    // Giờ 1% luôn là 1% của khoản đem đi, bất kể vault to nhỏ thế nào.
    let max_loss = (value_in as u128)
        .checked_mul(MAX_SLIPPAGE_BPS as u128)
        .ok_or(TribeError::MathOverflow)?
        .checked_div(BPS_SCALE as u128)
        .ok_or(TribeError::MathOverflow)?;

    let floor = (before.total_value as u128).saturating_sub(max_loss);

    require!((after.total_value as u128) >= floor, TribeError::ValueLost);

    // (2) + (3) Trần exposure từng asset.
    //
    // NAV-delta không bắt được một đòn: swap 1M USDC lấy 1M token X (X có oracle
    // hợp lệ nhưng thanh khoản mỏng). Tổng giá trị KHÔNG giảm → qua được (1). Nhưng
    // vault vừa đổi toàn bộ tài sản lấy một thứ không bán ra được.
    //
    // Trần exposure chặn đòn đó. Và vì nó tính trên NAV CUỐI, nó chặn luôn cả trò
    // lách trần bằng cách chẻ nhỏ thành nhiều lệnh — lệnh thứ N, cái vượt trần, sẽ
    // revert bất kể trước đó chia nhỏ thế nào.
    if after.total_value > 0 {
        for i in 0..(ctx.accounts.vault.asset_count as usize) {
            let asset: Account<Asset> = Account::try_from(&nav_accounts[i * 3])?;
            if asset.max_exposure_bps == 0 {
                continue; // 0 = không giới hạn
            }

            // value_i / nav > max_bps / SCALE  <=>  value_i × SCALE > nav × max_bps
            // Nhân chéo để không mất chính xác vì chia số nguyên.
            let lhs = (after.values[i] as u128)
                .checked_mul(BPS_SCALE as u128)
                .ok_or(TribeError::MathOverflow)?;
            let rhs = (after.total_value as u128)
                .checked_mul(asset.max_exposure_bps as u128)
                .ok_or(TribeError::MathOverflow)?;

            require!(lhs <= rhs, TribeError::ExposureExceeded);
        }
    }

    // (4) `reserved` vẫn còn nguyên trong vault.
    //
    // compute_nav trừ reserved khỏi balance, nên nếu adapter tiêu vào phần đã hứa,
    // `after.balances[i]` sẽ tụt xuống dưới 0 và saturate về 0 — NAV tụt theo và
    // (1) bắt được. Nhưng bắt gián tiếp là không đủ rõ ràng cho một bất biến quan
    // trọng như vậy, nên kiểm tra thẳng: số dư thật phải >= reserved.
    for i in 0..(ctx.accounts.vault.asset_count as usize) {
        let asset: Account<Asset> = Account::try_from(&nav_accounts[i * 3])?;
        let token: anchor_spl::token_interface::TokenAccount =
            anchor_spl::token_interface::TokenAccount::try_deserialize(
                &mut &nav_accounts[i * 3 + 1].try_borrow_data()?[..],
            )?;

        require!(
            token.amount >= asset.reserved,
            TribeError::ReservedViolated
        );
    }

    emit!(ExecuteEvent {
        adapter: adapter.program_id,
        action_id,
        mint_in: ctx.accounts.asset_in.mint,
        amount_in,
        nav_before: before.total_value,
        nav_after: after.total_value,
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(action_id: ActionId)]
pub struct ExecuteAction<'info> {
    /// Ai được bấm execute — đọc từ `vault.executor`, KHÔNG phải `vault.admin`.
    ///
    /// MVP: executor = admin. Sau: PDA của governance, và từ đó "được phép execute"
    /// nghĩa là "proposal đã pass + hết timelock". Đúng một chỗ phải sửa.
    pub authority: Signer<'info>,

    #[account(
        seeds = [VAULT_SEED],
        bump = vault.bump,
        constraint = vault.executor == authority.key() @ TribeError::Unauthorized,
    )]
    pub vault: Box<Account<'info, Vault>>,

    /// CHECK: PDA ký cho adapter. Đây là quyền lực được cấp — và cũng là lý do mọi
    /// thứ trong hàm này phải kiểm tra chặt.
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

    /// CHECK: xác thực khớp `adapter.program_id` trong hàm.
    pub adapter_program: UncheckedAccount<'info>,

    /// Bộ ba (adapter × action_id × asset_in) phải đã được whitelist.
    ///
    /// Account này KHÔNG TỒN TẠI cho một bộ ba chưa đăng ký, nên Anchor fail ngay ở
    /// khâu giải seeds. Đó chính là cách `stake × jito × ETH` bị chặn: không ai đăng
    /// ký nổi capability đó, nên không ai execute nổi nó.
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

    /// Asset mà vault chi ra. Vault chỉ cần biết đúng cái này — mọi asset khác
    /// (kể cả asset nhận về) được kiểm chứng qua NAV-delta, không cần khai trước.
    #[account(
        seeds = [ASSET_SEED, vault.key().as_ref(), asset_in.mint.as_ref()],
        bump = asset_in.bump,
        constraint = asset_in.vault == vault.key() @ TribeError::AssetNotRegistered,
    )]
    pub asset_in: Box<Account<'info, Asset>>,
}

/// Đăng ký một adapter. Governance vote + timelock 7 ngày.
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

/// Gỡ adapter. KHÔNG timelock — dừng chảy máu phải nhanh.
/// Mở cửa thì chậm (7 ngày), đóng cửa thì tức thì.
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
    pub amount_in: u64,
    pub nav_before: u64,
    pub nav_after: u64,
}
