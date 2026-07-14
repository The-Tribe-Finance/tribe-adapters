# Security

## Status: pre-audit

This code has **not been audited**. It is not suitable for real funds.

`mock-dex` is a test fixture, designed to misbehave so that the vault's checks can be
shown to catch it. It is not a production program and must never be deployed.

## Adapter trust levels

**Action adapters** produce results the vault verifies independently. They are treated as
untrusted; a fault produces a rejected transaction rather than a loss.

**Pricing adapters** feed values directly into NAV, where there is nothing to verify them
against. They are trusted, and must be audited to the same standard as the core vault, and
deployed immutably.

## Reporting a vulnerability

Please do not open a public issue.

Report privately through [GitHub Security Advisories](../../security/advisories/new).

Include, where possible:

- The steps that lead to the issue
- The impact — loss of funds, denial of service, incorrect accounting
- A reproduction, if you have one
