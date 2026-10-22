# claude-code-proxy

Claude Code, powered by **OpenAI Codex through your ChatGPT subscription**.

Docs: <https://claude-code-proxy.raine.dev>

LLM docs: <https://claude-code-proxy.raine.dev/llms.txt>

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

For the local `chatgpt-codex-improvements` branch, see the
[source build, ChatGPT login, and PR selection guide](docs/src/content/docs/using/chatgpt-integration.md).

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
ANTHROPIC_MODEL=gpt-6-sol[1m] \
ANTHROPIC_SMALL_FAST_MODEL=gpt-6-luna[1m] \
CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000 \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude
```

See [Getting started](https://claude-code-proxy.raine.dev/getting-started/)
for the complete first session.

Optional Codex image generation and editing can reuse the same ChatGPT login:

```sh
CCP_CODEX_IMAGES_API=1 claude-code-proxy serve
curl http://127.0.0.1:18765/v1/images/generations \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"A paper-cut fox","model":"gpt-image-2"}'
```

The opt-in Images API returns base64 image data and consumes the signed-in account's image quota. Image prompts and payloads are excluded from traffic captures. See the [HTTP API](https://claude-code-proxy.raine.dev/reference/http-api/) for generation and edit schemas.

## Codex models and APIs

Run `claude-code-proxy models` for the current Codex catalog. Registered models
support `-fast`; models offering the ultrafast tier also support `-ultrafast`.
Account access and quota come from your ChatGPT subscription.

Anthropic Messages is enabled by default. Set `CCP_CODEX_RESPONSES_API=1` on the
proxy to enable native Codex Responses and text-only Chat Completions compatibility.
Images have their separate opt-in gate shown above.

> [!WARNING]
> The proxy accepts local requests without client authentication. It binds to
> `127.0.0.1` by default. Protect any non-loopback listener with a firewall or
> authenticating reverse proxy. Subscription terms, model access, quotas, and
> account enforcement remain under OpenAI's control.

## Documentation

- [What is claude-code-proxy?](https://claude-code-proxy.raine.dev/)
- [Codex backend](https://claude-code-proxy.raine.dev/providers/codex/)
- [Configure Claude Code](https://claude-code-proxy.raine.dev/using/configure-claude-code/)
- [Models and routing](https://claude-code-proxy.raine.dev/using/models-and-routing/)
- [Monitor TUI](https://claude-code-proxy.raine.dev/using/monitor-tui/)
- [Troubleshooting](https://claude-code-proxy.raine.dev/using/troubleshooting/)
- [Command reference](https://claude-code-proxy.raine.dev/reference/command-reference/)
- [Configuration](https://claude-code-proxy.raine.dev/reference/configuration/)
- [HTTP API](https://claude-code-proxy.raine.dev/reference/http-api/)
- [Compatibility and limitations](https://claude-code-proxy.raine.dev/reference/compatibility-and-limitations/)
- [For coding agents](https://claude-code-proxy.raine.dev/using/for-coding-agents/)

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
