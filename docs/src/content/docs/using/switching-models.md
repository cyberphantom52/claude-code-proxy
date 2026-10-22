---
title: Switching models
description: Switch Codex model families inside Claude Code without exiting or rewriting global settings.
---

With the proxy already running, launch Claude Code once:

```sh
claude-code-proxy launch --model fable --small-model haiku
```

Then switch **inside the running Claude Code session**:

```text
/model fable
/model opus
/model sonnet
/model haiku
/model gpt-6-astra
/model gpt-6.1-sol
/model gpt-6-sol
/model gpt-6-luna
```

Fable resolves to Astra, Opus to GPT-6.1 Sol, Sonnet to GPT-6 Sol, and Haiku to Luna. Versioned family names such as `claude-opus-5-5` use the same mapping. Recognized tier/context suffixes survive resolution; for example, `/model fable-fast` requests Astra priority service if supported. Well-formed opaque IDs pass through unchanged even when absent from discovery; upstream decides availability and entitlement. Incompatible known controls error instead of substituting another model.

## Fast service without an unexpected model switch

Stock Claude Code's `/fast` is currently Opus-only and can switch another family to Opus before sending the request. The proxy preserves the model actually received; it does not guess and undo the client's change. To request another family's priority tier, use supported aliases:

```text
/model fable-fast
/model sonnet-fast
/model haiku-fast
```

Messages `speed: "fast"` maps to Codex priority for the received model, and `speed: "standard"` clears acceleration. This wire support does not remove the stock CLI's Opus-only UI restriction. See [service tiers and `/fast`](/using/models-and-routing/#claude-code-fast-is-opus-only).

No exit or global `settings.json` override is needed. `ANTHROPIC_MODEL` sets the launch default; `ANTHROPIC_SMALL_FAST_MODEL` independently selects background work. All requests still use the same ChatGPT Codex backend and proxy-owned OAuth account. Run `claude-code-proxy models` for the active catalog; listed models can still encounter upstream entitlement or quota errors.

## Forced model overrides

`CCP_CODEX_MODEL` or `codex.model` is a legacy model pin. Anthropic Messages requests that conflict with it fail instead of silently changing models. Remove the pin **before launch**, and restart the proxy if its configuration changed. The launcher rejects conflicting pins in its own configuration, but cannot inspect a separately running proxy's environment. OpenAI-compatible requests preserve their own model IDs.

## Context and connection boundaries

The launcher derives conservative context/compaction limits from the main and small models at launch; `/model` does not recompute those environment variables. Keep the cap safe for all models you intend to use and compact early before switching to a smaller window. `[1m]` never increases upstream capacity.

Claude Code keeps portable conversation history, but model changes can invalidate optional Codex continuation or compaction state. Changing the base URL or credentials requires a new client process; changing the model does not. See [Models and routing](/using/models-and-routing/) for discovery and capability rules.
