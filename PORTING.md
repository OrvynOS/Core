# Porting the core to Rust

Orvyn's backend was TypeScript (`packages/core`, ~20k lines, and the apps/api, runtime, agent, mcp
and mail servers). It's all Rust now: this crate (`orvyn-core`) and the servers in `crates/`. The
TypeScript backend was removed on 2026-11-25; it's kept in an archive outside the repository
(`orvyn-typescript-backend-2026-11-25.tar.gz`). The Rust core kept its database, JSON shapes
(camelCase), secrets, tokens, signatures and hashes, so data the TypeScript backend wrote reads as
it is.

- `tests/vectors/*.json` are that core's outputs, frozen: the tests check the Rust core still gives
  them. They were made by scripts that ran the TypeScript core (in the archive); don't regenerate
  them, add new Rust tests instead.
- Migrations live in `migrations/` with drizzle's journal and bookkeeping
  (`drizzle.__drizzle_migrations`). Add one with `orvyn-admin new-migration <name>`; never edit
  an applied migration.
- Tests use a real Postgres (`tests/support`, postgresql_embedded on port 55433, reused).
  Don't pipe `cargo test` output: the test Postgres inherits the pipe. Redirect to a file.

## Order and status

| Stage | Module (TypeScript) | Rust | Status |
|---|---|---|---|
| 1 Foundation | env.ts, secrets.ts, db/ (schema, migrations), push.ts | env, secrets, db, push, http, error | done |
| 1 | evm/ (chains, client, agent) | evm (reads, units, signature checks incl. ERC-1271/6492) | reads done; sending moves to wallets |
| 1 | auth.ts (SIWE, sessions, mobile codes, holder gate), accounts.ts | auth, accounts | done (vectors: auth.json) |
| 1 | privy.ts, wallets.ts | privy, wallets | done: Privy REST, identity tokens, wallet RPC with authorization signatures (vectors: privy.json, byte-exact with the SDK), local signer (vectors: local-signer.json, viem-exact), sending through the policy |
| 2 Money | wallet-policy/ | wallet_policy | done: every rule, caps, grants, guard (vectors: wallet-policy.json, 147 verdicts identical to the TypeScript core); the Privy-side policies (privy_policy, vectors: privy-policy.json, rule for rule) and `orvyn-admin privy-policy`, which creates them |
| 2 | x402/ | x402 | done: client, the paywall (`Paywall`: admit, serve, settle; tested with a mock facilitator), facilitator (vectors: x402.json, TS-signed payments verify) |
| 2 | binancepay.ts, billing.ts, subscription.ts, invoices.ts, withdraw.ts | | done: billing, subscription, invoices, Binance Pay (vectors: billing.json), withdrawals; tiers, environment (hash vectors), hosting, ERC-8004 |
| 2 | orvynpay/ | orvynpay | done: Poseidon, trees, notes, encrypted notes, transactions, pool scanning, standard and private payments (vectors: orvynpay.json). Groth16 proving with ark-circom from the circuit's wasm + zkey (as snarkjs); a proof made here verifies with `snarkjs groth16 verify` (test `proves_with_the_circuit_build`, run with ORVYN_SHIELDED_WASM/_ZKEY set) |
| 2 | hires/, jobs.ts | hires, jobs | done (vectors: mail.json): escrow calls, chain mirroring, messages and email, deliveries, ratings, listings |
| 3 Identity | identity.ts, agentid.ts, erc8004.ts, public.ts, views.ts, oauth/, external-agents.ts | identity, agentid, erc8004, public, views, oauth, external_agents | done (vectors: misc.json, oauth.json: same key gives the same JWKS, TypeScript-signed tokens verify; flows: code + PKCE, refresh rotation and reuse detection, device login, revocation) |
| 3 | rng/, price/ | rng, price | done (vectors: rng.json): BLS (blst), drand verification, rounds, relayer, price network, sources and consensus; a full round of both networks on a database |
| 4 Agents | ai/ (Anthropic, OpenAI-compatible, Google providers) | | done: Anthropic, OpenAI-compatible (10 providers), Gemini; streaming tool loop; AI SDK message shapes (tests/ai.rs with mock providers) |
| 4 | agent/, bliss/ (23 plugins), worker.ts, lifecycle.ts, launchpad/, hosting.ts, environment.ts | agent, bliss, worker, lifecycle, launchpad, hosting, environment | done: all 23 plugins (vectors: catalog.json, the whole library identical; plugin-tasks.json); chat, autopilot, mail and job sessions; the worker (jobs, timers for Binance Pay, RNG, price feeds, Vyn) |
| 4 | vyn/ | vyn | done: settings, threads, turns with server and device tools against a mock model, memory, reminders and check-ins (time zones as Node's Intl, DST included), read_web with pinned public addresses, Telegram, the companion and ask_agent |
| 5 Products | games/, studio/, mail/, aster.ts, regions.ts | games, studio, mail, aster | done: mail (round trip: queue, retry, bounce, threads, job replies), studio (renders, files, models), games (vectors: games.json; the engine linked natively gives the WebAssembly build's exact results; TypeScript scripts compiled with oxc instead of esbuild), scores, worlds. Spec validation for renders is the worker's (crates/studio spec.rs) |
| 5 | cards.ts, portfolio.ts, external-agents.ts, api-clients.ts, regions.ts, data/ | cards, portfolio, api_clients (incl. country_of), data | done |
| All | db/schema.ts | db | tests/schema.rs reads a full row of every table into its Rust type (caught agent_plugins.created_at) |
| 6 Servers | apps/api routes, apps/runtime, apps/agent | crates/api (axum; runs the orchestrator too since 2026-11-25), crates/agent | done: every route of apps/api incl. MCP (crates/api/tests: host routing, sessions, listings, the MCP protocol on a throwaway database); `orvyn-api migrate`; the orchestrator; the agent service, hosted and self-hosted (self-host run end to end: embedded Postgres, migrations, plugins, status, chat, restart on the same data) |
| 6b Services | apps/mcp, apps/mail | crates/mcp, crates/mail | done: the trading MCP tools (served at api.orvyn.cash/mcp with Orvyn's own tools and called in process by agents since 2026-11-25; its binary self-hosts them per venue; live check against mainnet: quotes, builds and eth_simulateV1 dry runs on PancakeSwap v2/v3, Uniswap v3, four.meme and Flap, and every refusal) and Orvyn Mail (hosted by the API since 2026-11-26: provider webhooks in, an authenticated relay out; DKIM/DMARC with Stalwart's mail-auth, parsing with mail-parser, DKIM signing; end-to-end test in crates/api/tests/mail_webhook.rs: each provider's format, refusals, delivery through a relay) |
| 7 Switch | render.yaml, agent provisioning, standalone repos | Dockerfiles in crates/api, crates/agent | config done: render.yaml runs the Rust API (pre-deploy `orvyn-api migrate`) and orchestrator; agent services are provisioned from crates/agent/Dockerfile; scripts/split-repos.mjs makes the Rust orvyn-core (with orvyn-mcp and orvyn-admin), orvyn-api, orvyn-agent (with orvyn-node), orvyn-mail. The web app (which serves the landing too since 2026-11-25) doesn't use a backend library (the web's response types are in apps/web/src/lib/api-types.ts). Every backend service in render.yaml and the mail VPS runs Rust. Not deployed |
| 8 Retire | packages/core and its scripts, apps/api, runtime, agent, mcp, mail | crates/admin (`orvyn-admin`) | done: the TypeScript backend archived and removed. Its operator scripts are `orvyn-admin` subcommands (api-client, hold, wallet-check, privy-policy, preview-db, new-migration, and the contracts' fixtures: `fixture rng` gives the committed rng.json's keys, proofs and hash-to-curve vector byte for byte; `fixture orvynpay` a deposit of the same shape, proved with the circuit). Its check scripts went with it; see Left for the ones without a Rust test yet |

## Where the Rust core differs, on purpose

- Studio specs aren't validated before a render is sent; the worker validates them
  (crates/studio/src/spec.rs). A refused spec records nothing and costs no render.
- Game scripts in TypeScript are compiled with oxc, not esbuild: same JavaScript semantics,
  different wording of compile errors.
- The Orvyn Engine is linked natively, not loaded as WebAssembly (identical check results).
- OrvynPay proofs come from ark-circom with the same wasm + zkey; they verify with snarkjs.
- Vyn's turns send a "medium" effort setting (the TypeScript core sent none).
- The self-hosted agent's database is Postgres (postgresql_embedded, in `DATA_DIR/postgres`), not
  PGlite: an existing self-hosted agent keeps its secrets, Orvyn connection and RNG key, but its
  database starts fresh (the agent is recreated from the environment).
- The MCP servers' dry run measures the output with a balance read before and after the calls in
  the same eth_simulateV1 block (viem traced asset changes); their JSON keys come out in
  alphabetical order (serde_json without preserve_order, which would leak into other crates).
- Orvyn Mail runs inside the API (since 2026-11-26): a mail provider posts inbound mail to
  /api/mail/webhook and outbound mail goes through an authenticated SMTP relay; the TypeScript
  backend's own SMTP server (and its Rust port) is gone. Without the sender's IP, SPF isn't checked
  on inbound mail; DMARC rests on aligned DKIM.
- Agent services on Render are Docker services (crates/agent/Dockerfile, command `orvyn-agent`),
  not Node services.
- On `api.orvyn.cash`, `/x402/*` is routed to the facilitator (the TypeScript proxy 404'd it).
- Long async chains (`ai::generate`, `agent::chat::stream_chat`, `vyn::run_turn`,
  `vyn::companion::ask_agent`) return boxed futures: without that, proving `Send` through the
  nesting made a build take 16 minutes instead of 25 seconds.

## The TypeScript backend's checks, in Rust

Every `scripts/*-check.ts` of the TypeScript core is a Rust test now. Those that need nothing
outside the test run with `cargo test`; the rest are `#[ignore]`d and run with `-- --ignored`:

| Test | What | Needs |
|---|---|---|
| agents.rs | Virtual Card records; agents without a token (launch later); self-hosted agents connecting; Vyn as the person's companion agent | |
| vyn_companion.rs | memory, reminders with push, check-ins, DST, read_web refusals, Telegram (link, payment card, delivery), streaming | |
| capabilities.rs | Track Record grading, Automation through Trader, Randomness, price consensus | `live_data`: network (the price-source check also PYTH_HERMES_URL and an unthrottled EVM_RPC_BSC) |
| external_plugins.rs (+ misc.rs) | third-party MCP plugins: submission, review, pinning, keys, untrusted output, rate limit, timeout, suspension; production URL rules | |
| live.rs | Aster API (approval through the wallet policy, forged approval refused); Flap launch simulated on the live Portal | network |
| orvynpay_private.rs | the private cycle with real proofs against a stand-in pool | the circuit build; slow in debug (~3 min a proof): use `--release` (71 s) |
| worlds.rs | dream worlds against the real Worlds server, usage reports, allowances | `cargo build -p orvyn-worlds --release` |
| studio_jobs.rs | creative jobs against a real Studio worker: tiers, quotas, delivery, signed links, game thumbnails | `cargo build -p orvyn-studio`, Chromium, ffmpeg |
| rng_chain.rs | OrvynRNG and OrvynPriceFeed on anvil with the live drand beacon | Foundry (`forge build`, anvil) |

The stand-ins they use: a chain RPC where wallets are empty and calls revert
(`support::zero_rpc`), and env overrides for outside services: ANTHROPIC_BASE_URL,
VYN_TELEGRAM_API_URL, EXPO_PUSH_URL, DEXSCREENER_BASE_URL.

## Added since the port

- Binance MCP (agentic), `binance` module, `binance` plugin, Vyn: a person's Binance Agentic
  sub-account through Binance's hosted MCP server, connected with MCP OAuth (public client: a
  client metadata document at /api/binance/client.json, PKCE; Binance issues no refresh tokens, so
  people reconnect when access expires). Agents: reads in chat, writes only in autopilot and only
  within the owner's caps (USD per order and per day, products, symbols, leverage, transfers;
  unknown writes refused). Vyn: every write becomes an approval card (desktop, mobile, Telegram)
  and the server runs exactly the approved call. tests/binance.rs runs it against a stand-in
  Binance. Not yet tried against Binance itself: its tool names are only visible after signing in,
  so the read/write split (annotations, else read verbs) and the order sizing (quoteOrderQty /
  quantity × price) need checking with a real connection.

## Left

- Deploying: the Rust API, orchestrator, agents, MCP servers and mail have run locally against
  throwaway databases (and mainnet, read-only, for MCP), not on Render; the Docker
  images haven't been built here (no Docker).
- Real-key runs: a real AI key end to end, Privy signing, testnet payments.
- Not yet run here: studio_jobs.rs (no ffmpeg on this machine) and rng_chain.rs (no Foundry);
  both compile.
