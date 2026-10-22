# claude-code-proxy

Claude Code, powered by **OpenAI Codex through your ChatGPT subscription**.

<a href="https://aventasks.dev/">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://aventasks.dev/readme/aven-banner-dark.webp">
    <img src="https://aventasks.dev/readme/aven-banner-light.webp" alt="aven: one task queue for you and your coding agents" width="100%">
  </picture>
</a>

<sub>Also from the author of claude-code-proxy: <a href="https://aventasks.dev/"><b>aven</b></a>, one
queue for every project, with a keyboard-driven TUI for you and a CLI for your
agents.</sub>

## Why?

Claude Code remains an excellent coding harness, with strong tools, skills,
hooks, subagents, and editor integrations. claude-code-proxy keeps that client
experience while translating its Anthropic API traffic to the ChatGPT Codex
backend. Codex is the only upstream backend; Claude Code is the client harness.
OpenAI-compatible routes describe local request schemas, not Platform API access.

One local process handles proxy-owned ChatGPT OAuth, model selection,
protocol translation, streaming responses, and diagnostics. The built-in
monitor shows sessions, active and recent requests, errors, token usage, and
throughput.

## Quick start with Codex

Install on macOS or Linux:

```sh
brew install raine/claude-code-proxy/claude-code-proxy
```

Or use the release installer:

```sh
curl -fsSL https://raw.githubusercontent.com/raine/claude-code-proxy/main/scripts/install.sh | bash
```

Windows and other prebuilt artifacts are available from
[GitHub Releases](https://github.com/raine/claude-code-proxy/releases).

Sign in with a **ChatGPT Plus or Pro account**, not an OpenAI API account:

```sh
claude-code-proxy codex auth login
```

The proxy stores and refreshes its own OAuth tokens. It does not use OpenAI
Platform API keys or native Codex CLI credentials.

Start the proxy in one terminal:

```sh
claude-code-proxy serve
```

For a background service, use `claude-code-proxy serve --no-monitor` and attach
from another terminal with `claude-code-proxy monitor`. Closing an attached
dashboard leaves the proxy running.

Start Claude Code in another:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
  claude
```

These connection settings apply only to this command; no global settings change
is needed. Optionally add `ANTHROPIC_MODEL=fable` and
`ANTHROPIC_SMALL_FAST_MODEL=haiku` to select main and background model families.

Switch models inside Claude Code without exiting:

```text
/model fable
/model opus
/model sonnet
```

Fable maps to `gpt-6-astra`, Opus to `gpt-6.1-sol`, Sonnet to `gpt-6-sol`,
and Haiku to `gpt-6-luna`. Well-formed explicit IDs pass through even when
absent from discovery; upstream validates access. Remove conflicting proxy-side
`CCP_CODEX_MODEL` / `codex.model` overrides on the proxy. Errors never cause
substitution with a different model. `[1m]` cannot increase the upstream context
window; Claude Code manages client compaction independently.

Optional Codex image generation and editing can reuse the same ChatGPT login:

```sh
CCP_CODEX_IMAGES_API=1 claude-code-proxy serve
curl http://127.0.0.1:18765/v1/images/generations \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"A paper-cut fox","model":"gpt-image-2"}'
```

The opt-in Images API returns base64 image data and consumes the signed-in account's image quota. Image prompts and payloads are excluded from traffic captures.

## Codex models and APIs

Run `claude-code-proxy models` for the current catalog, fetched directly with
proxy-owned ChatGPT OAuth from
`GET /backend-api/codex/models?client_version=0.163.0`. No installed Codex CLI
or automatic `~/.codex` / `CODEX_HOME` reads are needed. Built-in models provide
a fallback; `CCP_CODEX_MODELS_FILE` / `codex.modelsFile` explicitly imports optional
metadata. Discovery defaults on; `CCP_CODEX_MODEL_DISCOVERY=0` or
`codex.modelDiscovery=false` opts out of discovery network requests.

The catalog supplies menus, default choices, and known effort, verbosity, lane,
context, and service-tier metadata—not an admission allowlist. Well-formed
opaque model IDs are forwarded for upstream validation without substitution.
`-fast` / `-ultrafast` select supported tiers; paid acceleration is not enabled
merely by an advertised default tier. Stock Claude Code `/fast` is Opus-only
and may switch to Opus before sending a request; for other families use
`/model fable-fast`, `/model sonnet-fast`, or `/model haiku-fast` when supported.
Catalog advertising does not guarantee account entitlement or quota.

Anthropic Messages is enabled by default. Set `CCP_CODEX_RESPONSES_API=1` on the
proxy to enable native Codex Responses and Chat Completions compatibility.
Images have their separate opt-in gate shown above.

> [!WARNING]
> The proxy accepts local requests without client authentication. It binds to
> `127.0.0.1` by default. Protect any non-loopback listener with a firewall or
> authenticating reverse proxy. Subscription terms, model access, quotas, and
> account enforcement remain under OpenAI's control.

## Related projects

- [aven](https://github.com/raine/aven): local-first task management for power
  users and agents
- [claude-history](https://github.com/raine/claude-history): search Claude Code
  conversation history from the terminal
- [git-surgeon](https://github.com/raine/git-surgeon): non-interactive
  hunk-level git staging for coding agents
- [workmux](https://github.com/raine/workmux): parallel coding tasks in git
  worktrees and tmux
- [consult-llm](https://github.com/raine/consult-llm): consult other AI models
  from an agent workflow

## License

[MIT](LICENSE)
