---
title: Configure Claude Code
description: Launch Claude Code with child-only proxy settings, model-derived context limits, and tool search.
---

Claude Code reads its API connection when the process starts. These variables belong to **Claude Code**, not to the proxy server.

## Launch against a running proxy

Start `claude-code-proxy serve` in one terminal, then run in another:

```sh
claude-code-proxy launch
# Or choose model families explicitly:
claude-code-proxy launch --model fable --small-model haiku
```

The launcher uses the proxy-owned authenticated catalog, preferring Astra for the main model and Luna for small requests when listed with safe context metadata. Otherwise it chooses a listed model with usable metadata; an explicit context cap can supply a missing window. Explicit aliases or IDs are preserved in the child environment. A well-formed ID need not appear in discovery: it is forwarded for upstream validation, with no silent substitution. If its context window is unknown, supply a verified safe `--context-window`, such as `200000`.

This command starts **Claude Code only**, not the proxy. It sets client variables for that child process without editing global `~/.claude/settings.json` or changing your shell. Claude Code keeps control of tools, permissions, skills, hooks, and subagents. No upstream prompts are fetched or executed.

```sh
claude-code-proxy launch --proxy-url http://127.0.0.1:18765 --print-env
claude-code-proxy launch --model opus --small-model sonnet -- --resume
```

`--print-env` prints the computed environment as JSON without starting Claude Code. Arguments after `--` are passed to `claude`, except `--model` / `--model=...` are rejected: put model selection **before** `--` so the launcher can validate ID syntax and context. Remove conflicting `CCP_CODEX_MODEL` / `codex.model` forced overrides before launching, including from the running proxy's configuration; otherwise independent main/small selection and `/model` switching can be defeated. The launcher rejects conflicts it sees in its own configuration.

## Client environment

The launcher sets:

| Claude Code variable | Purpose |
| --- | --- |
| `ANTHROPIC_BASE_URL` | Running proxy URL; defaults to `http://127.0.0.1:<configured-port>`. |
| `ANTHROPIC_AUTH_TOKEN=unused` | Satisfies the client's credential requirement; not upstream auth. |
| `ANTHROPIC_MODEL` | Main alias or Codex ID. |
| `ANTHROPIC_SMALL_FAST_MODEL` | Alias or ID for title generation and other background work. |
| `ANTHROPIC_DEFAULT_FABLE_MODEL=claude-fable-5-1` | Canonical Fable alias, resolved by the proxy to Astra. |
| `ANTHROPIC_DEFAULT_OPUS_MODEL=claude-opus-5-5` | Canonical Opus alias, resolved to GPT-6.1 Sol. |
| `ANTHROPIC_DEFAULT_SONNET_MODEL=claude-sonnet-5-5` | Canonical Sonnet alias, resolved to GPT-6 Sol. |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-haiku-5-5` | Canonical Haiku alias, resolved to Luna. |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | Smaller usable context budget across the main and small models, including advertised safety headroom (95% by default). |
| `CLAUDE_CODE_AUTO_COMPACT_WINDOW` | Safe window ceiling: at most 90% of active context, capped by compaction metadata and usable budget. Actual compaction can occur earlier. |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` | Enables the client's best-effort gateway picker; typed `/model` aliases do not depend on picker enumeration. |
| `CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS=1` | Bypasses the child's Anthropic fast-mode network-availability check; not Codex entitlement or tier validation. |
| `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1` | Bypasses the child's Anthropic fast-mode organization check; not Codex entitlement or tier validation. |
| `ENABLE_TOOL_SEARCH=true` | Keeps deferred tool loading enabled behind the proxy. |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` | Reduces unnecessary subscription traffic. |
| `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` | Avoids duplicate tool calls from a non-streaming retry after partial streaming. |

These fast-mode check bypasses are child-only and leave global settings untouched. Stock Claude Code still restricts `/fast` to Opus and may switch another family to Opus before sending a request. The proxy does not undo that UI choice; use supported `/model fable-fast`, `/model sonnet-fast`, or `/model haiku-fast` for other families. Codex OAuth, supported tiers, entitlement, and quota remain authoritative; see [fast-mode limits](/using/models-and-routing/#claude-code-fast-is-opus-only).

The proxy always makes streaming upstream requests. It can still accumulate a non-streaming Anthropic response when the client requests one.

## MCP tool search

Claude Code normally turns tool search off when `ANTHROPIC_BASE_URL` is not an Anthropic host. That can send every MCP tool schema with every request. The launcher's `ENABLE_TOOL_SEARCH=true` keeps deferred tools available by name and loads schemas through `ToolSearch` as needed.

On Codex, the proxy appends loaded tools where they were loaded, preserving append-only input for prompt caching; see [Codex tools](/providers/codex/#tools-and-multimodal-input).

## Context and compaction

The launcher uses the active `context_window`, never promotes `max_context_window` to active capacity, and requires `--context-window` when only maximum-window metadata is known. For an active window of 272,000 tokens with default 95% safety headroom, it sets `CLAUDE_CODE_MAX_CONTEXT_TOKENS=258400` and the compaction window no higher than **244,800**. A lower advertised compaction limit or smaller small-model window lowers these values further. It does not use a larger maximum, such as 872,000, as active subscription capacity. The compaction window is a ceiling, not an exact trigger; Claude Code can compact earlier.

Use `--context-window <tokens>` to lower the cap, or to supply a known-safe capacity when metadata is absent. It cannot exceed an advertised active window. The launcher errors if the safe compaction window is below Claude Code's supported 100,000-token minimum, rather than emitting an ignored setting; the window is also capped at 1,000,000. For models whose verified active capacity is 272,000 tokens with default headroom and no lower limits, a manual equivalent includes:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=fable \
ANTHROPIC_SMALL_FAST_MODEL=haiku \
CLAUDE_CODE_MAX_CONTEXT_TOKENS=258400 \
CLAUDE_CODE_AUTO_COMPACT_WINDOW=244800 \
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 \
ENABLE_TOOL_SEARCH=true \
CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS=1 \
CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1 \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude
```

Use lower values if either model advertises a smaller active window or compaction limit; prefer the launcher rather than copying a fixed budget. A trailing `[1m]` is a client hint only: the proxy strips it and it cannot expand upstream context. `DISABLE_AUTO_COMPACT=1` preserves manual `/compact` but can let a session hit the upstream limit.

## Switching without global settings changes

Use `/model fable`, `/model opus`, `/model sonnet`, or `/model <Codex-ID>` inside Claude Code. You do not need to exit or rewrite global settings for a model change. Launch-time context caps remain conservative fixed client values; account for smaller models before switching. Only changing the connection URL or credentials requires a new client process. See [Switching models](/using/switching-models/).

## Proxy settings are separate

`CCP_*`, `PORT`, and `config.json` configure the **claude-code-proxy server process**. They control the listener, discovery, provider endpoints, transport, credentials, and diagnostics. They do not belong in Claude Code's client environment unless the same shell also starts the proxy.

See [Configuration](/reference/configuration/) for the canonical server setting table and [Command reference](/reference/command-reference/#launch) for launcher options.
