# tribe-adapters

> **Tương tác với protocol ngoài.** Mỗi action = một program riêng, immutable.

Adapter của [The Tribe](https://github.com/The-Tribe-Finance) — quỹ đầu tư chung do
cộng đồng quản lý trên Solana.

---

## 🚨 CHƯA AUDIT — KHÔNG ĐƯA TIỀN THẬT VÀO

`mock-dex` là **program test**, cho phép rút token tuỳ ý theo thiết kế.
**Tuyệt đối không deploy nó.**

---

## Nguyên tắc: cắm thêm, không vá

**Một program. Một action. Deploy một lần. Set immutable.**

```
KHÔNG NÊN                          NÊN
─────────                          ───
1 adapter "đa năng"                mỗi action = 1 adapter riêng
thêm lending → upgrade adapter     thêm lending → deploy adapter MỚI
  → code swap cũ có rủi ro           → swap adapter không bị đụng
  → 1 upgrade lỗi = hỏng hết         → adapter mới lỗi, chỉ nó bị revert
upgrade authority = attack surface  set immutable (upgrade = None)
```

Vòng đời khi thêm staking — **không đụng vào bất kỳ code đang chạy nào**:

```
1. viết & audit Staking adapter (program mới, độc lập)
2. deploy → program id mới
3. governance vote thêm id vào registry của vault   ← chỉ GHI MỘT DÒNG DATA
4. (nếu ra asset mới) thêm Pricing adapter tương ứng

→ adapter-swap, vault program: KHÔNG CHẠM
```

Cái "liên tục thay đổi" chỉ là **registry** — data trong state của vault, không phải
code. Bản thân các program đứng yên. Cũ không rủi ro vì mới; mới lỗi cũng chỉ tự
revert.

Gỡ adapter lỗi = governance xoá khỏi registry, **có hiệu lực ngay** (vì là data).

---

## Hai loại adapter — mức tin cậy KHÁC NHAU VỀ BẢN CHẤT

Đây là phân biệt quan trọng nhất trong toàn bộ kiến trúc. Gộp chung hai loại này là
nguồn gốc của mọi nhầm lẫn "vault có nên tin adapter không" — câu trả lời **khác nhau
tuỳ loại**.

### `Action` — UNTRUSTED ✅ verify được

Thực thi giao dịch (swap, lend, stake). Vault **kiểm chứng được** kết quả bằng
NAV-delta: đo giá trị vault trước và sau CPI, bắt buộc

```
NAV_sau ≥ NAV_trước − value_in × max_slippage
```

Adapter có bug, có ý đồ xấu, hay protocol đích bị tấn công — **tất cả đều bị revert**.

Chính vì kiểm chứng được nên vault **không cần tin**. Adapter được phép làm việc "khó"
(tính `min_out`, hiểu layout của Jupiter, dựng route) mà không cần audit ở mức core:
sai thì vault bắt.

### `Pricing` — TRUSTED ⚠️ KHÔNG verify được

Định giá một position (kToken, LST) → **feed thẳng vào NAV**.

Vault **không có gì để đo**. Con số adapter trả về *chính là* sự thật mà vault dùng để
mint share. Adapter này báo sai — dù chỉ vì một bug chia nhầm — thì NAV sai, mint share
sai, **mất vault**.

Nên nó phải được audit và khoá **nghiêm ngặt như core**, không phải như một plugin.
Set immutable là **bắt buộc**, không phải khuyến nghị.

> Vault từ chối dùng Pricing adapter để execute (`WrongAdapterKind`). Cho nó quyền di
> chuyển tiền là gộp hai mức tin cậy làm một, và mất luôn lý do khiến Action adapter
> an toàn.

---

## Adapter hiện có

| Program | Loại | action_id | Trạng thái |
|---|---|---|---|
| `adapter-swap` | Action | `0` | ✅ có test |
| `adapter-lend` | Action | — | 🔜 |
| `pricing-lst` | Pricing | — | 🔜 |
| `mock-dex` | 🧪 test | — | **không deploy** |

---

## Vì sao `min_out` nằm ở ADAPTER, không ở vault

`min_out` là khái niệm **riêng của swap**. Lend không có `min_out` — nó có
`min_receipt`. Staking cũng vậy.

Nếu vault tính `min_out`, vault phải *hiểu* swap — và thêm lending sẽ phải **upgrade
program đang giữ tiền**.

Nên phân chia:

| | |
|---|---|
| **Adapter** | Logic riêng của từng action. Tính `min_out`, hiểu layout của DEX, dựng route. |
| **Vault** | Chỉ kiểm chứng thứ **agnostic**: giá trị không giảm, exposure trong trần, `reserved` còn nguyên. |

---

## Chuỗi CPI

```
tribe-vault  ──►  adapter-swap  ──►  Jupiter / Orca / …
(kiểm chứng)      (biết swap)        (DEX thật)
```

Vault ký bằng PDA của nó (`vault_authority`) và **chuyển tiếp chữ ký xuống** adapter.
Adapter chỉ **mượn** chữ ký trong đúng transaction hiện tại — nó không giữ được, không
dùng lại được, và **không bao giờ sở hữu token của vault**.

Cả vault lẫn adapter đều áp cùng một nguyên tắc khi forward account list:

> **Chỉ `vault_authority` được ký.** Mọi account khác luôn `is_signer = false`, bất kể
> client khai báo gì. Không có kiểm tra này thì client đưa vào một account bất kỳ kèm
> cờ signer, và vault ký thay cho nó — tức là cho mượn quyền lực của mình để làm bất
> cứ điều gì, ở bất cứ đâu.

---

## Chạy

```bash
anchor build
yarn test        # 7 test — chuỗi đầy đủ vault → adapter → dex
```

Repo này chứa cả `tribe-vault` **chỉ để test** — adapter phải chứng minh nó hoạt động
*qua* vault, với đầy đủ guardrail thật. Nguồn chính thức của vault là
[`tribe-vault`](https://github.com/The-Tribe-Finance/tribe-vault).

`mock-dex` **cố ý gian lận được** theo lệnh — tiêu quá phần được phép, trả về ít hơn
cam kết, lấy tiền không trả gì. Mỗi test là một đòn tấn công cụ thể, và vault phải bắt
được tất cả.

---

## Viết một adapter mới

1. **Một program, một action.** Đừng gom nhiều action vào một program.
2. Nhận `vault_authority` làm `Signer` — chữ ký do vault cấp, chỉ dùng được trong
   transaction hiện tại.
3. Khi forward account xuống protocol đích: **chỉ `vault_authority` được ký**.
4. Đừng bao giờ để adapter **sở hữu** token của vault. Tiền đi thẳng vault → protocol
   → về lại vault, trong cùng một transaction.
5. Chọn một `action_id` chưa dùng. Vault không hiểu con số này — nó chỉ dùng làm seed
   của `Capability`. Ngữ nghĩa là quy ước của adapter.
6. **Set immutable sau khi deploy** (`solana program set-upgrade-authority --final`).

---

## Repo liên quan

| Repo | Vai trò |
|---|---|
| [tribe-vault](https://github.com/The-Tribe-Finance/tribe-vault) | Giữ tiền. Ít upgrade nhất có thể. |
| **tribe-adapters** *(đây)* | Tương tác protocol ngoài. |
| [tribe-governance](https://github.com/The-Tribe-Finance/tribe-governance) | Proposal, vote, timelock. |
| [tribe-web](https://github.com/The-Tribe-Finance/tribe-web) | Frontend. |

---

## License

Apache-2.0
