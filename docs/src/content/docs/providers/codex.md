---
title: Codex
description: Configure ChatGPT Codex authentication, models, reasoning, tools, images, transports, continuation, compaction, and OpenAI-compatible APIs.
---

Codex is the only upstream backend. It uses the ChatGPT subscription Responses endpoint at `https://chatgpt.com/backend-api/codex/responses`. Claude Code is the client harness; OpenAI-compatible routes are local schema adapters, not OpenAI Platform API access or the native Codex CLI.

OpenAI's Thibault Sottiaux has publicly welcomed using Codex through other coding
harnesses:

> [Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't
> discriminate on the harness.](https://x.com/thsottiaux/status/2075830097488249060)

## Account and authentication

Sign in with a **ChatGPT Plus or Pro account**, not OpenAI API credentials.

```sh
claude-code-proxy codex auth login
# Headless device-code flow
claude-code-proxy codex auth device
claude-code-proxy codex auth status
```

The proxy owns its tokens and does not read native Codex CLI credentials. It refreshes expiring access tokens with a single-flight guard. See [Files and storage](/reference/files-and-storage/) for credential locations.

## Models and fast mode

Use `claude-code-proxy models` as the current catalog. The proxy fetches
`GET /backend-api/codex/models?client_version=0.163.0` directly with its own
ChatGPT OAuth credentials; no installed Codex CLI is needed. Discovery defaults
on, with immutable account/endpoint/version-scoped in-memory snapshots (300s
freshness, 30s failure cooldown, same-owner stale reuse bounded to 900s).
`CCP_CODEX_MODEL_DISCOVERY=0` / `codex.modelDiscovery: false` disables network
discovery and uses built-in/explicit-import fallback metadata.

There are no automatic `~/.codex` / `CODEX_HOME` configuration or model-cache
reads. Only `CCP_CODEX_MODELS_FILE` / `codex.modelsFile` explicitly imports a
metadata file. A current live catalog is authoritative for discovered metadata,
menus, and default choices—not model admission. Well-formed opaque IDs pass
through for upstream validation even when absent from discovery; built-ins are
fallback metadata hints, not an allowlist. Hidden models remain explicitly
requestable. `supported_in_api: false`
is a Platform API-key filter, not a ChatGPT subscription block. Advertising a
model does not guarantee entitlement, availability, or quota; upstream generation
can still reject it.

Claude-style family aliases map Fable to `gpt-6-astra`, Opus to `gpt-6.1-sol`,
Sonnet to `gpt-6-sol`, and Haiku to `gpt-6-luna`, including versioned
`claude-<family>-*` names. Recognized tier/context suffixes survive mapping;
explicit opaque IDs do not change merely because they are absent from discovery.
Upstream rejects unavailable models; incompatible known parameters can fail
locally. Neither substitutes a different model. Use `/model fable`, `/model opus`, or `/model sonnet` inside
Claude Code without exiting or rewriting global settings.

`-fast` requests `service_tier: "priority"`; `-ultrafast` requests `ultrafast`.
Live `service_tiers` IDs determine whether the model supports these accelerated
tiers; not every model does. An advertised default tier does not silently opt
into paid acceleration. On `/v1/messages`, explicit `speed: "fast"` requests
priority for the received model and `speed: "standard"` clears acceleration;
otherwise `CCP_CODEX_SERVICE_TIER` / `codex.serviceTier` overrides suffixes.
Stock Claude Code `/fast` is Opus-only and can switch other families to Opus
before the proxy sees them. Use supported `/model fable-fast`,
`/model sonnet-fast`, or `/model haiku-fast` aliases for other families.
OpenAI-compatible routes use suffixes or the request's `service_tier`.
Unsupported known accelerated tiers fail, not downgrade. See
[Models and routing](/using/models-and-routing/) for full discovery, alias,
fast-mode UI limits, and context rules.

## Reasoning

Claude Code's `/effort` control maps to Codex `reasoning.effort`. The proxy uses the model's advertised supported efforts and default instead of assuming every model accepts the same values. `CCP_CODEX_EFFORT` can force an effort, but incompatible explicit controls fail. Metadata also guides reasoning summaries, verbosity, Responses lane, and context; absent metadata means unknown, not `false`. Native `ultra` is a Codex harness alias normalized to a supported effort, not a literal wire effort.

When reasoning is enabled, the proxy requests an automatic reasoning summary and translates summary deltas into Claude Code thinking blocks. Codex may omit a summary for a simple prompt. `CCP_CODEX_REASONING_SUMMARY=off` suppresses summaries while preserving effort and encrypted continuation content.

Detected auto-review classifier requests default to `low` effort, the effort Codex's own approval reviewer uses, unless the request already sets `output_config.effort`.

Claude Code summary compaction requests are capped at low effort by default because they perform extraction over a large transcript. `CCP_COMPACT_EFFORT=off` disables the cap, `none` removes reasoning, and another valid effort sets a different maximum. The cap never raises effort.

## Tools and multimodal input

- Model metadata selects the normal Responses lane. `CCP_CODEX_FULL_LANE=1`
  or `codex.fullLane: true` still opts legacy `gpt-5.6-sol` and
  `gpt-5.6-terra` into the full lane for parallel tool calls; it does not
  force every other model off Lite. Explicit unsupported parallel calls fail.
- Claude function tools and tool results map to Responses API function calls and outputs.
- With Claude Code's tool search on (`ENABLE_TOOL_SEARCH=true`, see [Configure Claude Code](/using/configure-claude-code/#mcp-tool-search)),
  a deferred tool reaches the model once `ToolSearch` loads it. On the Responses
  Lite lane, the proxy appends each loaded tool as its own `additional_tools` item
  right after the result that loaded it, as Codex's incremental tools do, so
  earlier input is never rewritten and stays in the prompt cache. Deferred tools
  with no load in the visible history, such as tools loaded before a compaction,
  stay in the leading tool list. `tool_reference` blocks render as
  `Tool loaded: <name>`, and Claude Code's `DeferredToolPlaceholder` stub is not
  forwarded.
- Claude Code's forced `web_search_20250305` subrequest uses Codex's standalone
  `/alpha/search` endpoint. It keeps the resolved model, omits search reasoning,
  and preserves non-empty domain filters, so Luna searches do not require a Sol
  Responses turn. Automatic hosted-search requests require the full Responses
  API because the standalone endpoint cannot decide whether to invoke a tool.
  A Lite-only Luna model rejects hosted search rather than silently selecting
  another model; standalone search remains available.
  Structured result DTOs map back to Anthropic `server_tool_use` and
  `web_search_tool_result` blocks, while standalone text output remains text.
  The proxy locally estimates input and output tokens and reports search usage.
  Standalone search sessions use the same ownership identity as continuation:
  Main keeps its Claude Code session ID, while each direct Agent gets a stable,
  opaque owner derived from its session and Agent IDs. The parent Agent ID is
  validation-only. Missing, malformed, or ambiguous identity headers use a fresh
  random search ID instead of sharing state. The search body ID and upstream
  `session_id`, `x-client-request-id`, and `x-codex-window-id` headers all carry
  that same search owner (with the window header's required `:0` suffix).
- Top-level base64 user images map to `input_image`.
- Supported base64 images nested in tool results also map to `input_image`.
- Remote image URLs, malformed images, and unsupported tool-result image forms remain textual placeholders.
- Strict JSON schema output maps to Responses `text.format`.

## Transport and continuation

`auto` is the default transport: it uses WebSocket with HTTP SSE fallback only when setup fails before a request is sent. Set `CCP_CODEX_TRANSPORT=websocket` for WebSocket-only behavior, or `http` for HTTP SSE. A failure after a request is sent does not trigger HTTP replay.

On the HTTP transport, the proxy waits five minutes for the response headers before failing the request. Codex withholds the response head until the model produces its first output, so a large request to a high-effort model can hold it for minutes. `CCP_CODEX_HEADER_TIMEOUT_MS` or `codex.headerTimeoutMs` changes that bound.

### Connection pacing

Fresh WebSocket connections are paced adaptively. While the origin accepts upgrades the
proxy opens connections without spacing them; each rejected upgrade widens the spacing
(1s, then doubling up to 8s), and a run of successful connections narrows it back down.

Because continuation is off by default, every request opens a fresh connection, so a
fixed spacing would cap a single process at roughly one generation per second no matter
how healthy the origin is. Pacing is therefore the price of an observed rejection rather
than a standing tax.

Set `CCP_CODEX_WS_CONNECT_SPACING_MS` (or `codex.websocketConnectSpacingMs`) to impose a
floor the proxy never relaxes below. The default is `0`.

WebSocket setup honors `HTTP_PROXY` for `ws://`, `HTTPS_PROXY` for the default `wss://` endpoint, `ALL_PROXY` as a fallback, and `NO_PROXY` exclusions. A normal HTTP proxy can therefore carry the default WebSocket connection with CONNECT; TUN mode is not required. Set proxy variables before starting the process and restart after changing them. For example, setting `HTTPS_PROXY` to `http://127.0.0.1:7890` sends HTTPS/WSS destinations through the HTTP proxy at port 7890; it does not require an `https://` proxy URL.

`CCP_CODEX_PREVIOUS_RESPONSE_ID=1` enables append-only WebSocket continuation. A valid identity containing only a Claude Code session ID owns the Main continuation for that session. Each valid direct Agent ID owns an independent continuation and reusable WebSocket within the same session. Nested Agents are keyed by their direct child ID; the parent ID is validated but does not become part of the owner key. The proxy sends `previous_response_id` only when the translated request shape and transcript extension are safe, and only on the exact live WebSocket that produced that response.

An absent, malformed, or ambiguous identity does not reject the HTTP request; that request proceeds without continuation or WebSocket reuse. If the originating socket is missing, dead, or has been replaced, the proxy retries once with the full translated input and without the stale response ID. Continuation and connection state is held only in memory and is lost when the proxy restarts.

Detected auto-review classifier subrequests are intentionally stateless even when valid session and Agent headers are present. They neither consume nor publish continuation or WebSocket ownership. Their `prompt_cache_key` is `guardian:<session id>`, the reviewer-scoped key Codex uses for its own approval reviewer, so they do not share the main conversation's cache key.

## Server compaction

Claude Code normally compacts a long conversation by asking the active model to write a portable text summary. Later turns contain that summary instead of the original transcript. A prose summary can flatten details from a long, tool-heavy Codex session.

Codex server compaction preserves the same boundary in a model-native form. Codex returns an opaque encrypted `compaction` item representing the earlier Responses history. On later turns, the proxy gives that item back to Codex together with selected recent messages and everything added after the boundary. Claude Code still receives its normal portable summary, so the session has a safe fallback.

This is most useful for long coding sessions where continuity after `/compact` or automatic compaction matters. It does not increase the model's context window or prevent Claude Code from compacting. The boundary also takes longer because it adds one Codex request.

### How it works

1. Claude Code reaches a manual or automatic compaction boundary.
2. The proxy sends the translated conversation to Codex with a trailing `compaction_trigger`.
3. Codex returns an encrypted `compaction` item, which the proxy keeps in memory for that Claude Code session and model.
4. Claude Code completes its normal summary request. The proxy uses the resulting summary as an exact anchor.
5. On subsequent matching turns, the proxy replaces the portable summary with the encrypted item, retained recent context, and post-compaction messages.

The encrypted item remains opaque to the proxy. It is stored only in memory and sent back to Codex as native Responses input.

### Enable server compaction

Server compaction is disabled by default. Enable it in `config.json`:

```json
{
  "codex": {
    "serverCompaction": true
  }
}
```

Or enable it for one proxy process:

```sh
CCP_CODEX_SERVER_COMPACTION=1 claude-code-proxy serve
```

### Fallbacks and visibility

Replay requires the same Claude Code session and Codex model with append-only history. A branch, proxy restart, model change, malformed response, upstream failure, memory limit, or 30 minutes without matching activity discards the native state and uses Claude Code's portable summary instead.

While the native request is active, the monitor shows `compacting`. Structured log events named `server_compaction_triggered`, `server_compaction_completed`, and `server_compaction_failed` report each attempt and outcome.

## OpenAI-compatible APIs

`CCP_CODEX_RESPONSES_API=1` enables both `POST /v1/responses` and `POST /v1/chat/completions`, backed only by Codex with the proxy's stored ChatGPT OAuth credential.

The Responses route preserves native JSON or SSE response bodies after ID-syntax/capability validation; well-formed model IDs do not need catalog membership. The Chat Completions route translates text, supported `image_url` input parts, reasoning effort, verbosity, service tier, JSON object/Schema output, and buffered or streaming responses; it does not add function-tool support. Omitted effort uses the model's advertised default with a compatibility fallback when metadata is absent; the proxy-wide Codex effort override takes precedence.

The proxy replaces incoming credentials with stored Codex auth for both routes. Response retrieval or deletion, function calling through Chat Completions, and WebSocket ingress are outside their scope. See [HTTP API](/reference/http-api/) for supported Chat Completions fields and error behavior.

## Images API

`CCP_CODEX_IMAGES_API=1` separately enables `POST /v1/images/generations` and `POST /v1/images/edits`. The routes reuse the proxy's stored ChatGPT OAuth session and target the ChatGPT Codex image backend; no OpenAI Platform API key is required.

```sh
CCP_CODEX_IMAGES_API=1 claude-code-proxy serve
```

The model defaults to and is restricted to `gpt-image-2`. Generation accepts JSON. Editing accepts either Codex JSON data URLs or OpenAI-style multipart uploads, which the proxy validates and converts into the Codex JSON contract. Results are returned as `data[].b64_json`. Masks, remote URLs, URL-formatted output, and image variations are not supported.

This is an internal ChatGPT Codex interface rather than the public Platform Images API. It consumes the signed-in account's image quota and can change without public API compatibility guarantees. Image prompts, uploads, generated base64, and upstream error bodies are excluded from traffic captures and persistent error diagnostics.

Because callers are not authenticated, binding to a LAN address lets every firewall-admitted host consume the signed-in account's quota. Restrict the listener to a trusted interface/subnet and never expose it through router forwarding, UPnP, a public tunnel, or permissive IPv6 rules.

See [Configuration](/reference/configuration/) for every Codex setting and [Troubleshooting](/using/troubleshooting/) for auth, model, and transport failures.
