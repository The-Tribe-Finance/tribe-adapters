use anchor_lang::prelude::*;

use crate::constants::MAX_ASSETS;

/// The protocol's central config and ledger. Exactly one Vault exists.
#[account]
#[derive(InitSpace)]
pub struct Vault {
    /// Admin authority: whitelist assets, pause, collect management fees.
    pub admin: Pubkey,

    /// Who may call `execute_action` — where money leaves the vault.
    ///
    /// MVP: the admin. Later: a tribe-governance PDA, meaning "the proposal passed and
    /// its timelock elapsed".
    ///
    /// This field exists FROM DAY ONE even though the MVP does not need it, because
    /// changing the layout of an account that holds real money is the most dangerous
    /// operation in a protocol's life. Cheap now, very expensive later. Handing control
    /// to governance is then a single `set_executor` call.
    pub executor: Pubkey,

    /// Wallet that receives the protocol's share of fees.
    pub treasury: Pubkey,
    /// Mint of the vault share (a plain SPL token, freely transferable).
    pub share_mint: Pubkey,
    /// PDA that owns every one of the vault's token accounts. Nobody holds its key.
    pub vault_authority: Pubkey,

    /// Total shares outstanding, per the vault's own books.
    ///
    /// NOTE: this is NOT equal to `share_mint.supply`. The first deposit adds
    /// MINIMUM_LIQUIDITY here without minting matching tokens — those are the shares
    /// permanently locked to defeat the inflation attack. So after the first deposit:
    ///
    ///     total_shares == share_mint.supply + MINIMUM_LIQUIDITY
    ///
    /// Do NOT "fix" the two numbers to match — that reopens the hole.
    pub total_shares: u64,
    /// Number of registered assets.
    pub asset_count: u8,

    /// Halts deposits and all actions that ENTER a position (buy/lend/stake).
    ///
    /// Does NOT halt redemptions, and does NOT halt actions that EXIT a position
    /// (sell/unlend/unstake). If exits were blocked too, assets already lent into
    /// Kamino would be stuck there forever.
    pub paused: bool,

    pub vault_authority_bump: u8,
    pub bump: u8,

    /// Mints of every registered asset. Used to force every NAV computation to supply
    /// all of them — a missing asset means a wrong NAV, and a wrong NAV means wrong
    /// mints and wrong redemptions.
    #[max_len(MAX_ASSETS)]
    pub asset_mints: Vec<Pubkey>,

    /// Monotonic counter handing a unique id to each redeem ticket.
    pub redeem_ticket_counter: u64,
}

/// How an asset is priced.
///
/// The MVP vault only holds SPL tokens priced by Pyth. But lending and staking are
/// fundamentally different: the vault no longer holds a token, it holds a *position*
/// (Kamino's kToken, Marinade's stake account) — and those have no Pyth feed. Their
/// value has to be asked of the protocol itself.
///
/// This enum exists from the start so that adding lend/stake later is just another
/// branch, NOT a migration of `Asset` — and migrating an account that holds real money
/// is the most expensive and dangerous thing you can do to a live protocol.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum AssetKind {
    /// A plain SPL token. Value = balance × Pyth price. (MVP)
    SplToken,
    /// A lending position (Kamino kToken, MarginFi...). Priced via an adapter.
    LendPosition,
    /// A staking position (stake account, LST). Priced via an adapter.
    StakePosition,
}

/// An asset the vault is allowed to hold.
#[account]
#[derive(InitSpace)]
pub struct Asset {
    pub vault: Pubkey,
    pub mint: Pubkey,
    /// The vault's token account for this asset (owned by vault_authority).
    pub token_account: Pubkey,
    /// How this asset is priced.
    pub kind: AssetKind,
    /// Address of the Pyth price update account. Only used when kind = SplToken.
    pub oracle: Pubkey,
    /// Pyth feed id (32 bytes), used to prove the feed matches the asset.
    pub feed_id: [u8; 32],
    /// Adapter responsible for pricing this position.
    /// Only used when kind = LendPosition | StakePosition. Default for SplToken.
    pub pricing_adapter: Pubkey,
    /// Cached mint decimals, so NAV does not have to load the Mint account.
    pub decimals: u8,
    /// Index into Vault::asset_mints. Doubles as the bit index in a redeem ticket.
    pub index: u8,

    /// Whether the vault may HOLD this asset.
    ///
    /// Disabled = do not take any more in (deposit, buy, receive as a receipt token) —
    /// but redemptions and sales still work. Otherwise a broken asset would be stuck in
    /// the vault forever.
    ///
    /// This is NOT the place that answers "what may be done with this asset, and where".
    /// That question belongs to `Capability`, because it depends on the triple
    /// (adapter × action × asset): ETH can be lent on Kamino but never staked on Jito
    /// (Solana does not stake ETH); BTC swaps on Jupiter but may have no Kamino market.
    /// A single boolean on Asset cannot express that.
    pub enabled: bool,

    /// Cap on this asset's weight in NAV, in BPS_SCALE units (1_000_000 = 100%).
    /// 0 = uncapped.
    ///
    /// Checked after every action: if this asset's value exceeds X% of NAV, revert.
    /// This is defense in depth — measuring balances after the CPI only catches "the
    /// adapter spent more than amount_in"; it does NOT catch "a perfectly valid trade
    /// that dumps the whole vault into one protocol". And because the check runs against
    /// FINAL NAV, it also defeats splitting one big trade into many small ones.
    pub max_exposure_bps: u64,

    /// Tokens already PROMISED to redeemers who have not finished claiming.
    ///
    /// # Why this exists
    ///
    /// `redeem_request` burns shares and locks in the amount owed, but the user must
    /// then call `claim_asset` repeatedly to collect (24 assets do not fit in one
    /// transaction). Between those two moments there is a gap.
    ///
    /// Without `reserved`, that gap is a real exploit:
    ///
    ///   1. `execute_action` can swap away EXACTLY the tokens already promised. By the
    ///      time the user claims, the vault is short — the transfer fails, their shares
    ///      are already burned, and the assets are stranded in a worthless ticket.
    ///
    ///   2. NAV would count assets already owed to someone else → NAV reads too high →
    ///      later depositors pay more per share than the shares are actually worth.
    ///
    /// So `reserved` is subtracted from BOTH:
    ///
    ///   - NAV                     = Σ price × (balance − reserved)
    ///   - what an adapter may spend =            balance − reserved
    ///
    /// Increased by `redeem_request`, decreased by `claim_asset`.
    pub reserved: u64,

    pub bump: u8,
}

/// Identifies an action. Defined by the ADAPTER — the vault does not know its meaning.
///
/// # Why this is NOT an enum in the vault
///
/// It used to be `enum Action { Buy, Sell, Lend, Stake, ... }`. But then adding a new
/// action (ProvideLiquidity, Short, ...) means editing that enum — i.e. **upgrading the
/// program that holds the money**. Exactly what this whole architecture exists to avoid.
///
/// Worse: the enum fed into `Capability`'s seeds, so changing it changed every PDA.
///
/// So the vault keeps only a number. It does NOT need to know that `2` means "lend" —
/// it only needs to know whether the triple `(adapter, action_id, asset)` has been
/// whitelisted by governance. Meaning lives in the adapter; the vault verifies RESULTS.
///
/// Adding an action = deploy a new adapter + governance writes one row into the
/// registry. **The vault does not change a single line of code.**
pub type ActionId = u8;

/// The two adapter kinds, which have FUNDAMENTALLY DIFFERENT trust levels.
///
/// This is the most important distinction in the entire adapter architecture. Conflating
/// the two is the source of every confused argument about "should the vault trust
/// adapters" — the answer differs by kind.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum AdapterKind {
    /// **UNTRUSTED.** Executes trades (swap, lend, stake).
    ///
    /// The vault CAN VERIFY its result by value-delta: measure the vault's value before
    /// and after the CPI, and require `value_after >= value_before − value_in × slippage`.
    /// A buggy adapter, a malicious one, or a compromised target protocol — all revert.
    ///
    /// Because it is verifiable, it need not be trusted. Upgrade it freely. (Still worth
    /// setting immutable, to close the upgrade-authority attack surface entirely.)
    Action,

    /// **TRUSTED.** Prices a position (kToken, LST) → feeds NAV directly.
    ///
    /// The vault has NOTHING to measure it against. With pricing there is no "check the
    /// balance" — the number the adapter returns IS the truth the vault mints shares on.
    ///
    /// A wrong number here — even from an innocent bug — means wrong NAV, wrong share
    /// mints, drained vault. So this must be audited and locked down AS STRICTLY AS THE
    /// CORE, not treated as a plugin. Setting it immutable is mandatory, not advisory.
    Pricing,
}

/// One permitted (adapter × action × asset) triple.
///
/// # Why this needs to be an account, not a flag on Asset
///
/// "Permitted" is NOT a property of an asset. It is a property of the triple:
///
///   swap  × jupiter × BTC  -> ✅ Jupiter routes nearly any SPL token
///   lend  × kamino  × ETH  -> ✅ Kamino has a wrapped-ETH market
///   lend  × kamino  × BTC  -> ❓ only if Kamino actually opened a market for THAT
///                                specific wrapped BTC (Solana has several)
///   stake × jito    × ETH  -> ❌ NEVER. Jito stakes SOL; ETH on Solana is a wrapper,
///                                and no validator can stake it.
///
/// A single `enabled: bool` on Asset answers "can this asset be used at all", but the
/// real question is "used FOR WHAT, and WHERE". Without Capability, an `execute_lend`
/// would read the very same flag `execute_swap` reads, and happily lend BTC into a
/// protocol that has no BTC market.
///
/// # Why an account, not a bitmask
///
/// A bit cannot hold an address. Kamino needs to know which reserve; Jito needs to know
/// which stake pool (`venue`, below). If the vault does not store that address, the
/// caller must supply it — and then the vault HAS NO WAY of knowing they supplied the
/// right one. Precisely the class of hole `execute.rs` carefully avoids.
#[account]
#[derive(InitSpace)]
pub struct Capability {
    pub vault: Pubkey,
    /// Program id of the adapter allowed to do this.
    pub adapter: Pubkey,

    /// What to do — a number defined by the ADAPTER. The vault has no idea what it means.
    /// Type is `ActionId` (= u8); written as u8 because `InitSpace` cannot see through
    /// type aliases.
    pub action_id: u8,

    /// Whether this action ENTERS a new position.
    ///
    /// Governance sets this when registering the capability. The vault cannot infer it
    /// from `action_id` (it does not understand the semantics), so it must be told.
    ///
    /// Deliberately asymmetric:
    ///
    ///   is_entry = true  (buy, lend, stake)      -> pause CAN block it
    ///   is_entry = false (sell, unlend, unstake) -> pause NEVER blocks it
    ///
    /// Why exits are never blocked: if the vault lent ETH into Kamino and someone then
    /// pauses the protocol (or disables that capability), and Unlend were blocked too,
    /// the money would be STUCK IN KAMINO FOREVER. Same philosophy as "redemptions are
    /// never paused".
    pub is_entry: bool,

    /// Which asset (the input asset's mint).
    pub mint: Pubkey,

    /// The protocol's own account for this exact (protocol, asset) pair.
    ///
    ///   lend  × kamino × ETH -> Kamino's ETH reserve account
    ///   stake × jito   × SOL -> Jito's stake pool
    ///   buy/sell × jupiter   -> Pubkey::default() (Jupiter routes itself; not needed)
    ///
    /// This is why Capability must be an account: somewhere to keep this address.
    pub venue: Pubkey,

    /// Mint of the receipt token this action produces (kToken, jitoSOL).
    ///
    /// `Pubkey::default()` = this action produces no receipt (a swap). Governance sets
    /// this at registration — the vault cannot infer it from `action_id`.
    ///
    /// # This is the single most important check in the whole Capability design
    ///
    /// The receipt mint MUST already be registered as a vault Asset. Without that check:
    /// the vault lends 1M USDC into Kamino, receives kTokens THAT NAV CANNOT SEE. NAV
    /// drops by exactly 1M. The vault did not *lose* the money — it just went blind. But
    /// the consequences are identical: the next depositor mints shares at an artificially
    /// cheap price, and every existing shareholder is diluted by exactly 1M USDC.
    ///
    /// A lend that is perfectly valid at the CPI level still drains the vault, purely
    /// because the accounting cannot see the asset.
    pub receipt_mint: Pubkey,

    /// Cap on EACH execution, in the unit of account (USDC, 6 decimals). 0 = uncapped.
    ///
    /// Stops one single trade from draining the vault. The post-CPI checks only catch
    /// "the adapter spent more than amount_in" — they do NOT catch "amount_in was the
    /// entire vault". If the Kamino adapter has a bug (or Kamino itself gets hacked),
    /// the damage is bounded by this number instead of by total NAV.
    pub max_notional: u64,

    /// When this capability becomes usable (after the 7-day timelock).
    ///
    /// Registering a capability opens a NEW PATH for funds to leave the vault — as
    /// dangerous as adding an adapter, so it waits just as long. Removal is immediate.
    pub active_at: i64,

    pub enabled: bool,
    pub bump: u8,
}

/// An adapter allowed to move or price the vault's assets.
///
/// Each adapter is **one program, immutable, one action**. NOT one big program that
/// accumulates features and gets upgraded over time.
///
/// # Why: adding an action must not touch any code that already runs
///
/// ```text
/// DON'T                            DO
/// ─────                            ──
/// one "universal" adapter          one adapter per action, deployed once
/// add lending -> upgrade adapter   add lending -> deploy a NEW adapter
///   -> the old swap code is at risk  -> the swap adapter is untouched
///   -> one bad upgrade breaks all    -> a bad new adapter only reverts itself
/// upgrade authority = attack surface  adapters can be set immutable
/// ```
///
/// Lifecycle when adding staking:
///
///   1. write & audit a Staking adapter (a new, independent program)
///   2. deploy → new program id
///   3. governance votes to add that id to the registry  ← JUST ONE ROW OF DATA
///   4. (if it yields a new asset) add the matching Pricing adapter
///
///   → the swap adapter, the lending adapter, THE VAULT PROGRAM: untouched.
///
/// The thing that "changes constantly" is only the **registry** — data in state, not
/// code. The programs themselves stand still. Old ones are not endangered by new ones;
/// a broken new one only reverts itself.
///
/// Removing a bad adapter = governance deletes it from the registry, effective
/// IMMEDIATELY (because it is data, not code).
#[account]
#[derive(InitSpace)]
pub struct Adapter {
    pub vault: Pubkey,
    /// The adapter's program id. The vault only ever CPIs into this exact address.
    pub program_id: Pubkey,

    /// Action (untrusted, verifiable) or Pricing (trusted, unverifiable).
    /// See `AdapterKind` — these two must NEVER be conflated.
    pub kind: AdapterKind,

    /// Display label: "jupiter", "kamino", "jito"...
    #[max_len(32)]
    pub label: String,
    /// On/off. Off means no more executions through this adapter.
    pub enabled: bool,
    /// When the adapter becomes usable (after its timelock). Unusable before then.
    pub active_at: i64,
    pub bump: u8,
}

/// A claim ticket for one in-kind redemption.
///
/// The vault can hold up to 24 assets, which cannot all be paid out in a single
/// transaction (Solana's account and compute limits). So redemption is three steps:
///
///   redeem_request  -> burn shares, lock in the amount owed for each asset
///   claim_asset     -> called repeatedly, one asset at a time
///   close_ticket    -> close the ticket and reclaim rent, once everything is claimed
///
/// Between those steps there is an in-between state: the shares are burned but the
/// assets have not all arrived. The right to those assets lives in this account, so it
/// must be immutable once created: the amounts are fixed at burn time and do not depend
/// on later NAV. However prices move, the redeemer still receives exactly the share of
/// assets they were entitled to at the moment they burned.
#[account]
#[derive(InitSpace)]
pub struct RedeemTicket {
    pub vault: Pubkey,
    pub owner: Pubkey,
    /// Unique id, taken from Vault::redeem_ticket_counter.
    pub ticket_id: u64,
    /// Shares burned to create this ticket.
    pub shares_burned: u64,

    /// Amount owed per asset; indices match Vault::asset_mints.
    #[max_len(MAX_ASSETS)]
    pub amounts: Vec<u64>,
    /// Snapshot of each asset's mint at ticket creation. If an admin adds or removes
    /// assets in the meantime, the ticket still pays exactly what was promised.
    #[max_len(MAX_ASSETS)]
    pub asset_mints: Vec<Pubkey>,

    /// Bitmap: bit i set means asset i has been claimed.
    /// This is what blocks double-claims — the single most important guard in redemption.
    pub claimed_mask: u32,
    /// Assets still owed. Once zero, the ticket can be closed.
    pub remaining_count: u8,

    pub created_at: i64,
    pub bump: u8,
}

impl RedeemTicket {
    pub fn is_claimed(&self, index: u8) -> bool {
        self.claimed_mask & (1u32 << index) != 0
    }

    pub fn mark_claimed(&mut self, index: u8) {
        self.claimed_mask |= 1u32 << index;
    }
}
