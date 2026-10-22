---
title: Models and routing
description: Select Codex models, Claude-style aliases, fast tiers, context hints, and gateway discovery.
---

Every accepted model resolves to Codex. Claude-style names are compatibility aliases, not access to Anthropic models.

## Discover models

```sh
claude-code-proxy models
curl http://127.0.0.1:18765/v1/models
```

The registry combines built-in models with API-supported entries in `$CODEX_HOME/models_cache.json` (default `~/.codex/models_cache.json`). Hidden catalog models can be requested explicitly. This reads metadata only, not native Codex credentials. Unknown IDs return HTTP 400 with the supported catalog; registration does not guarantee ChatGPT account access.

## Claude-style aliases

| Alias family | Codex model |
| --- | --- |
| `haiku`, `claude-haiku-*` | `gpt-6-luna` |
| `sonnet`, `claude-sonnet-*` | `gpt-5.6-terra` |
| `opus`, `fable`, `claude-opus-*`, `claude-fable-*` | `gpt-6-sol` |

Use concrete Codex IDs when you want explicit model selection.

## Fast tiers

Every registered Codex model has a `-fast` form, which removes the suffix upstream and requests `service_tier: "priority"`. Models offering ultrafast also have `-ultrafast` forms: currently `gpt-6-astra-ultrafast` and `gpt-6.1-sol-ultrafast`.

On `/v1/messages`, `codex.serviceTier` or `CCP_CODEX_SERVICE_TIER` overrides these suffixes. Forced ultrafast falls back to priority for models without that tier. OpenAI-compatible routes use the suffix or, on `/v1/responses`, the request's own `service_tier`.

## Context and small models

```sh
ANTHROPIC_MODEL=gpt-6-sol[1m]
ANTHROPIC_SMALL_FAST_MODEL=gpt-6-luna[1m]
CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000
```

`[1m]` is a Claude Code context hint; the proxy strips it before model matching. It does not enlarge Codex context. Set a safe compaction threshold for your model and subscription or omit the hint. The small/fast model handles background requests independently of the main model.

## Switching and gateway discovery

With `ANTHROPIC_BASE_URL` pointing to the proxy, use `claude --model <id>`, `/model <id>`, or `ANTHROPIC_MODEL` to select another Codex model. This does not switch upstream backends or change the client's base URL.

Set `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` before starting Claude Code to enable its gateway picker. `/v1/models` includes Codex IDs and Claude-style aliases; the picker filters for IDs beginning with `claude` or `anthropic`, so aliases are most likely to appear. Raw Codex IDs remain usable directly.
