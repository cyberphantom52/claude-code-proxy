---
title: Switching models
description: Change Codex models in Claude Code without changing the upstream backend.
---

With Claude Code connected to the proxy, choose a Codex model at launch:

```sh
claude --model gpt-6-sol
```

Or change it in the running session:

```text
/model gpt-6-sol-fast
/model gpt-6-luna
```

`ANTHROPIC_MODEL` sets the launch default; `ANTHROPIC_SMALL_FAST_MODEL` separately selects background work. All resolve to the same ChatGPT Codex backend and proxy-owned OAuth account. Use `claude-code-proxy models` for accepted IDs.

Claude Code keeps portable conversation history, but changing models can invalidate optional Codex continuation or compaction state. `[1m]` is only a client context hint; see [Models and routing](/using/models-and-routing/).

Connection settings are read when Claude Code starts. Changing `ANTHROPIC_BASE_URL` or client credentials requires a new process. The proxy does not rewrite Claude Code settings or provide runtime backend switching.
