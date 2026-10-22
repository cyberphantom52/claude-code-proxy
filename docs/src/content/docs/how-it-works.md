---
title: How it works
description: Follow ChatGPT authentication, Codex model selection, protocol translation, streaming, and session state.
---

claude-code-proxy has one upstream backend: the ChatGPT subscription Codex service. Anthropic Messages and the optional OpenAI-compatible routes are client-facing schemas, not separate providers or OpenAI Platform API access.

## Anthropic requests

1. Claude Code sends `/v1/messages` to the local proxy.
2. The proxy pins its own ChatGPT OAuth identity and prepares a model snapshot from authenticated discovery or bounded same-owner cache/fallback. No installed Codex CLI or automatic native config/cache reads are involved.
3. The shared resolver strips a trailing `[1m]` and resolves family aliases: Fable → Astra, Opus → GPT-6.1 Sol, Sonnet → GPT-6 Sol, Haiku → Luna. Well-formed opaque model IDs pass through unchanged even when absent from discovery; known capabilities are validated without substituting another model, while upstream decides model availability and access.
4. The translator maps messages, images, tools, tool results, thinking controls, and supported output settings into Codex Responses input, using model metadata for safe defaults.
5. Codex text, reasoning-summary, tool, usage, and completion events become Anthropic SSE events or an accumulated non-streaming response.
6. The monitor, JSONL logger, and optional traffic capture record diagnostics.

Claude Code executes its client tools. Using Codex upstream does not turn the proxy into the native Codex CLI or add its local agent tools.

## Optional routes

`CCP_CODEX_RESPONSES_API=1` enables native Codex `/v1/responses` passthrough and `/v1/chat/completions` compatibility. `CCP_CODEX_IMAGES_API=1` separately enables image generation and editing. All use the proxy's ChatGPT login, not incoming API keys. See [HTTP API](/reference/http-api/) for accepted fields and limits.

## Authentication boundary

The proxy owns its OAuth tokens and does not read native Codex CLI credentials. [Files and storage](/reference/files-and-storage/) describes storage. Incoming `ANTHROPIC_AUTH_TOKEN`, bearer tokens, and API-key headers do not supply upstream credentials or authenticate callers. Keep the listener on loopback or add external access control.

## Session state

Claude Code session and Agent identity scope optional Codex WebSocket continuation. Optional server compaction also holds native state in memory. A restart clears this state; Claude Code's portable history remains the fallback. See [Codex](/providers/codex/) for identity validation, replay safety, transport fallback, and compaction boundaries.

## Count tokens

`POST /v1/messages/count_tokens` stays local. It uses `o200k_base` for text plus estimates for images, encrypted reasoning, and protocol framing. This supports Claude Code's compaction decisions, not upstream billing.
