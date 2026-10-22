---
title: Configure Claude Code
description: Connect Claude Code to a running proxy and choose models without changing global settings.
---

Claude Code reads its API connection when the process starts. These variables belong to **Claude Code**, not to the proxy server.

## Connect to a running proxy

Start `claude-code-proxy serve` in one terminal, then run in another:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
  claude
```

The placeholder token satisfies the client's credential requirement; it is not upstream authentication. The proxy uses its own ChatGPT login. These settings apply only to this command and do not edit global Claude Code settings. Claude Code keeps control of tools, permissions, skills, hooks, and subagents.

## Choose model families

Optionally select a main model and an independent model for background work:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=fable \
ANTHROPIC_SMALL_FAST_MODEL=haiku \
  claude
```

Fable maps to Astra, Opus to GPT-6.1 Sol, Sonnet to GPT-6 Sol, and Haiku to Luna. A well-formed Codex ID need not appear in discovery: it is forwarded for upstream validation, with no silent substitution. Run `claude-code-proxy models` for the current catalog.

Use `/model fable`, `/model opus`, `/model sonnet`, or `/model <Codex-ID>` inside Claude Code without exiting or rewriting global settings. Remove conflicting `CCP_CODEX_MODEL` / `codex.model` pins from the proxy configuration so independent main/small selection and `/model` switching work. See [Switching models](/using/switching-models/).

Stock Claude Code restricts `/fast` to Opus and may switch another family to Opus before sending a request. The proxy does not undo that UI choice; use supported `/model fable-fast`, `/model sonnet-fast`, or `/model haiku-fast` aliases for other families. Codex OAuth, supported tiers, entitlement, and quota remain authoritative; see [fast-mode limits](/using/models-and-routing/#claude-code-fast-is-opus-only).

## Optional client controls

Add these variables only to the `claude` invocation when needed; the proxy does not set them or edit global settings:

- `CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS=1` and `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1` skip Anthropic fast-mode availability checks. They do not bypass Codex OAuth, model/tier support, entitlement, or quota, and `/fast` remains Opus-only.
- `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` avoids a non-streaming retry after a partial stream that could duplicate tool calls.
- `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` reduces nonessential client traffic.

## MCP tool search

Claude Code normally turns tool search off when `ANTHROPIC_BASE_URL` is not an Anthropic host. To keep deferred tool loading enabled, optionally add `ENABLE_TOOL_SEARCH=true` to the `claude` invocation. The proxy appends loaded tools where they were loaded, preserving append-only input for prompt caching; see [Codex tools](/providers/codex/#tools-and-multimodal-input).

## Context and compaction

A trailing `[1m]` is a client hint only: the proxy strips it and it cannot expand the upstream context window. Claude Code controls its own compaction independently of the proxy's model metadata. Compact early with `/compact` when approaching the upstream limit or switching to a smaller model. A larger advertised maximum window does not establish the active subscription capacity.

## Proxy settings are separate

`CCP_*`, `PORT`, and `config.json` configure the **claude-code-proxy server process**. They control the listener, discovery, provider endpoints, transport, credentials, and diagnostics. They do not belong in Claude Code's client environment unless the same shell also starts the proxy.

See [Configuration](/reference/configuration/) for the canonical server setting table.
