# Security Policy

## ⚠️ Trạng thái: CHƯA AUDIT

Code trong repo này **chưa qua audit chuyên nghiệp**.

Test xanh chỉ chứng minh code làm đúng những gì **đã được nghĩ ra để kiểm tra**. Nó
**không** chứng minh không có lỗ hổng chưa ai nghĩ tới.

**Đừng đưa tiền thật vào trước khi có audit.**

## Những thứ đang hở (biết trước, chưa đóng)

| | |
|---|---|
| **Upgrade authority** chưa khoá về governance | *Backdoor thật sự — hở nó thì mọi guardrail khác vô nghĩa* |
| **Governance chưa có** — `admin` là một keypair nắm toàn quyền | whitelist adapter/asset, pause, đổi executor |
| **Pricing adapter chưa có** | NAV gặp lend/stake position sẽ fail thẳng (cố ý) |

`mock-dex` là program **test**, cho phép rút token tuỳ ý theo thiết kế.
**Không bao giờ deploy nó.**

## Báo lỗi bảo mật

Tìm thấy lỗ hổng? **Đừng mở public issue.**

Dùng [GitHub Security Advisory](../../security/advisories/new) để báo riêng.

Xin nêu rõ:
- Đường tấn công cụ thể (ai làm gì, theo thứ tự nào)
- Tác động (mất tiền? đóng băng? pha loãng?)
- Nếu có thể: một test tái hiện được
