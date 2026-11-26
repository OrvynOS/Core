# Orvyn trading MCP servers

Model Context Protocol tools for trading on BNB Chain.

**Orvyn serves them at `https://api.orvyn.cash/mcp`**, one endpoint with Orvyn's own tools ([orvyn-api](https://github.com/OrvynOS/Api)), and Orvyn's agents call them in process (`orvyn_mcp::call_json`). This crate's binary serves them per venue, for anyone who self-hosts:

| Endpoint | Venue | Tools |
|---|---|---|
| `/pancakeswap/mcp` | PancakeSwap v2 (fee-on-transfer swaps) and v3 (SmartRouter) | `pancakeswap_quote`, `pancakeswap_build_swap` |
| `/uniswap/mcp` | Uniswap v3 (SwapRouter02) | `uniswap_quote`, `uniswap_build_swap` |
| `/fourmeme/mcp` | four.meme bonding curves (TokenManager2) | `fourmeme_token_info`, `fourmeme_quote`, `fourmeme_build_buy`, `fourmeme_build_sell` |
| `/flap/mcp` | Flap bonding curves (Portal), BNB-quoted tokens | `flap_token_info`, `flap_quote`, `flap_build_swap` |

**They never sign or send anything and hold no keys.** Build tools return unsigned calls (`{to, data, value}`) for the given account: an exact-amount approval when needed, then the swap with a minimum output and, on DEXes, a deadline. The output always goes to the same account.

Every plan is dry-run with `eth_simulateV1` (the approval and the swap in order) from the account, reading the account's balance of the output token before and after. The minimum output comes from what that dry run measured when it's below the quote: launchpad estimates and tax tokens can overstate. The result says whether the dry run passed (`simulation.status`).

## Run

```bash
cargo run -p orvyn-mcp          # :3400
claude mcp add --transport http pancakeswap http://localhost:3400/pancakeswap/mcp
```

| Env | |
|---|---|
| `PORT` | default 3400 |
| `BSC_RPC_URL` | a BNB Chain RPC that supports `eth_simulateV1` (public ones do; use a dedicated one in production) |
| `MCP_API_KEYS` | optional comma-separated bearer keys (16+ chars); unset = open |
| `MCP_RATE_LIMIT_PER_MIN` | per key or IP, default 120 |
| `FLAP_PORTAL` | override the Flap Portal address |

Stateless Streamable HTTP with JSON responses: no sessions, so it scales horizontally.
Docker (from the repository root): `docker build -f mcp/Dockerfile -t orvyn-mcp .`

## Check

```bash
cargo test -p orvyn-mcp                                     # offline
cargo test -p orvyn-mcp --test live -- --ignored --nocapture   # live, read-only
```

The live check quotes and builds on every venue (live four.meme and Flap curve tokens are found from recent logs), dry-runs each plan from a funded mainnet wallet, and checks the refusals (unknown token, same token, no balance, slippage over the cap, selling more than circulates, missing arguments).

## Contracts (BNB Chain)

| | |
|---|---|
| PancakeSwap v2 router | `0x10ED43C718714eb63d5aA57B78B54704E256024E` |
| PancakeSwap v3 SmartRouter / QuoterV2 | `0x13f4EA83D0bd40E75C8222255bc855a974568Dd4` / `0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997` |
| Uniswap v3 SwapRouter02 / QuoterV2 | `0xB971eF87ede563556b2ED4b1C0b0019111Dd85d2` / `0x78D78E420Da98ad378D7799bE8f4AF69033EB077` |
| four.meme TokenManager2 / TokenManagerHelper3 | `0x5c952063c7fc8610FFDB798152D69F0B9550762b` / `0xF251F83e40a78868FcfA3FA4599Dad6494E46034` |
| Flap Portal | `0xe2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0` |

Orvyn's wallet policy (`src/wallet_policy/trade.rs` in this repository) accepts exactly these calls before an agent's wallet signs, and only when the output goes to the agent.
