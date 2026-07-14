use anchor_lang::prelude::*;
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

declare_id!("rZfFySp7dkDrQMvYAP2AQiodhp2u5gPKnas2539oDKn");

/// # The Tribe — Community Vault
///
/// Vault giữ tiền của cộng đồng. Program này cố tình làm ít việc nhất có thể:
/// nhận tiền, trả tiền, đếm share, tính NAV. Governance và execution nằm ở
/// program khác, để chỗ giữ tiền càng ít phải upgrade càng tốt.
///
/// Ba bất biến không bao giờ được phá:
///
///   1. NAV luôn tính từ TOÀN BỘ asset. Thiếu một cái là fail.
///   2. Làm tròn luôn nghiêng về vault. Phần lẻ thuộc về người còn nắm share.
///   3. Redeem không bao giờ bị pause. Tiền người dùng luôn rút được.
#[program]
pub mod tribe_vault {
    use super::*;

    /// Khởi tạo vault. Chỉ gọi được một lần.
    pub fn initialize_vault(ctx: Context<InitializeVault>) -> Result<()> {
        let vault = &mut ctx.accounts.vault;

        vault.admin = ctx.accounts.admin.key();
        // MVP: admin tự bấm execute. Tầng 3 đổi sang PDA của tribe-governance
        // bằng `set_executor` — đúng một chỗ phải sửa, đó là lý do tách execution
        // ra khỏi governance ngay từ đầu.
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

    /// Thêm một tài sản vào whitelist.
    ///
    /// Đây là quyền nguy hiểm: thêm token rác kèm oracle giả là rút sạch vault.
    /// MVP để admin nắm, sẽ chuyển sang governance khi protocol trưởng thành.
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
        // Chưa hứa gì cho ai. Ghi tường minh thay vì dựa vào việc Anchor zero-fill —
        // một field kế toán sai giá trị khởi tạo là loại bug rất khó tìm về sau.
        asset.reserved = 0;
        asset.bump = ctx.bumps.asset;

        vault.asset_mints.push(mint_key);
        vault.asset_count = index.checked_add(1).ok_or(TribeError::MathOverflow)?;

        Ok(())
    }

    /// Bật/tắt pause.
    ///
    /// Chặn deposit và các action VÀO vị thế (buy/lend/stake). KHÔNG chặn redeem,
    /// và KHÔNG chặn các action THOÁT vị thế (sell/unlend/unstake) — nếu chặn,
    /// tài sản đã lend vào Kamino sẽ kẹt vĩnh viễn ở đó.
    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.vault.paused = paused;
        Ok(())
    }

    /// Chuyển quyền execute sang một địa chỉ khác.
    ///
    /// Đây là cây cầu duy nhất từ MVP sang governance. MVP: executor = admin.
    /// Tầng 3: gọi lệnh này một lần, trỏ executor vào PDA của tribe-governance —
    /// từ đó "được phép execute" nghĩa là "proposal đã pass + hết timelock 24h",
    /// và admin không còn tự ý chuyển tiền được nữa.
    pub fn set_executor(ctx: Context<AdminOnly>, executor: Pubkey) -> Result<()> {
        ctx.accounts.vault.executor = executor;
        Ok(())
    }

    /// Đăng ký một bộ ba (adapter × action × asset) được phép thực hiện.
    ///
    /// Xem `state::Capability` để hiểu vì sao "được phép" không thể là một cờ
    /// boolean trên Asset.
    ///
    /// MVP: admin đăng ký. Nhưng capability MỞ MỘT ĐƯỜNG TIỀN MỚI ra khỏi vault —
    /// nguy hiểm ngang việc thêm adapter — nên có lẽ phải chuyển sang governance
    /// ngay từ Tầng 3, không đợi tới Tầng 4.
    pub fn register_capability(
        ctx: Context<RegisterCapability>,
        action_id: ActionId,
        is_entry: bool,
        venue: Pubkey,
        max_notional: u64,
    ) -> Result<()> {
        let clock = Clock::get()?;

        // Adapter phải là loại ACTION — Pricing adapter không di chuyển tiền.
        let adapter = &ctx.accounts.adapter;
        require!(
            adapter.kind == AdapterKind::Action,
            TribeError::WrongAdapterKind
        );

        // Adapter phải đã sống thật: enabled và đã qua timelock 7 ngày của chính
        // nó. Không cho phép cấp quyền cho một adapter chưa có hiệu lực.
        require!(adapter.enabled, TribeError::AdapterDisabled);
        require!(
            clock.unix_timestamp >= adapter.active_at,
            TribeError::AdapterNotActive
        );

        // Asset đầu vào phải đã đăng ký trong đúng vault này.
        require_keys_eq!(
            ctx.accounts.asset.vault,
            ctx.accounts.vault.key(),
            TribeError::AssetNotRegistered
        );

        // --- Kiểm tra sống còn: receipt token phải được NAV nhìn thấy ---
        //
        // Vault KHÔNG suy ra được từ `action_id` là action này có sinh receipt token
        // hay không — nó không hiểu ngữ nghĩa. Nên governance phải KHAI BÁO: truyền
        // `receipt_asset` vào nếu action sinh ra kToken/LST.
        //
        // Và nếu có khai, vault kiểm tra chéo rất chặt. Không có kiểm tra này thì
        // vault lend 1M USDC đi, nhận về kToken mà NAV không hề biết tới — NAV tụt
        // đúng 1M, người deposit sau mint share với giá rẻ giả tạo, và mọi người
        // đang giữ share bị pha loãng đúng 1M. Một lệnh lend hợp lệ hoàn toàn về mặt
        // CPI vẫn rút ruột được vault, chỉ vì kế toán mù.
        let receipt_mint = match ctx.accounts.receipt_asset.as_ref() {
            Some(receipt) => {
                require_keys_eq!(
                    receipt.vault,
                    ctx.accounts.vault.key(),
                    TribeError::ReceiptAssetNotRegistered
                );

                // Receipt token là một POSITION, không phải SPL token thường — nếu
                // nó là SplToken thì đã có Pyth feed và không cần pricing adapter.
                require!(
                    receipt.kind != AssetKind::SplToken,
                    TribeError::ReceiptKindMismatch
                );

                // Position phải có pricing adapter, nếu không NAV sẽ fail thẳng khi
                // gặp nó — và vault đóng băng ngay sau lệnh lend đầu tiên.
                require!(
                    receipt.pricing_adapter != Pubkey::default(),
                    TribeError::PricingAdapterMismatch
                );

                receipt.mint
            }
            // Swap không sinh receipt mới — vault vẫn giữ token thường.
            None => Pubkey::default(),
        };

        let capability = &mut ctx.accounts.capability;
        capability.vault = ctx.accounts.vault.key();
        capability.adapter = adapter.program_id;
        capability.action_id = action_id;
        capability.is_entry = is_entry;
        capability.mint = ctx.accounts.asset.mint;
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

    /// Tắt một capability. Có hiệu lực NGAY, không timelock.
    ///
    /// Đóng một đường tiền ra khỏi vault phải nhanh. Nhưng lưu ý: tắt một capability
    /// `is_entry = true` (lend) KHÔNG khoá được tiền đã lend — capability thoát
    /// (`is_entry = false`, unlend) vẫn chạy được, vì cửa ra không bao giờ đóng.
    pub fn disable_capability(ctx: Context<DisableCapability>) -> Result<()> {
        ctx.accounts.capability.enabled = false;
        Ok(())
    }

    /// Đăng ký adapter mới (Jupiter, Kamino...).
    ///
    /// Adapter KHÔNG dùng được ngay: phải chờ hết timelock 7 ngày. Dài hơn hẳn
    /// timelock giao dịch thường (24h) và có chủ đích — adapter được cấp quyền
    /// điều khiển tiền vault, nên cộng đồng cần thời gian soi và redeem thoát
    /// ra nếu thấy đáng ngờ.
    ///
    /// MVP: admin đăng ký. Sau này: governance vote.
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
        // Action (untrusted, verify được bằng NAV-delta) hay Pricing (trusted, feed
        // thẳng vào NAV). Hai mức tin cậy khác nhau về bản chất — xem AdapterKind.
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

    /// Tắt một adapter. Có hiệu lực NGAY, không timelock.
    ///
    /// Bất đối xứng có chủ đích: mở cửa thì chậm (7 ngày), đóng cửa thì tức
    /// thì. Khi phát hiện adapter hỏng, cầm máu phải nhanh.
    pub fn disable_adapter(ctx: Context<DisableAdapter>) -> Result<()> {
        ctx.accounts.adapter.enabled = false;
        Ok(())
    }

    /// Thực thi một action lên tài sản của vault, qua adapter.
    ///
    /// MỘT cổng duy nhất cho MỌI action — swap, lend, stake, LP, và cả những action
    /// chưa tồn tại. Vault KHÔNG hiểu ngữ nghĩa của `action_id`; nó chỉ kiểm chứng
    /// kết quả bằng NAV-delta.
    ///
    /// Đó là lý do thêm một action mới không phải upgrade vault: deploy adapter mới
    /// + governance ghi một dòng vào registry, xong.
    ///
    /// Xem `execute.rs`.
    pub fn execute_action<'info>(
        ctx: Context<'_, '_, 'info, 'info, ExecuteAction<'info>>,
        action_id: ActionId,
        amount_in: u64,
        payload: Vec<u8>,
    ) -> Result<()> {
        execute::execute_action(ctx, action_id, amount_in, payload)
    }

    /// Gửi tài sản vào vault, nhận share.
    ///
    /// remaining_accounts: bộ ba [Asset, token account vault, Pyth price] cho
    /// MỌI asset đã đăng ký, đúng thứ tự `Vault::asset_mints`.
    pub fn deposit<'info>(
        ctx: Context<'_, '_, 'info, 'info, Deposit<'info>>,
        amount: u64,
    ) -> Result<()> {
        require!(!ctx.accounts.vault.paused, TribeError::VaultPaused);
        require!(amount >= MIN_DEPOSIT, TribeError::DepositTooSmall);

        let clock = Clock::get()?;

        // NAV phải được tính TRƯỚC khi token của người deposit chạm vào vault.
        //
        // Nếu tính sau, tiền của họ đã nằm trong NAV, nên họ được mint share
        // dựa trên chính khoản vừa gửi — tự pha loãng chính mình và mọi người.
        // Thứ tự ở đây là bắt buộc, không phải phong cách.
        let vault_key = ctx.accounts.vault.key();
        let snapshot = compute_nav(
            &ctx.accounts.vault,
            &vault_key,
            ctx.remaining_accounts,
            &clock,
        )?;
        let nav_before = snapshot.total_value;
        let total_shares = ctx.accounts.vault.total_shares;

        // Định giá đúng khoản đang gửi, bằng chính giá vừa xác thực ở trên.
        let deposit_value = deposit_value_in_quote(
            &ctx.accounts.vault,
            &ctx.accounts.asset,
            ctx.remaining_accounts,
            amount,
            &clock,
        )?;

        let mut shares = shares_for_deposit(deposit_value, nav_before, total_shares)?;

        // Lần deposit đầu tiên: khóa vĩnh viễn một ít share.
        //
        // Chống inflation attack: không có nó, kẻ tấn công deposit 1 unit (nhận
        // 1 share), rồi chuyển thẳng một lượng lớn token vào vault để thổi giá
        // share lên. Người deposit sau bị làm tròn về 0 share, và tiền của họ
        // rơi vào tay kẻ tấn công. Khóa cứng MINIMUM_LIQUIDITY khiến đòn này
        // không còn lãi.
        if total_shares == 0 {
            require!(shares > MINIMUM_LIQUIDITY, TribeError::DepositTooSmall);
            shares = shares
                .checked_sub(MINIMUM_LIQUIDITY)
                .ok_or(TribeError::MathOverflow)?;
            ctx.accounts.vault.total_shares = MINIMUM_LIQUIDITY;
        }

        require!(shares > 0, TribeError::ZeroSharesMinted);

        // Chuyển token vào vault trước, mint share sau.
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

    /// Bước 1/3 của redeem: burn share, chốt cứng phần tài sản được nhận.
    ///
    /// KHÔNG bị chặn bởi pause — người dùng luôn rút được tiền.
    ///
    /// Vault giữ tới 24 asset nên không thể trả hết trong một transaction. Lệnh
    /// này burn share và ghi ra một "phiếu nhận hàng" bất biến. Số lượng chốt
    /// tại đây, không phụ thuộc NAV về sau: giá chạy thế nào thì người redeem
    /// vẫn nhận đúng tỷ lệ tài sản tại thời điểm burn.
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

        // Số dư KHẢ DỤNG, đọc từ token account thật, trừ đi phần đã hứa cho những
        // người redeem trước.
        //
        // KHÔNG dùng oracle ở đây. Redeem là phép chia tỷ lệ thuần — giá không xuất
        // hiện trong công thức. Gọi oracle sẽ làm redeem fail khi giá stale, tức là
        // đúng lúc thị trường hỗn loạn và người dùng cần rút tiền nhất. Xem
        // `nav::available_balances`.
        let balances = available_balances(
            &ctx.accounts.vault,
            &vault_key,
            ctx.remaining_accounts,
        )?;

        // Chốt phần được nhận của từng asset, làm tròn xuống. Phần lẻ ở lại
        // vault cho những người còn nắm share.
        let mut amounts = Vec::with_capacity(asset_count);
        let mut remaining_count: u8 = 0;
        for i in 0..asset_count {
            let amount = pro_rata_amount(balances[i], shares, total_shares)?;
            if amount > 0 {
                remaining_count = remaining_count
                    .checked_add(1)
                    .ok_or(TribeError::MathOverflow)?;

                // KHOÁ phần này lại. Từ giờ tới lúc người dùng claim, không ai được
                // đụng vào: NAV không tính nó, và `execute` không tiêu được nó.
                //
                // Không có bước này, `execute` có thể swap đi đúng số token vừa hứa —
                // tới lúc claim thì vault không còn đủ, share đã burn mất rồi mà tài
                // sản thì kẹt trong một cái phiếu vô dụng.
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

    /// Bước 2/3: rút một asset từ phiếu. Gọi lặp lại cho tới khi hết.
    ///
    /// Chống double-claim bằng bitmap trong ticket. Đây là chốt chặn quan trọng
    /// nhất của redeem: hỏng nó thì một phiếu rút được cùng một asset nhiều lần.
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

        // Asset phải khớp ẢNH CHỤP lúc tạo phiếu, không phải danh sách hiện tại
        // của vault. Admin có thêm/bớt asset giữa chừng thì phiếu vẫn trả đúng
        // những gì đã hứa lúc burn share.
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

            // Nhả khoá: token đã ra khỏi vault, phần "hứa nhưng chưa trả" giảm đi.
            //
            // Phải khớp CHÍNH XÁC với phần đã cộng lúc redeem_request. Nếu trừ hụt,
            // `reserved` sẽ phình lên vĩnh viễn và dần khoá chết cả vault — NAV tụt
            // dần, và adapter không tiêu được tài sản nào nữa.
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

    /// Bước 3/3: đóng phiếu đã claim hết, hoàn rent về cho chủ.
    pub fn close_ticket(ctx: Context<CloseTicket>) -> Result<()> {
        require!(
            ctx.accounts.ticket.remaining_count == 0,
            TribeError::TicketNotFullyClaimed
        );
        Ok(())
    }
}

/// Định giá đúng khoản token đang được gửi vào.
///
/// Dùng lại chính oracle đã xác thực trong `compute_nav` — không đọc giá riêng,
/// để giá dùng cho NAV và giá dùng cho khoản deposit luôn là một. Lấy hai giá
/// khác nhau trong cùng một lệnh là mở đường cho chênh lệch bị khai thác.
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

    // Oracle của asset này nằm ở vị trí thứ 3 trong bộ ba của nó.
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

    /// PDA sở hữu mọi token account của vault. Không ai giữ private key của nó.
    /// CHECK: PDA thuần tuý, chỉ dùng để ký CPI.
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, vault.key().as_ref()],
        bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    /// Mint của share. Quyền mint nằm ở vault_authority — nếu để chỗ khác,
    /// người ta in được share từ hư không và rút sạch vault.
    #[account(
        init,
        payer = admin,
        mint::decimals = SHARE_DECIMALS,
        mint::authority = vault_authority,
    )]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    /// CHECK: chỉ lưu địa chỉ nhận fee, không đọc dữ liệu.
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

    /// CHECK: PDA, dùng làm authority của token account.
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

    /// Token account của vault cho asset này. Authority BẮT BUỘC là
    /// vault_authority — nếu không, tiền nằm dưới quyền người khác.
    #[account(
        init,
        payer = admin,
        token::mint = mint,
        token::authority = vault_authority,
    )]
    pub vault_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: Pyth price update. Được xác thực đầy đủ mỗi lần đọc giá (feed_id,
    /// độ tươi, độ tin cậy) trong `oracle::get_validated_price`.
    pub oracle: UncheckedAccount<'info>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
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
#[instruction(action_id: ActionId)]
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

    /// Asset đầu vào của action này.
    #[account(
        seeds = [ASSET_SEED, vault.key().as_ref(), asset.mint.as_ref()],
        bump = asset.bump,
    )]
    pub asset: Box<Account<'info, Asset>>,

    /// Asset của receipt token (kToken, LST), BẮT BUỘC có mặt khi action là
    /// Lend hoặc Stake. Xem `Capability::receipt_mint` — thiếu nó là NAV mù và
    /// vault bị pha loãng đúng bằng số tiền đem đi lend.
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
            asset.mint.as_ref(),
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

    /// CHECK: PDA ký cho việc mint share.
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

    // Chú ý: KHÔNG kiểm tra `paused` ở đây. Có chủ đích.
    // Redeem phải sống được kể cả khi protocol đóng băng.
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

    /// CHECK: PDA ký cho việc chuyển tài sản ra.
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

    /// `mut` vì claim_asset giảm `asset.reserved` — nhả khoá phần vừa trả cho user.
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
