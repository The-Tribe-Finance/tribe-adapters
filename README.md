# tribe-adapters

> **Talks to outside protocols.** One program per action, immutable.

Adapters for [The Tribe](https://github.com/The-Tribe-Finance) — a community-managed
investment fund on Solana.

---

## 🚨 NOT AUDITED — DO NOT PUT REAL MONEY IN

`mock-dex` is a **test program**. It lets anyone pull tokens out at will, *by design*.
**Never deploy it.**

---

## Proven against the real Jupiter

This is not a mock. `tests/surfnet-jupiter.ts` runs the full chain on a mainnet fork:

```
tribe-vault  ──►  adapter-swap  ──►  Jupiter  ──►  Raydium
(verifies)        (knows swaps)      (real)       (real pool)
```

A real run:

| | |
|---|---|
| Spent | **1,000 USDC** (exactly `amount_in`, not a lamport more) |
| Received | **12.94 WSOL** |
| Fill price | **$77.27/SOL** — matching the oracle |
| Compute | **79,625 CU** |
| Tx size | 756 bytes (with ALTs; a legacy tx was 1237 > 1232) |

The vault verified the result and let it through. Had the fill lost more than 1% of the
value sent in, measured against the oracle, `ValueLost` would have reverted it.

### Jupiter accepts a PDA as the user

Jupiter builds its instruction for `userPublicKey = vault_authority` — a PDA — and asks
for exactly **one** signer: that same PDA.

The client strips **every** signer flag before handing the account list over. The vault
puts one back, for `vault_authority` and nothing else:

```rust
is_signer: key == authority_key
```

A client cannot smuggle a signature through by claiming some account is a signer. **The
vault decides — and it only ever decides in favor of its own PDA.**

---

## The principle: plug in, don't patch

**One program. One action. Deployed once. Set immutable.**

```
DON'T                              DO
─────                              ──
one "universal" adapter            one adapter per action
add lending → upgrade adapter      add lending → deploy a NEW adapter
  → the old swap code is at risk     → the swap adapter is untouched
  → one bad upgrade breaks all       → a bad new adapter only reverts itself
upgrade authority = attack surface   adapters can be set immutable
```

Adding staking touches **no code that is already running**:

```
1. write & audit a Staking adapter (a new, independent program)
2. deploy → new program id
3. governance votes it into the vault's registry   ← ONE ROW OF DATA
4. (if it yields a new asset) add the matching Pricing adapter

→ adapter-swap, the vault program: untouched
```

What "changes constantly" is only the **registry** — data in the vault's state, not code.
The programs themselves stand still. Old ones are not endangered by new ones; a broken new
one only reverts itself.

Removing a bad adapter = governance deletes it from the registry, effective **immediately**
(it is data, not code).

---

## Two kinds of adapter — FUNDAMENTALLY different trust levels

This is the most important distinction in the whole architecture. Conflating the two is
the source of every confused argument about "should the vault trust adapters" — the answer
depends on which kind.

### `Action` — UNTRUSTED ✅ verifiable

Executes trades (swap, lend, stake). The vault **can verify** the result via value-delta:

```
NAV_after ≥ NAV_before − value_in × max_slippage
```

A buggy adapter, a malicious one, a compromised target protocol — **all revert**.

Because it is verifiable, the vault **does not need to trust it**. The adapter is free to
do the hard work (compute `min_out`, understand Jupiter's layout, build routes) without
being audited to core standards: if it gets it wrong, the vault catches it.

### `Pricing` — TRUSTED ⚠️ NOT verifiable

Prices a position (kToken, LST) → **feeds straight into NAV**.

The vault has **nothing to measure it against**. The number it returns *is* the truth the
vault mints shares on. A wrong number — even from an innocent bug — means wrong NAV, wrong
share mints, **drained vault**.

So it must be audited and locked down **as strictly as the core**, not treated as a plugin.
Immutable is **mandatory**, not advisory.

> The vault refuses to execute through a Pricing adapter (`WrongAdapterKind`). Giving it
> the power to move money collapses two trust levels into one, and destroys the very
> reason Action adapters are safe.

---

## Adapters

| Program | Kind | action_id | Status |
|---|---|---|---|
| `adapter-swap` | Action | `0` | ✅ tested against real Jupiter |
| `adapter-lend` | Action | — | 🔜 |
| `pricing-lst` | Pricing | — | 🔜 |
| `mock-dex` | 🧪 test | — | **never deploy** |

---

## Why `min_out` lives in the ADAPTER, not the vault

`min_out` is a concept **specific to swaps**. Lending has no `min_out` — it has
`min_receipt`. Staking is different again.

If the vault computed `min_out`, the vault would have to *understand swaps* — and adding
lending would mean **upgrading the program that holds the money**.

So:

| | |
|---|---|
| **Adapter** | Everything specific to one action. Computes `min_out`, knows the DEX's layout, builds the route. |
| **Vault** | Only the **agnostic** checks: value did not drop, exposure within cap, `reserved` intact. |

---

## The CPI chain

```
tribe-vault  ──►  adapter-swap  ──►  Jupiter / Orca / …
(verifies)        (knows swaps)      (the real DEX)
```

The vault signs with its PDA (`vault_authority`) and **forwards that signature down** into
the adapter. The adapter only **borrows** it, for this one transaction — it cannot keep it,
cannot reuse it, and **never owns the vault's tokens**.

Both the vault and the adapter apply the same rule when forwarding an account list:

> **Only `vault_authority` may sign.** Every other account is `is_signer = false`,
> regardless of what the client claims. Without this, a client passes in any account with
> the signer flag set and the vault signs on its behalf — lending its authority out to do
> anything, anywhere.

---

## Running

```bash
anchor build
yarn test                                          # 7 tests against mock-dex (bankrun)

# Against the REAL Jupiter, on a mainnet fork:
surfpool start --no-tui &
NO_DNA=1 surfpool run deployment --unsupervised
npx ts-mocha -p ./tsconfig.json -t 600000 tests/surfnet-jupiter.ts
```

This repo vendors `tribe-vault` **only for testing** — an adapter has to prove it works
*through* the vault, with all the real guards in place. The canonical source is
[`tribe-vault`](https://github.com/The-Tribe-Finance/tribe-vault).

`mock-dex` **deliberately misbehaves** on command — spends more than allowed, returns less
than promised, takes money and gives nothing back. Each test is a specific attack, and the
vault has to catch every one.

### Gotchas when testing on a mainnet fork

- **The oracle must agree with the market.** The fake Pyth price is derived from Jupiter's
  own quote. Say SOL is $100 while Jupiter's pools price it at $77, and the vault sees a
  23% loss and reverts — *correctly*.
- **Oracle-based AMMs reject a time-travelled clock.** HumidiFi, Quantum, and BisonFi
  revert with `0x1771` after the clock is jumped forward to clear the 7-day timelock, even
  with slippage widened to 99%. Their pool state on the fork is identical to mainnet — they
  read the clock to price. Classic constant-product AMMs (Raydium) work fine.
- **Read the Clock SYSVAR, not the wall clock.** A fork sits at a completely different
  timestamp; `Date.now()` makes every price look decades stale.

---

## Writing a new adapter

1. **One program, one action.** Do not bundle actions into one program.
2. Take `vault_authority` as a `Signer` — the vault grants that signature, and it is only
   valid inside the current transaction.
3. When forwarding accounts down to the target protocol: **only `vault_authority` signs**.
4. **Never let the adapter own the vault's tokens.** Money goes vault → protocol → vault,
   all in one transaction.
5. Pick an unused `action_id`. The vault does not understand the number — it only uses it
   as a `Capability` seed. The meaning is the adapter's convention.
6. **Set it immutable after deploying**
   (`solana program set-upgrade-authority --final`).

---

## Related repos

| Repo | Role |
|---|---|
| [tribe-vault](https://github.com/The-Tribe-Finance/tribe-vault) | Holds the money. Upgraded as rarely as possible. |
| **tribe-adapters** *(here)* | Talks to outside protocols. |
| [tribe-governance](https://github.com/The-Tribe-Finance/tribe-governance) | Proposals, voting, timelock. |
| [tribe-web](https://github.com/The-Tribe-Finance/tribe-web) | Frontend. |

---

## License

Apache-2.0
