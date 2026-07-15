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
    #[msg("Closing a non-last position requires the last asset account")]
    MissingLastAsset,
    #[msg("Vault is already holding the maximum number of positions")]
    TooManyPositions,
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

    /// The receipt token (kToken, LST) is not registered as a vault asset.
    ///
    /// Without this check, the vault lends money out and NAV never sees what comes
    /// back — NAV drops by exactly the amount lent, the next depositor mints shares at
    /// an artificially cheap price, and existing shareholders are diluted.
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

    /// The account list passed to the adapter does not contain vault_authority.
    ///
    /// Without it the CPI cannot move the vault's tokens, making the action pointless.
    /// Failing early here beats letting the target protocol fail with a cryptic error.
    #[msg("Vault authority is missing from the adapter account list")]
    MissingVaultAuthority,

    /// A vault token account is missing from the list passed to the adapter.
    ///
    /// The vault measures those balances to verify the CPI's result. If they are not
    /// the accounts the target protocol actually operates on, the vault is measuring
    /// the wrong thing — and the entire "only trust balances" defense collapses.
    #[msg("Vault token account is missing from the adapter account list")]
    MissingVaultTokenAccount,

    /// Available balance (balance − reserved) is not enough for this action.
    ///
    /// `reserved` is owed to users who already burned shares but have not finished
    /// claiming. An adapter must never touch it — otherwise, by the time they claim,
    /// the vault is short of tokens and their shares are already gone.
    #[msg("Insufficient available balance (reserved for pending redemptions)")]
    InsufficientAvailableBalance,

    #[msg("Amount must be greater than zero")]
    ZeroAmount,

    /// Wrong adapter kind: a Pricing adapter must never be used to execute.
    ///
    /// Pricing adapters are TRUSTED (they feed NAV directly, and the vault has no way
    /// to verify them). Granting one the power to move funds collapses two different
    /// trust levels into one, and destroys the very reason Action adapters are safe.
    #[msg("Wrong adapter kind for this operation")]
    WrongAdapterKind,

    /// The action made the vault lose more value than allowed.
    ///
    /// This is the AGNOSTIC check that replaced `min_out`: the vault does not need to
    /// know whether the action is a swap or a lend, only that it did not get poorer.
    #[msg("Action lost more value than the allowed slippage")]
    ValueLost,

    /// After the CPI, an asset's real balance dropped below what is owed to redeemers.
    #[msg("Action consumed balance reserved for pending redemptions")]
    ReservedViolated,
}
