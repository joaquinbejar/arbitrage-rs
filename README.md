# arbitrage-rs

Arbitrage models for spot, futures, perpetuals and options: pricing relations, opportunity detection and multi-leg execution.

Part of the Xynthex market making lab; usable standalone.

Status: pre-release (`0.1.0`, not published). License: MIT.

## Current surface

- Exact monetary types: `Currency`, `Money`, `Cost`, `FeeRate`, `Price`, `Quantity`.
- `exact`: `Decimal` arithmetic that fails with `ArbitrageError::Inexact` or
  `ArbitrageError::Overflow` instead of rounding silently.
- `ArbitrageError`, the crate's typed error.

The arithmetic, unit and sign conventions and the dependency direction are documented in the crate docs (`cargo doc --open`).

## Dependency direction

arbitrage-rs depends only on `rust_decimal` and `thiserror`. It does not depend on market-maker-rs, margin-rs, tessera-rl or any simulator; adapters live on their side.

## Development

```sh
make pre-push   # fmt check, clippy -D warnings, tests, docs with -D warnings, MSRV check
```

MSRV: Rust 1.85 (edition 2024).
