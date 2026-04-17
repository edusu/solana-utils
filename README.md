# solana-utils

A set of command-line utilities to **keep a Solana wallet clean**: close empty accounts to reclaim rent, and consolidate every balance into a single reference token (typically USDC) via Jupiter, burning mints with no liquidity.

Designed for real-world wallets that have accumulated airdrop leftovers, dead memecoins, dust from old swaps, and stragglers token accounts.

---

## Table of contents
- [What it does](#what-it-does)
- [Repo architecture](#repo-architecture)
- [Requirements and setup](#requirements-and-setup)
- [Configuration (`config.toml`)](#configuration-configtoml)
- [Available commands](#available-commands)
  - [`close-accounts`](#close-accounts)
  - [`swap-all`](#swap-all)
- [Environment variables](#environment-variables)
- [Behavior notes](#behavior-notes)

---

## What it does

Two wallet operations:

1. **Close empty accounts** (`close-accounts`). Every open SPL token account holds ~0.002 SOL in rent. Closing the ones already empty reclaims those lamports.
2. **Consolidate everything into a single token** (`swap-all`). Sells every balance you hold (except SOL and whitelisted mints) into a target mint (USDC by default) using Jupiter as aggregator, and afterwards cleans the drained accounts — burning the tokens with no liquidity so those accounts can also be closed.

Both commands honor a configurable whitelist per section in `config.toml` and have a `--dry-run` mode to preview what they would do without sending any transaction.

---

## Repo architecture

Cargo workspace with two crates:

```
solana-utils/
├── Cargo.toml           # workspace: members = ["cli", "rpc"]
├── config.toml          # whitelists per section
│
├── rpc/                 # library: RPC, SPL, Jupiter
│   └── src/
│       ├── lib.rs       # re-exports
│       ├── error.rs     # RpcError / RpcResult (error_stack)
│       ├── rpc.rs       # create_rpc_client, create_client
│       ├── jupiter.rs   # JupiterClient: /order + /price/v3, local signing
│       └── spl/
│           ├── mod.rs      # fetch_multiple_accounts_chunked
│           ├── tokens.rs   # list_wallet_token_accounts, get_tokens_data
│           ├── close.rs    # close_token_accounts + CloseableTokenAccount
│           ├── burn.rs     # burn_and_close_token_accounts
│           └── utils.rs    # is_native_sol_mint, WSOL consts
│
└── cli/                 # `solana-utils` binary
    └── src/
        ├── main.rs              # clap Cli + subcommand dispatch
        ├── config.rs            # Config / CloseAccountsConfig / SwapAllConfig
        ├── error.rs             # CliError / CliResult
        ├── close_accounts.rs    # `close-accounts` subcommand
        └── swap_all.rs          # `swap-all` subcommand
```

### `rpc` crate — reusable logic

| Module | What it provides |
|---|---|
| `rpc::rpc` | Builds `RpcClient` (and `anchor-client::Client`) with `confirmed` commitment. |
| `rpc::spl::tokens::list_wallet_token_accounts` | Lists **every** token account in the wallet across both programs (legacy SPL Token and Token-2022), parsing raw balance, mint and `frozen` state. |
| `rpc::spl::tokens::get_tokens_data` | Mint metadata (decimals, program_id, account_len) batching `getMultipleAccounts`. |
| `rpc::spl::close::close_token_accounts` | Closes a `Vec<CloseableTokenAccount>` in batches of up to 20 ix per TX, parallelizing the `send_and_confirm`. Filters non-zero balances and WSOL. |
| `rpc::spl::burn::burn_and_close_token_accounts` | For accounts with non-zero balance: burns the balance and closes, in `[burn, close_account]` pairs grouped into batches of 10 pairs (20 ix) per TX. Skips frozen accounts. |
| `rpc::spl::utils::is_native_sol_mint` | Detects WSOL and its alias. |
| `rpc::jupiter::JupiterClient` | HTTP client: `order()` for the Swap v2 `/order`, `prices()` for Price v3 (returns `PriceInfo { usd_price, decimals }`). |
| `rpc::jupiter::{fetch_signed_swap_transaction, swap_via_rpc}` | High-level helpers: fetch quote, decode base64, sign, and optionally send through the caller's RPC. |

### `cli` crate — user entry point

- Clap with subcommands (`Command::CloseAccounts`, `Command::SwapAll`).
- Typed errors in `CliError` (`Config`, `Rpc`, `Input`) wrapping `RpcError` via `error_stack::Report::change_context`.
- Loads `.env` on startup (`dotenvy::dotenv().ok()`).
- Structured logging with `tracing_subscriber` controlled by `RUST_LOG` (default `info`).

---

## Requirements and setup

### Tools
- **Rust** stable (edition 2024 in the workspace). `rustup update stable`.
- A **Jupiter** API account if you are going to use `swap-all`: https://portal.jup.ag → copy the API key.
- A Solana **RPC endpoint**. The default is the public `api.mainnet-beta.solana.com`, but the public ones have very strict rate limits — for real usage prefer Helius, Triton, QuickNode, Alchemy, etc.

### Installation
```bash
git clone <repo>
cd solana-utils
cargo build --release
# binary at target/release/solana-utils
```

### Environment variables
Create a `.env` file at the root (it is loaded automatically):
```env
SOLANA_PRIVATE_KEY=<your_private_key_in_base58>
JUP_API_KEY=<your_jupiter_api_key>    # only required for swap-all
RUST_LOG=info                         # optional, levels: trace, debug, info, warn, error
```

⚠️ The private key must be in base58 (standard Phantom / Solflare "Export Private Key" format).
⚠️ Never commit the `.env`.

---

## Configuration (`config.toml`)

TOML file with one section per subcommand. Example:

```toml
[close_accounts]
# Mints whose token accounts must never be closed.
whitelist_mints = [
    "So11111111111111111111111111111111111111112",   # WSOL (already filtered internally; listing it is cosmetic)
]

[swap_all]
# Mints that must NOT be swapped by `swap-all`. They stay untouched in the wallet.
whitelist_mints = [
    "So11111111111111111111111111111111111111112",   # WSOL
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",  # USDC (useful if USDC IS your target)
]
```

- Each section is `#[serde(default)]`, so an absent section = empty whitelist. An empty `config.toml` works.
- Mints must be valid base58 addresses. Invalid entries abort with `CliError::Config`.
- The default path is `./config.toml`; override it with `--config <PATH>`.

---

## Available commands

Generic invocation:
```bash
solana-utils <COMMAND> [OPTIONS]
```

Built-in help:
```bash
solana-utils --help
solana-utils <command> --help
```

---

### `close-accounts`

**Closes every token account with zero balance, honoring the whitelist.**

Walks both SPL programs (legacy Token and Token-2022), filters out those with balance > 0 or in `[close_accounts].whitelist_mints`, and closes the rest in parallel batches. WSOL is always excluded (closing WSOL is equivalent to "unwrap" and that flow is left to the user explicitly).

```bash
solana-utils close-accounts [OPTIONS]
```

| Flag | Type | Default | Purpose |
|---|---|---|---|
| `--config <CONFIG>` | path | `config.toml` | Path to the TOML with the whitelist. Lets you keep multiple profiles (`--config ./cold-wallet.toml`). |
| `--rpc-url <RPC_URL>` | URL | `https://api.mainnet-beta.solana.com` | RPC endpoint to use. If you are going to close many accounts, use a private RPC — the public one will throw `429` quickly. |
| `--destination <DESTINATION>` | base58 pubkey | (the signer itself) | Where to send the reclaimed rent lamports. Useful if you want to consolidate rent into a cold wallet or a treasury. |
| `--dry-run` | flag | off | Lists which accounts would be closed (`token_account`, `mint`, `program`) without sending anything. Safe to run. |

#### Examples
```bash
# Preview
solana-utils close-accounts --dry-run

# Close and send the rent to another wallet
solana-utils close-accounts \
  --rpc-url https://mainnet.helius-rpc.com/?api-key=... \
  --destination 9XYz...
```

#### Environment requirements
- `SOLANA_PRIVATE_KEY` mandatory.
- Does not need `JUP_API_KEY`.

---

### `swap-all`

**Sells every balance (except SOL and whitelist) to a target mint, then cleans the accounts.**

Full flow:

1. **Discover** every token account in the wallet.
2. **Filter** in `select_swap_candidates`: skip native SOL/WSOL, the target itself, the whitelist, zero balances, frozen accounts and duplicate accounts for the same mint (keeps the one with the largest balance and warns about the rest).
3. **Classify** with Jupiter Price v3 (`classify_candidates`):
   - If Jupiter returns `usdPrice` and `decimals`, and `value_usd = amount_raw / 10^decimals * usdPrice ≥ --min-usd-value` → **swappable**.
   - If there is no price or the value is below the threshold → **burnable** (it will be burned and closed).
4. **Swap loop** sequentially over the swappable ones: requests `/order` from Jupiter, locally signs the `VersionedTransaction`, waits and sends through the caller's RPC with `send_and_confirm_transaction`. Between iterations it sleeps 1s before Jupiter and 1s after a confirmed TX.
5. **Post-swap verification**: re-lists the token accounts on-chain. If any of the swapped ones still carries dust (Token-2022 transfer fees, route rounding, incoming mid-swap), it is redirected into the burn+close pipeline instead of failing with error 0xb when attempting `close_account`.
6. **Close / burn**:
   - Accounts at zero → `close_token_accounts` (batches of 20 ix/TX).
   - Accounts with residual balance + burnables from step 3 → `burn_and_close_token_accounts` (batches of 10 burn+close pairs per TX). Frozen accounts are skipped with a warning.

```bash
solana-utils swap-all --target <TARGET> [OPTIONS]
```

| Flag | Type | Default | Purpose |
|---|---|---|---|
| `--target <TARGET>` | base58 pubkey | **(required)** | Mint everything will be consolidated into. Typically USDC (`EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`). The target mint is never swapped against itself. |
| `--config <CONFIG>` | path | `config.toml` | TOML with `[swap_all].whitelist_mints`. |
| `--rpc-url <RPC_URL>` | URL | `https://api.mainnet-beta.solana.com` | RPC endpoint. A private one is recommended — the command issues many queries and sends several TXs. |
| `--slippage-bps <SLIPPAGE_BPS>` | integer | `50` | Slippage forwarded to Jupiter, in basis points (100 bps = 1%). `50` = 0.5%. For memecoins with low liquidity you may need `200`-`500`. |
| `--min-usd-value <MIN_USD_VALUE>` | float | `0.01` | USD threshold below which no swap is attempted: those mints go straight to burn+close. Avoids wasting fees trying to sell dust worth pennies. |
| `--destination <DESTINATION>` | base58 pubkey | (the signer itself) | Where the reclaimed rent (from closes and burns) is sent. |
| `--no-close` | flag | off | Skip both the close-accounts and the burn+close steps. Swap only. Useful if you want to inspect the accounts manually before cleaning. |
| `--dry-run` | flag | off | Prints the full classification (SWAP vs BURN with reason and estimated USD value) without sending any TX. It does make calls to Jupiter Price API and to the RPC to be able to classify — it is not completely offline. |

#### Examples

```bash
# Preview — ALWAYS recommended before the first real run
solana-utils swap-all \
  --target EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --dry-run

# Consolidate everything to USDC, slippage 1%, only attempt swap if worth >= $0.10
solana-utils swap-all \
  --target EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --slippage-bps 100 \
  --min-usd-value 0.10

# Swap only, inspect the result and close manually on a separate run
solana-utils swap-all \
  --target EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --no-close
solana-utils close-accounts --dry-run   # inspect
solana-utils close-accounts             # execute
```

#### Environment requirements
- `SOLANA_PRIVATE_KEY` mandatory.
- `JUP_API_KEY` mandatory.

---

## Environment variables

| Name | Required | Used by | Format |
|---|---|---|---|
| `SOLANA_PRIVATE_KEY` | ✔ | every command | base58 (Phantom/Solflare format) |
| `JUP_API_KEY` | only `swap-all` | `swap-all` | raw string from `portal.jup.ag` |
| `RUST_LOG` | — | tracing | e.g. `info`, `solana_utils=debug,rpc=info` |

They are auto-loaded from a `.env` at the repo root via `dotenvy` at binary startup.

---

## Behavior notes

### Security
- The CLI signs locally with `Keypair::from_base58_string(private_key)`. The key **never** leaves the process.
- No transaction is sent unless you run the command without `--dry-run`. `--dry-run` is the safe default before a real run.
- **Frozen** accounts (with `freeze_authority` activated by a scam token or a regulated stablecoin) are detected during listing and **skipped across every pipeline** — no swap, burn or close is attempted. They are left as-is with a warning.

### Rate limits
- Jupiter `/order` and `/price/v3` have per-API-key limits. The swap loop injects a 1s delay before every call and another 1s after each confirmed TX; that is usually enough for the free/basic tier.
- The public Solana RPC (`api.mainnet-beta.solana.com`) is very strict. For `close-accounts` with many accounts, or for `swap-all`, use a private RPC.

### Partial failures and retries
- In `swap-all`, an individual failure (parse error, `Failed to get quotes`, slippage exceeded, reverted TX) is logged as `warn!` and **does not abort** the run. The mint stays untouched for the next execution.
- The command is **idempotent**: re-running it automatically ignores the mints already drained (balance 0) and only retries the pending ones.
- The post-swap verification step guarantees that `close_account` is not attempted on accounts Jupiter left with dust — those are routed into the automatic burn.

### Token-2022
- Both token programs are listed and processed. Token-2022 accounts with transfer fee extensions can leave dust after the swap — the verification step detects it and routes into burn.
- Some exotic extensions (transfer hooks, confidential transfers) may not be routable by Jupiter. Those mints will fall into "no Jupiter price" → direct burn+close.

### Logs
Everything goes through `tracing` to stderr. For more detail:
```bash
RUST_LOG=solana_utils=debug,rpc=debug cargo run -p cli -- swap-all --target ... --dry-run
```
