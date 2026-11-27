# Vyness

Run Claude Code, OpenCode, Aider or any OpenAI-compatible coding agent on **Orvyn AI**, paid from your Orvyn credits.

```bash
cargo install --git https://github.com/OrvynOS/Core vyness
vyness login        # approve the code at app.orvyn.cash/device
vyness claude       # or: vyness opencode, vyness aider, vyness run -- <command>
vyness credits
```

`vyness login` signs in with Orvyn (device flow, client `orv_vyness`, scope `inference`) and stores an Orvyn AI key for this computer in `~/.vyness/config.json` (or `$VYNESS_HOME`). Revoke it under API keys at app.orvyn.cash/credits.

Docs: https://docs.orvyn.cash/developers/vyness
