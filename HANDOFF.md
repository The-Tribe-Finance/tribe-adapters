# The Tribe — Handoff & Continuation Guide

A single-file overview so another AI/engineer can continue the work. Written
2026-07-15. All code comments and git commits in this project are **English**.

---

## 1. What this project is

**The Tribe** — a community-managed investment fund (DAO vault) on Solana, built
with Anchor 0.31.1. A user deposits USDC, receives freely-transferable **share
tokens**, governance proposes trades, and the vault executes them through
**adapters** (Jupiter for swaps today; lending/staking later). Redemption is
**in-kind, pro-rata, and never blocked**.

Split across public GitHub repos under **The-Tribe-Finance**:

| Repo | Role |
|---|---|
| `tribe-vault` | Core vault: custody, NAV, shares, deposit/redeem, execute |
| `tribe-adapters` | Protocol integrations (adapter-swap → Jupiter; test-adapter) |
| `tribe-governance` | Proposals/voting (design stage, no code) |
| `tribe-web` | React + Vite frontend |

This working monorepo is at `/Users/mac/tribe-contracts` (contains all programs +
tests). The public repos are split-out copies.

### Design principle (the heart of it)

The vault **does not understand what an action means**. `execute_action` forwards
an opaque payload to an adapter and verifies only the **result** via 4 agnostic
checks (value-delta ≤ slippage, no unrelated asset moved, exposure cap, reserved
untouched). This lets new actions be added by deploying a new adapter — never by
upgrading the money-holding program.

---

## 2. Program IDs (current build)

| Program | ID |
|---|---|
| tribe_vault | `7JVBNNDs9uKgYYuJ3wPqdBSjtnNgV6s3pjxZ83QMmhVs` |
| adapter_swap | `3wQCqUNGMBZL3Pe1v1iyqvonKMNMHjgByEw7iBTGHwyN` |
| test_adapter | `E88wFPQJPYv7PoeV2JbwEw76u6oiRS4m2fZSqqhaG2JA` (test-only, never deployed live) |

Vault instructions: `initialize_vault, register_asset, close_position, set_paused,
set_executor, register_capability, disable_capability, register_adapter,
disable_adapter, execute_action, assert_exposure, deposit, redeem_request,
claim_asset, close_ticket`.

---

## 3. Deployment / test environments — READ THIS FIRST

Three environments, each with a different purpose. Getting these confused wastes
hours.

- **bankrun** (`solana-bankrun`) — in-process, deterministic. Runs the unit +
  most integration tests. Can write Pyth price accounts directly and time-travel.
  **This is where you verify logic.**
- **surfpool** — a **mainnet fork** running locally (`http://127.0.0.1:8899`).
  Has real Jupiter + real mainnet token mints, so it is the ONLY place the real
  swap path runs. NOT a public network — Phantom cannot reach it; accounts vanish
  when it stops.
- **devnet** — a real public network Phantom can reach, but **Jupiter is not on
  devnet** (mainnet-only), and Pyth pull-oracle prices are not continuously
  maintained there (must be posted). Good for deposit/redeem demos, not swaps.

**Key oracle facts:**
- On bankrun/surfpool, tests write **fake but well-formed** `PriceUpdateV2`
  accounts via `tests/pyth.ts` (`encodePriceUpdate`/`healthyPrice`), owned by the
  Pyth receiver `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`. Verification level
  = **Partial** (2 bytes), publish_time at byte offset **94**.
- Oracle staleness window = **60 s** (`MAX_PRICE_AGE_SECONDS`). On surfpool the
  clock can be ~16 days ahead of wall-clock due to time-travels; set publish_time
  to the SysvarClock (offset 32), not `Date.now()`.

---

## 4. Current state — what is DONE and VERIFIED

### Core vault (Tiers 1–3) — done, tested
deposit → mint shares; in-kind redeem (redeem_request → claim_asset → close_ticket);
NAV from Pyth across every held asset; the Capability system (adapter × action ×
asset triple); `execute_action` with all 4 security guards.

### Real Jupiter swap — proven on surfpool
`tests/surfnet-user-journey.ts` runs the full flow deposit → swap (real Jupiter →
Raydium) → redeem → claim → close, and prints every tx signature.

### Whitelist-vs-Held refactor — **functionally COMPLETE (just finished)**
The big recent work. See `DESIGN-WHITELIST-VS-HELD.md`. Goal: whitelist
**thousands** of tokens but hold at most `MAX_ASSETS` positions at once (NAV only
loops over HELD positions, so only they hit the per-tx account/compute limit).

- **`register_capability` decoupled from `Asset`** — now takes `mint: Pubkey` as a
  param; whitelisting no longer requires the vault to already hold the token.
  New signature: `registerCapability(action_id, mint, is_entry, venue, max_notional)`.
- **`close_position`** — frees a held slot via **swap-remove** (moves the last
  asset into the freed slot, keeps indices dense). Only closes an empty slot
  (balance == 0 && reserved == 0). Safe for outstanding redeem tickets because
  tickets snapshot `asset_mints`/`amounts` at burn and claim against their own
  arrays, never live vault indices.
- **Lazy-open in `execute_action`** — `asset_out` is `init_if_needed`; the first
  time the vault buys a never-held token, a held slot opens (index = asset_count,
  push mint, bump count; fails `TooManyPositions` if full). New param
  `out_feed_id: [u8;32]` and new accounts (see §5). `MAX_ASSETS` is currently 40.

**Tests (all green, run each file separately — see §6):**
`vault.ts` 19, `execute.ts` 7 (all security guards), `close-position.ts` 4,
`lazy-open.ts` 2 = **32 deterministic tests passing**.

### xStocks whitelist + frontend — done on surfpool
34 official Jupiter xStocks (Backed Finance, `dev == S7vYFFWH...`) were registered;
all have Pyth equity feeds. `deploy/xstocks-whitelist.json` holds the list. The web
app has a wallet picker (Phantom/Backpack/MetaMask with real logos), a searchable
token dropdown, real xStock logos, and a real deposit path (Anchor + ALT). See the
memory notes `xstocks-whitelist.md` and `surfpool-web-integration.md`.

---

## 5. The new `execute_action` interface (IMPORTANT for callers)

Because of lazy-open, the signature and accounts changed. Any client calling
`executeAction` must use:

```ts
await vaultProgram.methods
  .executeAction(
    actionId,            // u8
    new BN(amountIn),
    Array.from(outFeedId), // [u8;32] Pyth feed id of the RECEIVED asset
    payload,             // Buffer, opaque adapter payload
  )
  .accounts({
    authority, capability, adapter, adapterProgram,
    assetIn,               // Asset PDA of the spent token (must already be held)
    assetOut,              // Asset PDA of the received token (may be fresh → opened)
    outMint,               // Mint of the received token
    vaultOutTokenAccount,  // vault_authority's ATA for outMint
    outOracle,             // Pyth price account for the received token
    tokenProgram,          // associatedTokenProgram + systemProgram auto-resolve
  })
  .remainingAccounts([...meter, ...adapterAccounts])
```

**Balance-meter (`remainingAccounts` first region) gotcha for a FRESH buy:** the
meter is `[Asset PDA, vault token account]` per held asset, in vault index order,
then the two oracles. For a slot being opened THIS instruction, do **not** put the
real Asset PDA in its meter slot (it aliases the `init_if_needed` `asset_out` and
throws `AccountDiscriminatorMismatch`). Put a placeholder (e.g. the token account)
there — the program ignores that slot for the fresh index and reads only the token
account. See `tests/lazy-open.ts` for a working example.

---

## 6. How to build & test

```bash
cd /Users/mac/tribe-contracts

anchor build                       # BPF build (required before integration tests)
cargo test --lib -p tribe-vault    # 20 unit tests (share/redeem math)

# Deterministic integration tests — RUN EACH FILE SEPARATELY.
# vault.ts and close-position.ts share the "vault" PDA seed and cross-talk if run
# in the same mocha invocation.
yarn ts-mocha -p ./tsconfig.json -t 1000000 tests/vault.ts
yarn ts-mocha -p ./tsconfig.json -t 1000000 tests/execute.ts
yarn ts-mocha -p ./tsconfig.json -t 1000000 tests/close-position.ts
yarn ts-mocha -p ./tsconfig.json -t 1000000 tests/lazy-open.ts

# Real Jupiter on a mainnet fork:
surfpool start --no-tui &
surfpool run deployment --unsupervised          # deploys the 3 programs
yarn ts-mocha -p ./tsconfig.json -t 600000 tests/surfnet-user-journey.ts
```

`anchor-lang` has the `init-if-needed` feature enabled in
`programs/tribe-vault/Cargo.toml` (needed for lazy-open; re-init is explicitly
guarded — see the "guarded re-init" comment in `execute.rs`).

---

## 7. What is PENDING (pick up here)

Ordered by how load-bearing they are.

1. **Update two surfnet test callers to the new `executeAction` signature.**
   `tests/surfnet-jupiter.ts` and `tests/surfnet-flow.ts` still call the OLD form
   (no `out_feed_id`, no `outMint/vaultOutTokenAccount/outOracle/tokenProgram`).
   `tests/surfnet-user-journey.ts` and `tests/execute.ts`/`vault.ts` are already
   updated — copy that pattern. Until this is done, those two surfpool tests fail
   to compile against the new IDL.

2. **`MAX_ASSETS` is 40; the intended cap is 50 held positions.** Bumping it is a
   one-line change in `constants.rs`, BUT it needs a fresh vault (the account is
   pre-allocated at init; the old 40-slot account can't hold 50) and a re-test of
   deposit compute with 50 assets. `claimed_mask` is `u64` (fits ≤64). Beyond 64
   would need `claimed_mask` → `[u64; N]`.

3. **Re-run the surfpool bootstrap + web flow against the new program.** The
   whitelist-vs-held changes altered `execute_action`; re-deploy to surfpool
   (`surfpool run deployment`), re-run `scripts/surfpool-bootstrap-stocks.ts`, copy
   `deploy/surfpool.json` → `tribe-web/src/chain/config.json`, and re-verify the
   web deposit. The web deposit already uses an ALT (needed for many-asset NAV).

4. **Devnet is stale.** The devnet vault (`deploy/devnet.json`) was deployed BEFORE
   the whitelist-vs-held refactor and only has USDC. To use the new code on devnet
   you must deploy under a NEW program id (the old vault account is a fixed
   singleton that can't be resized), then re-init + re-register. Note Jupiter is
   mainnet-only, so devnet proves deposit/redeem only.

5. **Governance (Tier 3) — design only, no code.** See `DESIGN-GOVERNANCE.md`:
   deploy an SPL Governance instance + a `tribe-voter-weight` plugin, then call
   `set_executor` once to hand `vault.executor` to the governance PDA. Token
   authority always stays with the vault PDA; a proposal can only trigger the
   vault's guarded instructions, never move funds directly.

6. **Pricing adapters (lend/stake) — not implemented.** `nav.rs` deliberately
   errors (`PricingKindNotSupported`) on `LendPosition`/`StakePosition`. The
   `AdapterKind::Pricing` scaffolding exists but the CPI to feed a position's value
   into NAV is not wired. Needed before lending/staking adapters can be held.

7. **Upgrade authority not locked** — intentionally deferred by the project owner.

---

## 8. Landmines (things that already cost hours)

- **`execute_action`'s `vault` account needs `#[account(mut)]`.** Lazy-open writes
  `asset_mints`/`asset_count`; without `mut` Anchor silently does NOT serialize the
  change and `asset_count` reverts on the next read. (This was the final lazy-open
  bug.)
- **Anchor + Pyth SDK cannot share a `node_modules`** (rpc-websockets/web3.js
  version clash). The devnet Pyth price-poster lives in its own isolated project
  `scripts/pyth-poster/` with shims. Do NOT add the Pyth SDK to the root project.
- **Anchor + browser (Vite):** needs a `Buffer`/`global` polyfill imported FIRST
  (`tribe-web/src/polyfills.js`), before any Solana import, or spl-token crashes
  with `Buffer is not defined`.
- **Deposit with many assets needs an Address Lookup Table + versioned tx.** 35
  assets × 3 NAV accounts = 105 remaining accounts, past a legacy tx's limit. The
  ALT must contain the **Asset PDAs** (not the mints — NAV reads the PDA).
- **Vault token accounts MUST be canonical ATAs of `vault_authority`.** Jupiter
  and every DEX derive these addresses and bake them into the instruction. A token
  account anywhere else fails DEX validation (Jupiter error `0x1789`).
- **Phantom (recent builds) cannot add a custom Solana RPC** — so it cannot reach
  surfpool localhost. Use **Backpack** for surfpool testing (it still allows custom
  RPC at `http://127.0.0.1:8899`).

---

## 9. Key files

```
programs/tribe-vault/src/
  lib.rs        instructions (deposit, redeem, register_*, close_position, ...)
  state.rs      Vault, Asset, Capability, Adapter, RedeemTicket
  execute.rs    execute_action + the 4 guards + lazy-open
  nav.rs        NAV from Pyth; available_balances(_with_fresh); value_of/value_from_balance
  math.rs       share & redemption arithmetic
  oracle.rs     Pyth price validation (feed, staleness, confidence)
  constants.rs  MAX_ASSETS, timelocks, decimals
  errors.rs

tests/
  vault.ts, execute.ts, close-position.ts, lazy-open.ts   deterministic (bankrun)
  surfnet-*.ts                                            surfpool (mainnet fork)
  pyth.ts, spl.ts                                          helpers

DESIGN-WHITELIST-VS-HELD.md   the whitelist/held architecture (current work)
DESIGN-CACHED-NAV.md          a rejected alt (cached NAV / crank) — for context
DESIGN-GOVERNANCE.md          Tier-3 governance design
ARCHITECTURE.md, SPEC.md      broader system docs

deploy/xstocks-whitelist.json   34 xStocks (symbol, mint, decimals, feedId, icon)
deploy/surfpool.json            surfpool addresses (regenerate via bootstrap)
deploy/devnet.json              STALE devnet addresses (pre-refactor)

scripts/surfpool-bootstrap-stocks.ts   init vault + register all xStocks on surfpool
scripts/pyth-poster/                   isolated Pyth price-poster (devnet)
```

---

## 10. Working conventions

- **English** for all code comments and git commits.
- Commit messages end with the `Co-Authored-By: Claude ...` trailer.
- Do NOT push internal `DESIGN-*.md`/`SPEC.md`/`ARCHITECTURE.md` to the public
  repos (they're gitignored there); READMEs are the public-facing docs.
- Run integration test files **separately** (shared "vault" PDA seed).
- Treat `execute_action` and `deposit`/`redeem` as the highest-risk code: they
  hold real money. Re-run `execute.ts` (the 7 security tests) after any change to
  them.
