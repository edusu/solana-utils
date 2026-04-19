# solana-utils

CLI utilities to keep a Solana wallet clean: close empty SPL accounts to reclaim rent, and consolidate every balance into a reference mint (typically USDC) via Jupiter, burning mints with no liquidity.

Aimed at real wallets that have accumulated airdrop leftovers, dead memecoins, dust from old swaps, and straggler token accounts.

## What it does

- **`close-accounts`**: closes SPL accounts with zero balance, reclaiming ~0.002 SOL of rent per account.
- **`swap-all`**: sells everything (except SOL and the whitelist) into a target mint using Jupiter, then cleans the drained accounts — burns the mints with no price so those accounts can also be closed.

Both commands honor a per-section whitelist in `config.toml` and support `--dry-run`.

## Architecture

Cargo workspace with three crates:

- `rpc/` — Solana-only library: `RpcClient` wrapper, SPL operations (list, close, burn), generic tx signing helper.
- `integrations/` — third-party HTTP APIs (Jupiter today): Swap v2 `/order`, Price v3, plus helpers that stitch together quote + sign + submit.
- `cli/` — `solana-utils` binary with subcommands (`clap`), loads `.env` on startup, logs via `tracing`.

Files worth knowing:

```
rpc/src/
├── spl/tokens.rs    # lists accounts from both programs (Token and Token-2022)
├── spl/close.rs     # close in batches of 20 ix/TX
├── spl/burn.rs      # burn+close pairs grouped 10 per TX
└── tx.rs            # sign_versioned_transaction (generic signing helper)
integrations/src/
└── jupiter.rs       # quote + decode + fetch_signed_swap_transaction + swap_via_rpc
cli/src/
├── close_accounts.rs
└── swap_all.rs
```

Dependency direction: `cli` → `integrations` → `rpc`. The `rpc` crate never depends on `integrations`.

Typed errors with `error_stack`: `CliError` wraps `RpcError` and `IntegrationError`.

## Requirements

- Rust stable (edition 2024).
- A Jupiter account at https://portal.jup.ag (only if you're going to use `swap-all`).
- A Solana RPC. The public one (`api.mainnet-beta.solana.com`) works for testing, but for real use prefer Helius / Triton / QuickNode / Alchemy.

### Installation

```bash
git clone <repo>
cd solana-utils
cargo build --release
# binary at target/release/solana-utils
```

### Environment variables

`.env` at the root (auto-loaded):

```env
SOLANA_PRIVATE_KEY=<base58_private_key>        # Phantom/Solflare export format
JUP_API_KEY=<jupiter_api_key>                  # only for swap-all
RUST_LOG=info                                  # optional
```

The private key never leaves the process — signing happens locally via `Keypair::from_base58_string`.

## `config.toml`

One section per subcommand. Whitelists are mints the command will never touch.

```toml
[close_accounts]
whitelist_mints = [
    "So11111111111111111111111111111111111111112",   # WSOL (already filtered internally)
]

[swap_all]
whitelist_mints = [
    "So11111111111111111111111111111111111111112",   # WSOL
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",  # USDC (if it's your target)
]
```

Missing sections = empty whitelist. Invalid mints abort with `CliError::Config`. Default path is `./config.toml`, override with `--config <PATH>`.

## Commands

```bash
solana-utils --help
solana-utils <command> --help
```

### `close-accounts`

Walks both SPL programs, filters out accounts with balance > 0 and those in the whitelist, and closes the rest in parallel. WSOL is always excluded (closing it is equivalent to "unwrap" and that flow is left to the user).

| Flag | Default | Purpose |
|---|---|---|
| `--config <PATH>` | `config.toml` | Useful for profiles (`--config ./cold-wallet.toml`). |
| `--rpc-url <URL>` | public mainnet-beta | With many accounts the public RPC returns 429 quickly. |
| `--destination <PUBKEY>` | the signer itself | Where to send the reclaimed rent. |
| `--dry-run` | off | Prints which accounts would be closed without sending anything. |

```bash
solana-utils close-accounts --dry-run
solana-utils close-accounts --rpc-url https://mainnet.helius-rpc.com/?api-key=... --destination 9XYz...
```

Requires `SOLANA_PRIVATE_KEY`.

### `swap-all`

```bash
solana-utils swap-all --target <TARGET> [OPTIONS]
```

Flow:

1. List every token account in the wallet.
2. Filter: native SOL/WSOL, the target itself, whitelist, zero balances, frozen accounts, and duplicates of the same mint (keeps the one with the largest balance).
3. Classify via Jupiter Price v3: if there's a price and `amount_raw / 10^decimals * usdPrice ≥ --min-usd-value` → **swap**. Otherwise → **burn**.
4. Sequential swap: Jupiter `/order`, local signing, send through the RPC with `send_and_confirm`. 1s delay before every Jupiter call and 1s after each confirmed TX.
5. Post-swap verification: if Jupiter leaves dust (Token-2022 fees, rounding, transfers arriving mid-swap), the account is redirected to the burn pipeline instead of failing with 0xb when trying to close it.
6. Close: balance 0 → `close_token_accounts` (20 ix/TX). With residual balance + burnables → `burn_and_close_token_accounts` (10 pairs/TX). Frozen accounts are skipped with a warning.

| Flag | Default | Purpose |
|---|---|---|
| `--target <PUBKEY>` | **required** | Usually USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`). |
| `--config <PATH>` | `config.toml` | — |
| `--rpc-url <URL>` | public mainnet-beta | Use a private one; the command makes many queries and sends several TXs. |
| `--slippage-bps <N>` | `50` | In bps (100 bps = 1%). Low-liquidity memecoins may need 200-500. |
| `--min-usd-value <F>` | `0.01` | Below this no swap is attempted, goes straight to burn. Avoids spending fees on dust. |
| `--destination <PUBKEY>` | the signer itself | Recipient of the reclaimed rent. |
| `--no-close` | off | Swap only, leaves accounts for inspection. |
| `--dry-run` | off | Prints the full classification without sending TXs. Does query Jupiter Price and the RPC. |

```bash
# Preview — recommended before the first real run
solana-utils swap-all \
  --target EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --dry-run

# Consolidate to USDC, 1% slippage, ignore <$0.10
solana-utils swap-all \
  --target EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --slippage-bps 100 \
  --min-usd-value 0.10

# Swap, inspect, close separately
solana-utils swap-all --target ... --no-close
solana-utils close-accounts --dry-run
solana-utils close-accounts
```

Requires `SOLANA_PRIVATE_KEY` and `JUP_API_KEY`.

## Notes

- **Frozen accounts**: mints with active `freeze_authority` (scam tokens or regulated stablecoins) are detected when listing and skipped across every pipeline.
- **Idempotency**: `swap-all` can be re-run; mints already drained (balance 0) are ignored automatically. Individual failures (slippage, quote unavailable, reverted TX) are logged as `warn!` and do not abort the run.
- **Token-2022**: transfer fee extensions leave dust after the swap — that's why step 5 exists. Exotic extensions (transfer hooks, confidential transfers) may not be routable by Jupiter; those mints fall straight into burn+close.
- **Rate limits**: the 1s delay in the swap loop is usually enough for Jupiter's free/basic tier. For many accounts in `close-accounts`, use a private RPC.

Logs to stderr via `tracing`:

```bash
RUST_LOG=solana_utils=debug,rpc=debug cargo run -p cli -- swap-all --target ... --dry-run
```
