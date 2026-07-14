# tribe-adapters

Protocol integrations for [The Tribe](https://github.com/The-Tribe-Finance) — a
community-managed investment fund on Solana.

An adapter is how the vault talks to an outside protocol. Each one is a **separate,
immutable program handling a single action**. Adding a new capability means deploying a
new adapter, not modifying an existing one.

> ⚠️ **Not audited. Do not deposit real funds.**
> See [SECURITY.md](./SECURITY.md) for current status.

---

## The model

```
tribe-vault  ──►  adapter  ──►  Jupiter / Kamino / Jito / …
(verifies)        (knows the protocol)
```

The vault signs with its PDA and forwards that authority into the adapter for the duration
of a single transaction. The adapter never holds the vault's tokens — funds move from the
vault, through the protocol, and back, all within one instruction.

Adding staking, for example, touches no code that is already running:

1. Write and audit a staking adapter — an independent program
2. Deploy it
3. Governance adds its program id to the vault's registry

The swap adapter and the vault program are untouched. What changes over time is the
registry — data, not code. A faulty adapter is removed by deleting its entry, which takes
effect immediately.

---

## Two kinds of adapter

The distinction is about **what the vault can verify**.

### Action adapters

Execute trades. The vault checks the result against its own oracles and balances: value
must not drop beyond the allowed slippage, no unrelated asset may move, exposure limits
must hold.

Because the outcome is verifiable, the adapter does not need to be trusted. A bug, a
compromised route, or a misbehaving venue all produce a result the vault rejects.

### Pricing adapters

Value a position that has no price feed — a lending receipt token, a liquid staking token.
The number they return feeds directly into NAV.

There is nothing to verify it against. A pricing adapter is **trusted**, and must be
audited and locked down accordingly. The vault refuses to execute trades through one.

---

## Adapters

| Program | Kind | Status |
|---|---|---|
| `adapter-swap` | Action | Working; exercised against Jupiter on a mainnet fork |
| `adapter-lend` | Action | Planned |
| `pricing-lst` | Pricing | Planned |
| `test-adapter` | Test fixture | Never deployed |

---

## Why `min_out` belongs here

Slippage bounds are specific to swaps. Lending has no `min_out`; staking is different
again.

If the vault computed `min_out`, it would have to understand swaps — and adding lending
would mean upgrading the program that holds the assets. So the vault only enforces checks
that are true of *any* action, and everything protocol-specific lives in the adapter.

---

## Building and testing

```bash
anchor build
yarn test          # integration tests against a test adapter (bankrun)
```

Against the real Jupiter, on a mainnet fork:

```bash
surfpool start --no-tui &
surfpool run deployment --unsupervised
npx ts-mocha -p ./tsconfig.json -t 600000 tests/surfnet-jupiter.ts
```

This repository vendors `tribe-vault` for testing only — an adapter must prove itself
*through* the vault, with every guard in place. The canonical source is
[`tribe-vault`](https://github.com/The-Tribe-Finance/tribe-vault).

`test-adapter` is a test fixture that can be instructed to misbehave, so that the vault's
checks can be shown to catch it.

---

## Writing an adapter

1. One program, one action.
2. Take the vault's authority PDA as a signer. It is valid only within the current
   transaction.
3. When forwarding accounts to the target protocol, only that PDA may sign.
4. Never take custody of the vault's tokens.
5. Choose an unused action id. The vault treats it as an opaque number; the meaning is the
   adapter's convention.
6. Set the program immutable after deployment.

---

## Repositories

| | |
|---|---|
| [tribe-vault](https://github.com/The-Tribe-Finance/tribe-vault) | Asset custody, NAV, shares |
| **tribe-adapters** | Protocol integrations |
| [tribe-governance](https://github.com/The-Tribe-Finance/tribe-governance) | Proposals, voting, timelocks |
| [tribe-web](https://github.com/The-Tribe-Finance/tribe-web) | Frontend |

---

## License

Apache-2.0
