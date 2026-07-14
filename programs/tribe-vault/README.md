# tribe-vault

**Program giữ tiền của The Tribe.** Nhận deposit, trả redeem, đếm share, tính
NAV, swap tài sản qua adapter.

Program này cố tình làm **ít việc nhất có thể**. Governance và execution nằm ở
program khác, để chỗ giữ tiền càng ít phải upgrade càng tốt — mỗi lần upgrade là
một lần có thể cài lỗi vào nơi nguy hiểm nhất.

---

## Bốn bất biến không bao giờ được phá

Nếu bạn sửa code trong repo này, đây là những điều phải giữ nguyên. Mỗi cái chặn
một đòn tấn công cụ thể.

### 1. NAV luôn tính từ TOÀN BỘ asset

`compute_nav` bắt buộc nhận đủ bộ ba `[Asset, token account, oracle]` cho **mọi**
asset đã đăng ký. Thiếu một cái là fail.

> **Vì sao:** nếu người gọi được phép bỏ bớt asset, họ sẽ bỏ đúng những asset
> đắt tiền để dìm NAV xuống, deposit vào lúc share đang "rẻ giả tạo", rồi ăn
> phần chênh lệch của tất cả những người còn lại.

### 2. Deposit tính NAV TRƯỚC khi tiền vào vault

Thứ tự trong `deposit()` là bắt buộc, không phải phong cách.

> **Vì sao:** tính NAV sau khi token đã chuyển vào thì tiền của người deposit đã
> nằm trong NAV — họ được mint share dựa trên chính khoản vừa gửi. Tự pha loãng
> chính mình và mọi người, và ai cũng khai thác được.

### 3. Làm tròn luôn nghiêng về vault

Mọi phép chia làm tròn **xuống**. Deposit → share làm tròn xuống. Redeem → tài
sản làm tròn xuống. Phần lẻ **ở lại vault**, thuộc về những người còn nắm share.

> **Vì sao:** để phần lẻ chảy ra ngoài là mở đường rút ruột vault bằng cách lặp
> lại hàng triệu lệnh nhỏ để gom sai số. Test `splitting_redeem_never_extracts_more`
> khóa chặt bất biến này.

### 4. Redeem KHÔNG BAO GIỜ bị pause

`RedeemRequest` **không** kiểm tra cờ `paused`. Có chủ đích.

> **Vì sao:** pause tồn tại để cứu protocol khỏi lỗi, không phải để nhốt tiền
> người dùng. Kể cả khi mọi thứ đóng băng, ai cũng phải rút được tiền ra.

---

## Instruction

### Quản trị (admin)

| Instruction | Việc |
|---|---|
| `initialize_vault` | Khởi tạo. Gọi một lần duy nhất. |
| `register_asset` | Thêm token vào whitelist (kèm Pyth feed). |
| `set_paused` | Dừng deposit + execute. **Không** dừng redeem. |
| `register_adapter` | Thêm adapter. **Timelock 7 ngày** mới dùng được. |
| `disable_adapter` | Tắt adapter. **Có hiệu lực ngay**, không timelock. |

> Bất đối xứng có chủ đích: **mở cửa thì chậm** (7 ngày để cộng đồng soi và
> thoát nếu adapter đáng ngờ), **đóng cửa thì tức thì** (phát hiện adapter hỏng
> thì cầm máu phải nhanh).

### Người dùng

| Instruction | Việc |
|---|---|
| `deposit` | Gửi token → nhận share. |
| `redeem_request` | Burn share → nhận "phiếu nhận hàng". |
| `claim_asset` | Rút một asset từ phiếu. Gọi lặp lại. |
| `close_ticket` | Đóng phiếu đã claim hết, hoàn rent. |

### Execution

| Instruction | Việc |
|---|---|
| `execute_swap` | Swap tài sản vault qua adapter (MVP: Jupiter). |

**Hiện tại quyền gọi là `admin`.** Tầng 3 sẽ thay đúng một chỗ đó thành
"proposal đã pass + hết timelock 24h".

---

## Redeem in-kind: vì sao phải 3 bước

Vault giữ tới **24 asset**. Không thể trả hết trong một transaction — vượt giới
hạn account và compute của Solana.

```
redeem_request  →  burn share, chốt cứng số lượng phải trả cho từng asset
      ↓
claim_asset     →  gọi nhiều lần, mỗi lần rút một asset
      ↓
close_ticket    →  đóng phiếu, hoàn rent
```

Giữa các bước tồn tại **trạng thái dở dang**: share đã burn nhưng tài sản chưa
về hết. Đây là phần nguy hiểm nhất của cả program. Hai lớp bảo vệ:

- **`RedeemTicket` bất biến sau khi tạo.** Số lượng chốt cứng tại thời điểm
  burn, không phụ thuộc NAV về sau. Giá có chạy thế nào thì người redeem vẫn
  nhận đúng phần của mình.
- **Bitmap `claimed_mask`** chặn double-claim. Hỏng nó thì một phiếu rút được
  cùng một asset nhiều lần.

Phiếu còn lưu **ảnh chụp** danh sách asset lúc tạo. Admin có thêm/bớt asset giữa
chừng thì phiếu vẫn trả đúng những gì đã hứa.

---

## Execute: vault KHÔNG tin adapter

Đây là instruction nguy hiểm nhất — nơi tiền rời khỏi vault. Nó CPI sang program
bên ngoài và **cấp quyền ký PDA** cho program đó di chuyển tài sản.

### Vấn đề: vault không đọc được route của Jupiter

Jupiter tính route off-chain rồi trả về instruction data dựng sẵn. Vault không
có cách nào hiểu nội dung đó — nó chỉ chuyển tiếp `payload` như một khối byte mờ
đục.

### Giải pháp: chỉ tin số dư thực tế

```
1. Đo số dư asset_in và asset_out TRƯỚC CPI
2. Gọi adapter (ký bằng PDA)
3. Đo LẠI sau CPI
4. BẮT BUỘC:  spent    ≤ amount_in
              received ≥ min_out
   Sai một li → revert cả transaction
```

Adapter có bug, có ý đồ xấu, hay bị sandwich attack — đều không rút được quá
giới hạn này. **Mọi thứ khác có thể bị lừa; số dư thì không.**

### `min_out` do VAULT tự tính, không lấy từ payload

Điểm sống còn. `min_out` = giá Pyth − **1% slippage**, do chính vault tính.

> **Nếu tin `min_out` từ payload:** kẻ tấn công đưa một route tồi kèm
> `min_out = 0`, swap 1 triệu USDC lấy về 1 lamport SOL — hoàn toàn "hợp lệ"
> theo mọi kiểm tra khác.

---

## Oracle: bốn cửa kiểm tra

`oracle::get_validated_price` là **ranh giới tin cậy** của protocol. Mọi thứ phía
sau — mint bao nhiêu share, trả bao nhiêu tài sản — đều dựa trên con số ra từ đó.
Nên thà chặn nhầm còn hơn bỏ sót:

| Kiểm tra | Chặn đòn gì |
|---|---|
| `feed_id` khớp asset | Tráo feed SOL vào chỗ BTC để làm lệch NAV |
| Giá tươi ≤ **60s** | Dùng giá cũ để mint rẻ / redeem đắt |
| Giá > 0 | Oracle hỏng trả giá âm/0 |
| Độ tin cậy ≤ **2%** giá | Thị trường loạn, thanh khoản cạn → giá không đáng tin |

---

## Cấu trúc file

```
src/
├── lib.rs         instruction + account struct
├── state.rs       Vault, Asset, AssetKind, RedeemTicket, Adapter
├── math.rs        ⚠️ toán tiền
├── nav.rs         tính NAV, bắt buộc đủ mọi asset
├── oracle.rs      xác thực giá Pyth
├── execute.rs     ⚠️ swap qua adapter
├── errors.rs
└── constants.rs
```

---

## Test

```bash
cargo test --lib      # 26 unit test toán, <1s
```

Hai test đáng chú ý nhất:

- **`splitting_redeem_never_extracts_more`** — chẻ nhỏ lệnh redeem **không** rút
  được nhiều hơn rút một lần. Khóa chặt bất biến chống bòn rút vault qua sai số
  làm tròn.
- **`min_out_never_exceeds_fair_amount`** — `min_out` không bao giờ vượt mức giá
  công bằng (vượt thì mọi swap fail, vault đóng băng) và không thấp hơn 99%
  (thấp quá thì vault bị rút ruột hợp lệ).

**Chưa có test tích hợp** — chưa chạy deposit/redeem/swap thật trên validator.
Cho tới khi có, phần CPI chưa được kiểm chứng thực tế.

---

## Hằng số

| Hằng | Giá trị | Ghi chú |
|---|---|---|
| `MAX_ASSETS` | 24 | Trần số tài sản |
| `MAX_PRICE_AGE_SECONDS` | 60 | Giá cũ hơn → fail |
| `MIN_DEPOSIT` | 1 USDC | Chặn lỗi làm tròn |
| `MINIMUM_LIQUIDITY` | 1000 | Share khóa vĩnh viễn — chống inflation attack |
| `MAX_SLIPPAGE_BPS` | 1% | Trần slippage khi swap |
| `ADAPTER_TIMELOCK_SECONDS` | 7 ngày | Adapter mới phải chờ |

### `MINIMUM_LIQUIDITY` chống gì?

Không có nó, kẻ tấn công deposit 1 unit (nhận 1 share), rồi **chuyển thẳng** một
lượng lớn token vào vault để thổi giá share lên. Người deposit sau bị làm tròn về
**0 share**, và toàn bộ tiền của họ rơi vào tay kẻ tấn công.

Khóa cứng 1000 share đầu tiên khiến đòn này không còn lãi.
