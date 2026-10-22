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
