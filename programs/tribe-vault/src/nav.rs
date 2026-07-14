use anchor_lang::prelude::*;
use anchor_spl::token_interface::TokenAccount;
use pyth_solana_receiver_sdk::price_update::PriceUpdateV2;

use crate::constants::{MAX_ASSETS, QUOTE_DECIMALS};
use crate::errors::TribeError;
use crate::math::asset_value_in_quote;
use crate::oracle::get_validated_price;
use crate::state::{Asset, AssetKind, Vault};

/// Số dư từng asset của vault, kèm tổng NAV.
pub struct NavSnapshot {
    /// NAV tính bằng đơn vị kế toán (USDC, 6 decimals).
    pub total_value: u64,
    /// Số dư thô của từng asset, theo đúng thứ tự Vault::asset_mints.
    /// Dùng cho redeem in-kind: trả đúng token, không quy đổi.
    pub balances: [u64; MAX_ASSETS],
    /// Giá trị (quy về USDC) của từng asset, cùng thứ tự.
    /// Dùng cho kiểm tra exposure: tỷ trọng của asset i trong NAV.
    pub values: [u64; MAX_ASSETS],
}

/// Tính NAV từ remaining_accounts.
///
/// Chờ đợi bộ ba account cho MỖI asset đã đăng ký, đúng thứ tự Vault::asset_mints:
///
///   [Asset PDA, token account của vault, Pyth price update]
///
/// # Vì sao bắt buộc phải đủ, không thiếu một cái
///
/// NAV quyết định mint bao nhiêu share. Nếu người gọi được phép bỏ bớt asset,
/// họ sẽ bỏ đúng những asset đắt tiền để dìm NAV xuống, deposit vào lúc share
/// đang "rẻ giả tạo", rồi ăn phần chênh lệch của tất cả những người còn lại.
/// Nên: thiếu một asset là fail, sai thứ tự là fail, trùng asset là fail.
pub fn compute_nav<'info>(
    vault: &Vault,
    vault_key: &Pubkey,
    remaining: &'info [AccountInfo<'info>],
    clock: &Clock,
) -> Result<NavSnapshot> {
    let asset_count = vault.asset_count as usize;

    // Vault rỗng: NAV = 0, không cần đọc oracle nào.
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

        // Account<T> tự kiểm tra owner + discriminator. Không dùng
        // UncheckedAccount ở đây: nếu không, kẻ tấn công dựng account giả có
        // cùng layout để bơm giá tùy ý.
        let asset: Account<Asset> = Account::try_from(asset_info)?;

        // Asset PDA phải đúng cái mà vault đã đăng ký, đúng vị trí thứ i.
        // Kiểm tra này chặn luôn cả việc truyền trùng một asset nhiều lần.
        require_keys_eq!(asset.vault, *vault_key, TribeError::AssetNotRegistered);
        require!(
            asset.mint == vault.asset_mints[i],
            TribeError::AssetNotRegistered
        );
        require!(asset.index as usize == i, TribeError::AssetNotRegistered);

        // Token account phải đúng cái vault sở hữu cho asset này.
        require_keys_eq!(
            asset.token_account,
            token_info.key(),
            TribeError::AssetNotRegistered
        );
        let token_account: InterfaceAccount<TokenAccount> =
            InterfaceAccount::try_from(token_info)?;

        // NAV chỉ tính phần tài sản vault THẬT SỰ còn sở hữu.
        //
        // `reserved` là phần đã hứa cho những người đã burn share nhưng chưa claim
        // xong. Tiền đó không còn là của những người đang giữ share nữa — nó đang
        // chờ được trả đi. Tính nó vào NAV thì NAV cao hơn thực tế, và người deposit
        // sau sẽ mua share đắt hơn giá trị thật.
        //
        // saturating_sub thay vì checked_sub: nếu vì lý do nào đó reserved > balance
        // (không nên xảy ra), coi như khả dụng = 0 chứ không panic. Thà NAV thấp hơn
        // sự thật còn hơn vault đóng băng — làm tròn luôn nghiêng về vault.
        let balance = token_account.amount.saturating_sub(asset.reserved);
        balances[i] = balance;

        // Định giá theo loại tài sản.
        //
        // MVP chỉ hỗ trợ SplToken. Lend/stake position không có Pyth feed —
        // giá trị của chúng phải hỏi chính protocol đó (Kamino, Marinade...)
        // qua adapter. Chưa làm được thì FAIL thẳng, tuyệt đối không đoán bừa:
        // NAV sai dù chỉ một chút cũng đủ để ai đó mint share rẻ và rút ruột
        // vault. Thà đóng băng còn hơn định giá sai.
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

/// Đọc số dư KHẢ DỤNG của từng asset — KHÔNG đụng tới oracle.
///
/// # Vì sao redeem không được dùng oracle
///
/// Redeem là phép chia tỷ lệ thuần:
///
///   phần_được_nhận[i] = (balance[i] − reserved[i]) × share_burn / total_shares
///
/// Giá không xuất hiện trong công thức. Người redeem nhận đúng tỷ lệ của TỪNG asset
/// (in-kind), không phải một số tiền quy đổi — nên vault không cần biết asset đó
/// đáng bao nhiêu.
///
/// Gọi oracle ở đây không chỉ thừa, nó còn NGUY HIỂM: redeem sẽ fail khi giá stale
/// hoặc confidence quá rộng — tức là đúng lúc thị trường hỗn loạn và người dùng cần
/// rút tiền nhất. Điều đó phá vỡ bất biến quan trọng nhất của protocol:
///
///   **Redeem không bao giờ bị chặn. Tiền người dùng luôn rút được.**
///
/// Bỏ oracle khỏi redeem làm nó MẠNH HƠN, không yếu đi.
///
/// Chờ đợi CẶP account cho mỗi asset (không phải bộ ba như `compute_nav`):
///
///   [Asset PDA, token account của vault]
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

    // Vẫn bắt buộc đủ MỌI asset. Thiếu một cái thì người redeem không nhận được
    // phần của mình ở asset đó — và phần đó ở lại vault vĩnh viễn.
    require!(
        remaining.len() == asset_count * 2,
        TribeError::IncompleteAssetSet
    );

    for i in 0..asset_count {
        let asset_info: &AccountInfo<'info> = &remaining[i * 2];
        let token_info: &AccountInfo<'info> = &remaining[i * 2 + 1];

        // Account<T> tự kiểm tra owner + discriminator — không dùng UncheckedAccount,
        // nếu không kẻ tấn công dựng account giả cùng layout để khai khống số dư.
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

        // Chỉ chia phần CHƯA HỨA cho ai. Phần `reserved` thuộc về những người đã
        // burn share trước và đang chờ claim — không được chia lại cho người sau.
        balances[i] = token_account.amount.saturating_sub(asset.reserved);
    }

    Ok(balances)
}

/// Cộng thêm vào `Asset.reserved` của asset thứ `i`, ghi thẳng xuống account.
///
/// Các `Asset` account đến qua `remaining_accounts` nên Anchor không tự động ghi lại
/// chúng như các account khai báo trong struct `Accounts`. Phải tự deserialize, sửa,
/// rồi serialize ngược — đó là việc hàm này làm.
///
/// Được gọi từ `redeem_request` (cộng vào) và `claim_asset` (trừ đi).
pub fn add_reserved<'info>(
    asset_info: &'info AccountInfo<'info>,
    delta: u64,
) -> Result<()> {
    let mut asset: Account<'info, Asset> = Account::try_from(asset_info)?;

    asset.reserved = asset
        .reserved
        .checked_add(delta)
        .ok_or(TribeError::MathOverflow)?;

    // Ghi ngược xuống account data.
    //
    // Dùng AnchorSerialize (KHÔNG phải try_serialize): try_serialize tự chèn 8 byte
    // discriminator vào đầu, mà ta đang ghi vào vùng SAU discriminator — nó sẽ đè
    // discriminator lên đúng field đầu tiên của struct.
    let mut data = asset_info.try_borrow_mut_data()?;
    let mut cursor: &mut [u8] = &mut data[8..];
    AnchorSerialize::serialize(&*asset, &mut cursor)?;

    Ok(())
}
