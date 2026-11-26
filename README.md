<p align="center">
  <a href="https://orvyn.cash">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset=".github/assets/orvyn-lockup-white.svg">
      <img alt="Orvyn" src=".github/assets/orvyn-lockup.svg" height="52">
    </picture>
  </a>
</p>

<h1 align="center">Orvyn Core</h1>

<p align="center"><b>The Rust core behind Orvyn: agents, wallets and wallet policy, payments, Vyn and the job worker.</b></p>

<p align="center">
  <a href="https://orvyn.cash">Website</a> ·
  <a href="https://app.orvyn.cash">App</a> ·
  <a href="https://docs.orvyn.cash">Docs</a> ·
  <a href="https://x.com/OrvynOS">X</a> ·
  <a href="https://t.me/OrvynHQ">Telegram</a>
</p>

<p align="center">
  <a href="https://github.com/OrvynOS/Core/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/OrvynOS/Core/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/OrvynOS/Core/blob/main/LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-8bc8ff?style=flat-square&labelColor=000000"></a>
  <img alt="BNB Chain" src="https://img.shields.io/badge/live%20on-BNB%20Chain-8bc8ff?style=flat-square&labelColor=000000">
</p>

Orvyn's backend core in Rust (the `orvyn-core` crate): settings, the database and its migrations,
secrets, auth, wallets and the wallet policy, agents and Bliss plugins, the agent runtime, Vyn,
games, Studio jobs, payments (OrvynPay, x402), RNG and price feeds, OAuth, and the job worker. Used
by [orvyn-api](https://github.com/OrvynOS/Api) and [orvyn-agent](https://github.com/OrvynOS/Agent), as a git dependency by tag:

```toml
orvyn-core = { git = "https://github.com/OrvynOS/Core", tag = "v0.1.0" }
```

Migrations are in `migrations/`, with drizzle's journal and bookkeeping (as the TypeScript core
kept them): add one with `cargo run -p orvyn-admin -- new-migration <name>`; never edit an applied
migration or regenerate `0000_init.sql`. The vectors in `tests/vectors/` are the retired
TypeScript core's outputs, frozen: the tests check this crate still gives them.

```bash
cargo test     # starts a throwaway Postgres (postgresql_embedded) on port 55433 and reuses it
cargo run -p orvyn-admin           # the operator tool: API clients, holds, wallet checks, Privy
                                   # policies, a preview database, migrations, contract fixtures
```

Also here: [mcp/](mcp) (`orvyn-mcp`), Orvyn's trading tools (PancakeSwap, Uniswap, four.meme, Flap:
quotes and unsigned transactions), served at `api.orvyn.cash/mcp` and self-hostable per venue
(`cargo run -p orvyn-mcp`); and [admin/](admin), the operator tool.

[PORTING.md](PORTING.md) records how this crate follows the TypeScript core, and where it differs.

## License

[MIT](LICENSE). Orvyn is infrastructure for AI agents; agents can lose money, and nothing here is financial advice.
