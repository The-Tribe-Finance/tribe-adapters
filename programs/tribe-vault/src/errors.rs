use anchor_lang::prelude::*;

#[error_code]
pub enum TribeError {
    #[msg("Math overflow")]
    MathOverflow,

    #[msg("Vault is paused")]
    VaultPaused,

    #[msg("Unauthorized")]
    Unauthorized,

    // --- Oracle ---
    #[msg("Oracle price is stale")]
    StalePrice,
    #[msg("Oracle price is invalid (non-positive)")]
    InvalidPrice,
    #[msg("Oracle confidence interval too wide")]
    PriceConfidenceTooWide,
    #[msg("Oracle feed does not match the asset")]
    OracleFeedMismatch,
    #[msg("Oracle exponent out of supported range")]
    InvalidOracleExponent,

    // --- Asset registry ---
    #[msg("Asset is already registered")]
    AssetAlreadyRegistered,
    #[msg("Asset is not registered in this vault")]
    AssetNotRegistered,
    #[msg("Vault has reached the maximum number of assets")]
    TooManyAssets,
    #[msg("Asset still holds a balance and cannot be removed")]
    AssetNotEmpty,
    #[msg("Asset is disabled")]
    AssetDisabled,

    // --- Deposit ---
    #[msg("Deposit amount is below the minimum")]
    DepositTooSmall,
    #[msg("Deposit would mint zero shares")]
    ZeroSharesMinted,

    // --- Redeem ---
    #[msg("Redeem amount must be greater than zero")]
    ZeroRedeem,
    #[msg("Insufficient share balance")]
    InsufficientShares,
    #[msg("This asset has already been claimed for this ticket")]
    AssetAlreadyClaimed,
    #[msg("Redeem ticket still has unclaimed assets")]
    TicketNotFullyClaimed,
    #[msg("Redeem ticket asset list does not match the vault")]
    TicketAssetMismatch,

    // --- NAV ---
    #[msg("Not all vault assets were provided for NAV computation")]
    IncompleteAssetSet,
    #[msg("Duplicate asset supplied")]
    DuplicateAsset,
    #[msg("Vault NAV is zero while shares are outstanding")]
    InvalidVaultState,
    #[msg("Pricing for this asset kind is not supported yet")]
    PricingKindNotSupported,

    // --- Adapter ---
    #[msg("Adapter is not registered")]
    AdapterNotRegistered,
    #[msg("Adapter is disabled")]
    AdapterDisabled,
    #[msg("Adapter timelock has not elapsed yet")]
    AdapterNotActive,
    #[msg("Adapter program id does not match")]
    AdapterProgramMismatch,

    // --- Capability ---
    #[msg("No capability registered for this (adapter, action, asset) triple")]
    CapabilityNotRegistered,
    #[msg("Capability is disabled")]
    CapabilityDisabled,
    #[msg("Capability timelock has not elapsed yet")]
    CapabilityNotActive,
    #[msg("Capability does not match the requested action or asset")]
    CapabilityMismatch,
    #[msg("Trade size exceeds the capability's maximum notional")]
    NotionalExceeded,
    #[msg("Asset would exceed its maximum share of NAV")]
    ExposureExceeded,

    /// Receipt token (kToken, LST) chưa được đăng ký làm Asset.
    ///
    /// Không có kiểm tra này thì vault lend tiền đi và NAV không nhìn thấy phần
    /// nhận về — NAV tụt đúng bằng số đã lend, người deposit sau mint share rẻ,
    /// người đang giữ share bị pha loãng.
    #[msg("Receipt mint is not registered as a vault asset")]
    ReceiptAssetNotRegistered,
    #[msg("Receipt asset kind does not match the action")]
    ReceiptKindMismatch,
    #[msg("Receipt asset is priced by a different adapter")]
    PricingAdapterMismatch,

    // --- Execution ---
    #[msg("Adapter spent more of the input asset than authorized")]
    ExcessiveSpend,
    #[msg("Adapter returned less than the minimum output")]
    SlippageExceeded,
    #[msg("Adapter touched an asset it was not authorized to move")]
    UnexpectedBalanceChange,
    #[msg("Adapter returned fewer receipt tokens than required")]
    InsufficientReceipt,

    /// Danh sách account gửi cho adapter không chứa vault_authority.
    ///
    /// Không có nó thì CPI không chuyển được token của vault — giao dịch vô nghĩa.
    /// Bắt lỗi sớm ở đây thay vì để CPI fail với thông báo khó hiểu của protocol đích.
    #[msg("Vault authority is missing from the adapter account list")]
    MissingVaultAuthority,

    /// Token account của vault không có trong danh sách gửi cho adapter.
    ///
    /// Vault đo số dư của chúng để kiểm chứng kết quả CPI. Nếu chúng không phải là
    /// thứ protocol đích thao tác lên, vault đang đo nhầm chỗ — và toàn bộ lớp bảo
    /// vệ "chỉ tin số dư" sụp đổ.
    #[msg("Vault token account is missing from the adapter account list")]
    MissingVaultTokenAccount,

    /// Số dư khả dụng (balance − reserved) không đủ cho giao dịch này.
    ///
    /// Phần `reserved` đã hứa cho những người đã burn share nhưng chưa claim xong.
    /// Adapter không được đụng vào — nếu không, tới lúc họ claim thì vault không còn
    /// đủ token để trả, và share thì đã burn mất rồi.
    #[msg("Insufficient available balance (reserved for pending redemptions)")]
    InsufficientAvailableBalance,

    #[msg("Amount must be greater than zero")]
    ZeroAmount,

    /// Adapter sai loại: Pricing adapter không được dùng để execute.
    ///
    /// Pricing adapter là TRUSTED (feed thẳng vào NAV, vault không kiểm chứng được).
    /// Cho nó quyền di chuyển tiền là gộp hai mức tin cậy làm một, và mất luôn lý do
    /// khiến Action adapter an toàn.
    #[msg("Wrong adapter kind for this operation")]
    WrongAdapterKind,

    /// Giao dịch làm vault mất giá trị quá mức cho phép.
    ///
    /// Đây là chốt chặn AGNOSTIC thay cho `min_out`: vault không cần biết action là
    /// swap hay lend, nó chỉ cần biết vault không nghèo đi.
    #[msg("Action lost more value than the allowed slippage")]
    ValueLost,

    /// Sau CPI, số dư thật của một asset tụt xuống dưới phần đã hứa cho redeemer.
    #[msg("Action consumed balance reserved for pending redemptions")]
    ReservedViolated,
}
