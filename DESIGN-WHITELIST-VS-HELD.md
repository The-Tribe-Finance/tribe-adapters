# Whitelist (thousands) vs Held positions (max 50)

## Two different concepts, deliberately separated

| | Whitelist | Held position |
|---|---|---|
| Meaning | "the vault is ALLOWED to buy this token" | "the vault currently OWNS a balance of this token" |
| On-chain | `Capability` PDA per (adapter × action × mint) | `Asset` PDA, only while balance > 0 |
| Count | thousands — only costs rent | **max 50** — deposit prices every one |
| Cost driver | account rent (paid once at registration) | per-transaction compute + accounts on deposit |

The per-transaction limit only ever touches **held** positions, because only held
positions have value and thus enter NAV. Whitelisting a token the vault does not
hold costs nothing at deposit time.

## Why NAV stays simple (no crank needed)

With held capped at 50, NAV over all held positions is ≤ 50 oracle reads —
which fits ONE transaction with an Address Lookup Table (50 × 3 = 150 accounts,
~185k CU, well under the 1.4M limit). So the current "deposit computes NAV
directly" architecture is kept unchanged. No cached NAV, no crank, no keeper.

The scaling win comes entirely from decoupling whitelist from held, not from
changing how NAV is computed.

## What changes vs today

Today, to trade an asset it must already be a registered `Asset` (a held slot).
That conflates the two. The change:

1. **Whitelist by mint, unbounded.** `register_capability` already keys on a
   mint. Keep registering thousands of these — they are the whitelist. Rent only.

2. **Held `Asset` slots are lazily managed and capped at `MAX_HELD` (50).**
   - When `execute_action` BUYS a token the vault does not yet hold, it opens an
     `Asset` slot for it (if a free slot exists; else the action fails with
     `TooManyPositions` — governance must first exit a position).
   - When a position's balance returns to 0 (fully sold), its `Asset` slot is
     closed and freed, so the vault can open a different one.
   - Deposit / redeem loop only over the ≤50 open slots.

3. **`MAX_ASSETS` → `MAX_HELD = 50`.** The array that deposit/redeem iterate.
   `claimed_mask` stays `u64` (50 ≤ 64 bits).

## The held-slot lifecycle

```
whitelist (Capability):   thousands of allowed (adapter, action, mint) triples
                          │
       governance proposes buy of mint X (X is whitelisted)
                          ▼
execute_action(buy X):    is there a free held slot?  (held_count < 50)
                          ├─ yes → open Asset slot for X, receive tokens
                          └─ no  → fail TooManyPositions
                          ▼
       ... vault now holds X; deposit/redeem include X in NAV / pro-rata ...
                          ▼
execute_action(sell all X): balance(X) → 0
                          ▼
close_position(X):        free the Asset slot; held_count -= 1
```

## Safety notes

- **Deposit invariant unchanged.** It still requires every HELD asset's fresh
  oracle (≤50). A whitelisted-but-not-held token is never in NAV, so it cannot
  affect share price and needs no oracle at deposit.
- **Redemption unchanged.** Pro-rata over the ≤50 held slots, oracle-free, never
  blocked. `claimed_mask: u64` covers 50.
- **No lazy-open griefing.** Only `execute_action` (governance-gated) opens
  slots, so a random user cannot fill the 50 slots. Governance manages the held
  set deliberately.
- **Receipt tokens (kToken/LST) count as held positions** and consume a slot,
  same as any asset — consistent with today.

## Index scheme when closing a held slot (decided)

Held assets keep DENSE indices `0..held_count`, so every `for i in 0..count`
loop and `asset.index == i` check stays valid. To close the slot at index `k`:

**Swap-remove.** Move the last held asset (index `count-1`) into slot `k`:
- `vault.asset_mints[k] = vault.asset_mints[count-1]; asset_mints.pop()`
- the moved `Asset` account gets `asset.index = k`
- `vault.asset_count -= 1`
- the closed `Asset` account is closed (rent refunded)

**Why this is safe for outstanding redeem tickets:** a `RedeemTicket` snapshots
`asset_mints.clone()` and `amounts` at burn time, and `claim_asset` indexes into
the TICKET's own arrays (`ticket.asset_mints[idx]`, `ticket.amounts[idx]`) and
its own `claimed_mask` — never the live vault index. So reordering vault slots
after a ticket exists cannot corrupt it. Verified in claim_asset.

This avoids a tombstone free-list entirely — no sparse indices, no change to the
NAV/redeem loops beyond the swap itself. A slot can only be closed when its
balance (and reserved) are both zero, so no money is ever stranded by the move.

## Migration

Breaking layout change (Vault gains `max_held`/`held_count`; Asset slot
lifecycle). Fresh vault (new program id or wiped surfpool). Whitelist is now
just capabilities; assets are opened by execution.

## Practical guidance

Whitelist as many tokens as you like (all xStocks + every liquid Jupiter token).
The vault trades among them but never holds more than 50 distinct positions at
once — a normal, healthy constraint for a fund (concentration, not breadth).
