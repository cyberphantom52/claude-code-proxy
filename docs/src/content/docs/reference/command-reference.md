---
title: Command reference
description: Canonical claude-code-proxy command syntax for serving, monitoring, listing models, version output, and provider authentication.
---

Running `claude-code-proxy` without a subcommand is equivalent to `claude-code-proxy serve`.

## Global version commands

```sh
claude-code-proxy --version
claude-code-proxy -v
claude-code-proxy version
```

Each prints `claude-code-proxy <version>`.

## `serve`

```sh
claude-code-proxy serve [--port <PORT>] [--no-monitor]
```

Starts the local HTTP proxy and blocks until shutdown.

| Option | Behavior |
| --- | --- |
| `--port <PORT>` | Overrides `PORT`, `config.json`, and the default for this invocation. |
| `--no-monitor` | Uses plain output even when stdout is a terminal. |

The bind address comes from `CCP_BIND_ADDRESS` or `bindAddress`. Interactive stdout opens the monitor unless `--no-monitor` is present. Non-terminal stdout uses plain mode.

Plain mode continues collecting monitor history and supports separate dashboards. SIGTERM (Unix) and Ctrl-C request graceful service shutdown.

## `launch`

```sh
claude-code-proxy launch [--model <ID>] [--small-model <ID>] \
  [--context-window <TOKENS>] [--proxy-url <URL>] [--print-env] \
  [-- <Claude arguments>]
```

Starts `claude` against an **already-running proxy**; it does not start a server or rewrite global settings. Claude Code must be on `PATH`. Model discovery uses this command's proxy-owned ChatGPT OAuth/configuration; keep it consistent with the server process.

| Option | Behavior |
| --- | --- |
| `--model <ID>` | Main family alias or Codex ID. Prefers listed Astra when omitted, otherwise a listed catalog model. |
| `--small-model <ID>` | Independent background model; prefers a listed Luna/small model when omitted. |
| `--context-window <TOKENS>` | Safe capacity when active context is absent (including max-only metadata), or a lower cap. Cannot exceed an advertised context bound; errors if the safe compaction window is below the client's 100,000-token minimum. |
| `--proxy-url <URL>` | Running proxy's HTTP(S) URL; defaults to `http://127.0.0.1:<configured-port>`. No embedded credentials, query, or fragment. |
| `--print-env` | Prints only the computed child environment as JSON; does not launch Claude. |
| `-- <Claude arguments>` | Passes remaining arguments unchanged, except forwarded `--model` / `--model=...` are rejected. Put model selection before `--` for validation. |

Explicit aliases/IDs are preserved in the child environment. A well-formed opaque ID can be absent from discovery; upstream validates it, while missing context metadata requires `--context-window` (for example, `200000`). Conflicting `CCP_CODEX_MODEL` / `codex.model` overrides are rejected; remove forced overrides from the server too so `/model` switching works. Defaults choose listed models with safe metadata (unless an explicit context cap supplies missing metadata), never an implicit substitute for an explicit model.

The child receives connection/main/small settings, `ENABLE_TOOL_SEARCH=true`, best-effort gateway model discovery, conservative context/compaction limits, the nonessential-traffic/non-streaming-fallback guards, and `CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS=1` / `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1` to bypass Anthropic client checks only. These do not grant Codex tier access or change the stock CLI's Opus-only `/fast` restriction. Context uses active rather than maximum capacity, with model safety headroom; the compaction window is a ceiling of at most 90% of active context, capped by metadata, usable budget, and 1,000,000 tokens (the actual trigger can be earlier). The command inherits terminal I/O and returns the child's exit code (on Unix, signal termination uses `128 + signal`). See [Configure Claude Code](/using/configure-claude-code/) for variables and examples.

## `monitor`

```sh
claude-code-proxy monitor [--url <URL>]
```

Attach a read-only dashboard to a running proxy. The default URL is `http://127.0.0.1:<configured-port>`. No provider login is needed in the dashboard process. `q` and `Ctrl-C` detach without stopping the proxy; multiple dashboards are supported. After a connection failure, the dashboard retains its last snapshot and retries. The proxy accepts monitor reads only from loopback peers; use an SSH port forward for another machine.

## `demo`

```sh
claude-code-proxy demo
```

Opens the monitor with deterministic simulated traffic. It does not bind a network port or contact providers.

## `models`

```sh
claude-code-proxy models
```

Prints the authenticated ChatGPT Codex model catalog and compatible aliases. Default-on discovery fetches the pinned native `/models?client_version=0.163.0` endpoint with proxy-owned OAuth, using a bounded account/endpoint/version-scoped cache and built-in/explicit-import fallback. No Codex CLI installation or automatic native configuration reads are needed. `CCP_CODEX_MODEL_DISCOVERY=0` disables network discovery. Claude-style aliases resolve to Codex, not Anthropic models; listing is not an entitlement or quota guarantee.

## ChatGPT OAuth authentication

```sh
claude-code-proxy codex auth login
claude-code-proxy codex auth device
claude-code-proxy codex auth status
claude-code-proxy codex auth logout
```

`login` uses browser PKCE; `device` uses a headless device-code flow. `status` reports account, expiry, and storage. Authenticate with a ChatGPT Plus or Pro subscription, not a Platform API key. Credentials belong to the proxy, not the native Codex CLI.

Missing credentials make `auth status` exit with status 1; other auth failures exit with status 2. Successful commands exit with status 0. Logout deletes the local proxy credential without revoking the upstream refresh token.

## Development commands

From a source checkout:

```sh
cargo run -- serve
cargo test --all
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
just check
just docs
```

`just docs` installs the locked documentation dependencies and starts the Astro development server on an available local port.
