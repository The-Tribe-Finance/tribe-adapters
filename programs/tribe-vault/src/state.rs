use anchor_lang::prelude::*;

use crate::constants::MAX_ASSETS;

/// Cấu hình và sổ cái trung tâm của protocol. Chỉ tồn tại duy nhất một Vault.
#[account]
#[derive(InitSpace)]
pub struct Vault {
    /// Quyền admin: whitelist asset, pause, thu management fee.
    pub admin: Pubkey,

    /// Ai được phép gọi `execute` — nơi tiền rời khỏi vault.
    ///
    /// MVP: chính là admin. Tầng 3: PDA của tribe-governance, tức là "proposal đã
    /// pass + hết timelock 24h".
    ///
    /// Field này tồn tại NGAY TỪ ĐẦU dù MVP chưa cần, vì đổi layout của account
    /// đang giữ tiền thật là thao tác nguy hiểm nhất trong vòng đời protocol. Rẻ
    /// bây giờ, rất đắt về sau. Chuyển sang governance chỉ là một lệnh set_executor.
    pub executor: Pubkey,

    /// Ví nhận phần fee của protocol.
    pub treasury: Pubkey,
    /// Mint của vault share (SPL token, transfer tự do).
    pub share_mint: Pubkey,
    /// PDA giữ toàn bộ token account của vault. Không ai có private key.
    pub vault_authority: Pubkey,

    /// Tổng share đang lưu hành, theo sổ sách của vault.
    ///
    /// CHÚ Ý: con số này KHÔNG bằng supply của share_mint. Lần deposit đầu tiên
    /// cộng MINIMUM_LIQUIDITY vào đây nhưng không mint ra token tương ứng — đó
    /// chính là phần share bị "khóa vĩnh viễn" để chống inflation attack. Nên
    /// luôn có: total_shares == share_mint.supply + MINIMUM_LIQUIDITY (sau
    /// deposit đầu tiên). Đừng "sửa" cho hai số bằng nhau — sẽ mở lại lỗ hổng.
    pub total_shares: u64,
    /// Số asset đang được đăng ký.
    pub asset_count: u8,

    /// Dừng deposit và các action VÀO vị thế (buy/lend/stake).
    ///
    /// KHÔNG chặn redeem, và KHÔNG chặn các action THOÁT vị thế (sell/unlend/
    /// unstake) — nếu chặn, tài sản đã lend vào Kamino sẽ kẹt vĩnh viễn ở đó.
    pub paused: bool,

    pub vault_authority_bump: u8,
    pub bump: u8,

    /// Danh sách mint của các asset đã đăng ký. Dùng để bắt buộc mọi lệnh tính
    /// NAV phải cung cấp đủ asset — thiếu một cái là NAV sai, và NAV sai thì
    /// mint/redeem sai.
    #[max_len(MAX_ASSETS)]
    pub asset_mints: Vec<Pubkey>,

    /// Bộ đếm tăng dần, cấp id duy nhất cho mỗi redeem ticket.
    pub redeem_ticket_counter: u64,
}

/// Cách định giá một tài sản.
///
/// Vault MVP chỉ giữ SPL token định giá bằng Pyth. Nhưng lending và staking
/// khác về bản chất: vault không còn giữ token nữa mà giữ một *position*
/// (kToken của Kamino, stake account của Marinade), và những thứ đó không có
/// Pyth feed — phải hỏi chính protocol đó mới biết giá trị.
///
/// Enum này có mặt ngay từ đầu để sau này thêm lend/stake chỉ là thêm một
/// nhánh, KHÔNG phải migrate `Asset` — mà migrate account đang giữ tiền thật
/// là thao tác đắt và nguy hiểm nhất trong vòng đời một protocol.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum AssetKind {
    /// SPL token thường. Giá trị = số dư × giá Pyth. (MVP)
    SplToken,
    /// Vị thế cho vay (kToken Kamino, MarginFi...). Định giá qua adapter.
    LendPosition,
    /// Vị thế staking (stake account, LST). Định giá qua adapter.
    StakePosition,
}

/// Một tài sản được phép nắm giữ trong vault.
#[account]
#[derive(InitSpace)]
pub struct Asset {
    pub vault: Pubkey,
    pub mint: Pubkey,
    /// Token account của vault cho asset này (do vault_authority sở hữu).
    pub token_account: Pubkey,
    /// Định giá theo cách nào.
    pub kind: AssetKind,
    /// Địa chỉ price update account của Pyth. Chỉ dùng khi kind = SplToken.
    pub oracle: Pubkey,
    /// Feed id của Pyth (32 byte), dùng để xác thực đúng feed cho đúng asset.
    pub feed_id: [u8; 32],
    /// Adapter chịu trách nhiệm định giá position này.
    /// Chỉ dùng khi kind = LendPosition | StakePosition. SplToken để mặc định.
    pub pricing_adapter: Pubkey,
    /// Số decimals của mint, cache lại để tính NAV không cần nạp Mint account.
    pub decimals: u8,
    /// Vị trí trong Vault::asset_mints. Dùng làm chỉ số bitmap của redeem ticket.
    pub index: u8,

    /// Asset có được GIỮ trong vault không.
    ///
    /// Tắt = không nhận vào thêm (deposit, buy, nhận làm receipt token), nhưng
    /// VẪN redeem và bán ra được — nếu không, asset hỏng sẽ kẹt trong vault
    /// vĩnh viễn.
    ///
    /// Đây KHÔNG phải chỗ trả lời "được làm gì với asset này, ở protocol nào".
    /// Câu hỏi đó thuộc về `Capability`, vì nó phụ thuộc bộ ba
    /// (adapter × action × asset): ETH lend được ở Kamino nhưng không stake được
    /// ở Jito (Solana không stake ETH), BTC swap được ở Jupiter nhưng chưa chắc
    /// có market ở Kamino. Một cờ boolean trên Asset không biểu diễn nổi điều đó.
    pub enabled: bool,

    /// Trần tỷ trọng của asset này trong NAV, tính theo BPS_SCALE (1_000_000 = 100%).
    /// 0 = không giới hạn.
    ///
    /// Kiểm tra SAU mỗi lần execute: nếu giá trị asset này vượt quá X% NAV thì
    /// revert. Đây là phòng thủ theo chiều sâu — kiểm tra số dư sau CPI chỉ bắt
    /// được "adapter tiêu quá amount_in", nó KHÔNG bắt được "lệnh hợp lệ nhưng
    /// dồn toàn bộ vault vào một protocol". Và vì kiểm tra dựa trên NAV cuối
    /// cùng, nó chặn được cả trò lách trần bằng cách chẻ nhỏ thành nhiều lệnh.
    pub max_exposure_bps: u64,

    /// Số lượng token ĐÃ HỨA cho những người redeem nhưng chưa được claim.
    ///
    /// # Vì sao phải có
    ///
    /// `redeem_request` burn share và chốt cứng số lượng phải trả, nhưng người dùng
    /// phải gọi `claim_asset` nhiều lần sau đó mới nhận đủ (24 asset không trả hết
    /// trong một transaction). Giữa hai thời điểm đó tồn tại một khoảng hở.
    ///
    /// Không có `reserved`, khoảng hở đó là một lỗ hổng thật:
    ///
    ///   1. `execute` có thể swap đi ĐÚNG số token đã hứa cho người redeem. Tới lúc
    ///      họ claim, vault không còn đủ → transfer fail → share đã burn mất rồi mà
    ///      tài sản thì kẹt trong một cái phiếu vô dụng.
    ///
    ///   2. NAV tính cả phần đã hứa cho người khác → NAV cao hơn thực tế → người
    ///      deposit sau mua share đắt hơn giá trị thật của nó.
    ///
    /// Nên `reserved` bị trừ khỏi CẢ HAI:
    ///
    ///   - NAV                     = Σ price × (balance − reserved)
    ///   - phần adapter được tiêu  = balance − reserved
    ///
    /// Tăng khi `redeem_request`, giảm khi `claim_asset`.
    pub reserved: u64,

    pub bump: u8,
}

/// Định danh một action, do ADAPTER quy ước — vault không hiểu ngữ nghĩa.
///
/// # Vì sao KHÔNG phải một enum trong vault
///
/// Trước đây đây là `enum Action { Buy, Sell, Lend, Stake, ... }`. Nhưng như vậy,
/// thêm một action mới (ProvideLiquidity, Short, ...) sẽ phải sửa enum —
/// tức là **UPGRADE PROGRAM ĐANG GIỮ TIỀN**. Đúng cái mà cả kiến trúc này sinh ra
/// để tránh.
///
/// Tệ hơn: enum đó ăn vào seed của `Capability`, nên đổi nó là đổi cả PDA.
///
/// Nên vault chỉ giữ một con số. Nó KHÔNG cần biết `2` nghĩa là "lend" — nó chỉ
/// cần biết bộ ba `(adapter, action_id, asset)` đã được governance whitelist hay
/// chưa. Ngữ nghĩa nằm ở adapter; vault chỉ kiểm chứng KẾT QUẢ bằng value-delta.
///
/// Thêm action mới = deploy adapter mới + governance ghi một dòng vào registry.
/// **Vault không đụng một dòng code nào.**
pub type ActionId = u8;

/// Điều kiện mà một Adapter/Capability phải thoả để dùng được.
///
/// Tách ra thành hàm riêng vì logic "cửa vào bị chặn, cửa ra luôn mở" xuất hiện ở
/// nhiều chỗ và phải nhất quán tuyệt đối — sai một chỗ là tiền kẹt vĩnh viễn.
pub fn entry_blocked_by_pause(is_entry: bool, paused: bool) -> bool {
    is_entry && paused
}

/// Một bộ ba (adapter × action × asset) được phép thực hiện.
///
/// # Vì sao cần account này, thay vì một cờ trên Asset
///
/// "Được phép" KHÔNG phải thuộc tính của asset. Nó là thuộc tính của bộ ba:
///
///   swap  × jupiter × BTC  -> ✅ Jupiter route được gần như mọi SPL token
///   lend  × kamino  × ETH  -> ✅ Kamino có market cho wrapped ETH
///   lend  × kamino  × BTC  -> ❓ chỉ khi Kamino thật sự mở market cho ĐÚNG cái
///                                wrapped BTC đó (Solana có nhiều loại wBTC)
///   stake × jito    × ETH  -> ❌ KHÔNG BAO GIỜ. Jito stake SOL; ETH trên Solana
///                                chỉ là token bọc, không validator nào stake nổi.
///
/// Một `enabled: bool` trên Asset trả lời được "asset này có dùng được không",
/// nhưng câu hỏi thật là "dùng được ĐỂ LÀM GÌ, Ở ĐÂU". Không có Capability thì
/// `execute_lend` sẽ đọc đúng cái cờ mà `execute_swap` đang đọc, và cho phép lend
/// BTC vào một protocol không có market cho BTC.
///
/// # Vì sao phải là account, không phải bitmask
///
/// Một cái bit không chứa được địa chỉ. Kamino cần biết reserve nào, Jito cần biết
/// stake pool nào (`venue` bên dưới). Nếu vault không cất sẵn địa chỉ đó thì người
/// gọi phải tự truyền vào — và lúc đó vault KHÔNG CÓ CÁCH NÀO biết họ truyền đúng.
/// Đúng kiểu lỗ hổng mà execute.rs đang cẩn thận tránh.
#[account]
#[derive(InitSpace)]
pub struct Capability {
    pub vault: Pubkey,
    /// Program id của adapter được phép làm việc này.
    pub adapter: Pubkey,

    /// Làm gì — một con số do ADAPTER quy ước. Vault không hiểu ngữ nghĩa.
    /// Kiểu là `ActionId` (= u8); viết thẳng u8 vì `InitSpace` không giải được
    /// type alias.
    pub action_id: u8,

    /// Action này có đưa vault VÀO một vị thế mới không.
    ///
    /// Governance đặt cờ này lúc đăng ký capability. Vault không suy ra được từ
    /// `action_id` (nó không hiểu ngữ nghĩa), nên phải được nói cho biết.
    ///
    /// Bất đối xứng có chủ đích:
    ///
    ///   is_entry = true  (buy, lend, stake)      -> pause CHẶN được
    ///   is_entry = false (sell, unlend, unstake) -> pause KHÔNG BAO GIỜ chặn
    ///
    /// Vì sao cửa ra không bao giờ đóng: nếu vault đã lend ETH vào Kamino rồi ai đó
    /// pause protocol hoặc tắt capability đó, mà Unlend cũng bị chặn theo, thì tiền
    /// KẸT VĨNH VIỄN trong Kamino. Cùng triết lý với "redeem không bao giờ bị pause".
    pub is_entry: bool,

    /// Trên asset nào (mint của asset đầu vào).
    pub mint: Pubkey,

    /// Account riêng của protocol cho đúng cặp (protocol, asset) này.
    ///
    ///   lend  × kamino × ETH -> reserve account của ETH trên Kamino
    ///   stake × jito   × SOL -> stake pool của Jito
    ///   buy/sell × jupiter   -> Pubkey::default() (Jupiter tự route, không cần)
    ///
    /// Đây là lý do Capability phải là account: chỗ để cất địa chỉ này.
    pub venue: Pubkey,

    /// Mint của receipt token mà action này sinh ra (kToken, jitoSOL).
    ///
    /// `Pubkey::default()` = action này không sinh receipt (swap). Governance đặt
    /// giá trị này lúc đăng ký — vault không suy ra được từ `action_id`.
    ///
    /// # Đây là kiểm tra quan trọng nhất của toàn bộ thiết kế Capability
    ///
    /// Receipt mint BẮT BUỘC đã được đăng ký làm Asset của vault. Không có kiểm
    /// tra đó thì: vault lend 1M USDC vào Kamino, nhận về kToken mà NAV KHÔNG
    /// NHÌN THẤY. NAV tụt đúng 1M. Vault không "mất" tiền — nó chỉ mù. Nhưng hậu
    /// quả giống hệt: người deposit ngay sau đó mint share với giá rẻ giả tạo, và
    /// mọi người đang giữ share bị pha loãng đúng 1M USDC.
    ///
    /// Một lệnh lend hoàn toàn hợp lệ về mặt CPI vẫn rút ruột được vault, chỉ vì
    /// kế toán không nhìn thấy tài sản.
    pub receipt_mint: Pubkey,

    /// Trần cho MỖI lần execute, tính bằng đơn vị kế toán (USDC, 6 decimals).
    /// 0 = không giới hạn.
    ///
    /// Chặn một giao dịch đơn lẻ quét sạch vault. Kiểm tra số dư sau CPI chỉ bắt
    /// được "adapter tiêu quá amount_in" — nó không bắt được "amount_in chính là
    /// toàn bộ vault". Nếu adapter Kamino có bug (hoặc Kamino bị hack), thiệt hại
    /// tối đa bị chặn bởi con số này thay vì bằng toàn bộ NAV.
    pub max_notional: u64,

    /// Thời điểm capability có hiệu lực (sau timelock 7 ngày).
    ///
    /// Thêm capability = mở một đường tiền MỚI ra khỏi vault, nguy hiểm ngang việc
    /// thêm adapter, nên chịu cùng timelock. Gỡ thì có hiệu lực ngay.
    pub active_at: i64,

    pub enabled: bool,
    pub bump: u8,
}

/// Một adapter được phép điều khiển tiền của vault.
///
/// Đây là account nguy hiểm nhất của protocol: adapter được vault cấp quyền ký
/// PDA để di chuyển tài sản. Adapter độc hại (hoặc chỉ cần có bug) là mất sạch
/// vault. Nên có hai lớp bảo vệ:
///
///   1. Thêm adapter phải qua governance vote + timelock 7 NGÀY — dài hơn hẳn
///      timelock giao dịch thường (24h), để ai thấy adapter đáng ngờ còn kịp
///      redeem thoát ra trước khi nó có hiệu lực.
///   2. Vault KHÔNG tin adapter: sau mỗi CPI, vault tự đo lại số dư và bắt
///      buộc kết quả phải đúng cam kết (xem `execute` ở Tầng 4).
///
/// Gỡ adapter thì admin làm được ngay lập tức, không cần chờ — chỉ gỡ, không
/// thêm. Dừng chảy máu phải nhanh; mở cửa mới thì phải chậm.
/// Hai loại adapter, và chúng có MỨC TIN CẬY KHÁC NHAU VỀ BẢN CHẤT.
///
/// Đây là phân biệt quan trọng nhất trong toàn bộ kiến trúc adapter. Gộp chung hai
/// loại này lại là nguồn gốc của mọi tranh cãi "vault có nên tin adapter không" —
/// câu trả lời khác nhau tuỳ loại.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum AdapterKind {
    /// **UNTRUSTED.** Thực thi giao dịch (swap, lend, stake).
    ///
    /// Vault KIỂM CHỨNG ĐƯỢC kết quả của nó bằng value-delta: đo giá trị vault
    /// trước và sau CPI, bắt buộc `value_after >= value_before × (1 − slippage)`.
    /// Adapter có bug, có ý đồ xấu, hay protocol đích bị tấn công — đều bị revert.
    ///
    /// Vì kiểm chứng được nên KHÔNG cần tin. Upgrade thoải mái. (Dù vậy vẫn nên set
    /// immutable để đóng luôn cả đường upgrade authority.)
    Action,

    /// **TRUSTED.** Định giá một position (kToken, LST) → feed thẳng vào NAV.
    ///
    /// Vault KHÔNG kiểm chứng được. Với định giá, không có phép "đo số dư" nào —
    /// con số adapter trả về CHÍNH LÀ sự thật mà vault dùng để mint share.
    ///
    /// Adapter này báo sai (dù chỉ vì bug) → NAV sai → mint share sai → mất vault.
    /// Nên nó phải được audit và khoá NGHIÊM NGẶT NHƯ CORE, không phải như một
    /// plugin. Set immutable là bắt buộc, không phải khuyến nghị.
    Pricing,
}

/// Một adapter được phép điều khiển hoặc định giá tài sản của vault.
///
/// Mỗi adapter = **một program riêng, immutable, một action**. KHÔNG phải một program
/// to gom mọi thứ rồi upgrade dần.
///
/// # Vì sao: thêm action mới không được đụng vào code đang chạy
///
/// ```text
/// KHÔNG NÊN                        NÊN
/// ─────────                        ───
/// 1 adapter "đa năng"              mỗi action = 1 adapter riêng, deploy 1 lần
/// thêm lending -> upgrade adapter  thêm lending -> deploy adapter MỚI
///   -> code swap cũ có rủi ro        -> swap adapter không bị đụng
///   -> 1 upgrade lỗi = hỏng hết      -> adapter mới lỗi, chỉ nó bị revert
/// upgrade authority = attack surface  adapter set immutable (upgrade = None)
/// ```
///
/// Vòng đời khi thêm staking:
///
///   1. viết & audit Staking adapter (program mới, độc lập)
///   2. deploy → program id mới
///   3. governance vote thêm id vào registry  ← chỉ là GHI MỘT DÒNG DATA
///   4. (nếu ra asset mới) thêm Pricing adapter tương ứng
///
///   → swap adapter, lending adapter, VAULT PROGRAM: không chạm.
///
/// Cái "liên tục thay đổi" chỉ là **registry** — data trong state, không phải code.
/// Bản thân các program đứng yên. Cũ không rủi ro vì mới; mới lỗi cũng chỉ tự revert.
///
/// Gỡ adapter lỗi = governance xoá khỏi registry, có hiệu lực NGAY (vì là data).
#[account]
#[derive(InitSpace)]
pub struct Adapter {
    pub vault: Pubkey,
    /// Program id của adapter. Vault chỉ CPI tới đúng địa chỉ này.
    pub program_id: Pubkey,

    /// Action (untrusted, verify được) hay Pricing (trusted, không verify được).
    /// Xem `AdapterKind` — hai loại này KHÔNG được lẫn lộn.
    pub kind: AdapterKind,

    /// Tên để hiển thị: "jupiter", "kamino", "jito"...
    #[max_len(32)]
    pub label: String,
    /// Bật/tắt. Tắt thì không execute qua adapter này được nữa.
    pub enabled: bool,
    /// Thời điểm adapter có hiệu lực (sau timelock). Trước mốc này không dùng được.
    pub active_at: i64,
    pub bump: u8,
}

/// Phiếu nhận hàng của một lượt redeem in-kind.
///
/// Vault giữ tới 24 asset nên không thể trả hết trong một transaction (vượt
/// giới hạn account/compute của Solana). Nên redeem chia làm ba bước:
///
///   redeem_request  -> burn share, chốt số lượng phải trả cho từng asset
///   claim_asset     -> gọi nhiều lần, mỗi lần rút một asset
///   close_ticket    -> đóng phiếu, hoàn rent, sau khi đã claim hết
///
/// Giữa các bước tồn tại trạng thái dở dang: share đã burn nhưng tài sản chưa
/// về hết. Quyền đòi tài sản nằm ở account này, nên nó phải bất biến sau khi
/// tạo: số lượng đã chốt cứng, không phụ thuộc NAV về sau. Giá có biến động thế
/// nào thì người redeem vẫn nhận đúng phần tài sản của mình tại thời điểm burn.
#[account]
#[derive(InitSpace)]
pub struct RedeemTicket {
    pub vault: Pubkey,
    pub owner: Pubkey,
    /// Id duy nhất, lấy từ Vault::redeem_ticket_counter.
    pub ticket_id: u64,
    /// Số share đã burn để tạo phiếu này.
    pub shares_burned: u64,

    /// Số lượng phải trả cho từng asset, chỉ số khớp với Vault::asset_mints.
    #[max_len(MAX_ASSETS)]
    pub amounts: Vec<u64>,
    /// Ảnh chụp mint của từng asset lúc tạo phiếu. Nếu admin thêm/bớt asset
    /// giữa chừng, phiếu vẫn trả đúng những gì đã hứa.
    #[max_len(MAX_ASSETS)]
    pub asset_mints: Vec<Pubkey>,

    /// Bitmap: bit thứ i bật nghĩa là asset i đã được claim.
    /// Đây là thứ chặn double-claim — chốt chặn quan trọng nhất của redeem.
    pub claimed_mask: u32,
    /// Số asset còn phải trả. Về 0 thì đóng được phiếu.
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
