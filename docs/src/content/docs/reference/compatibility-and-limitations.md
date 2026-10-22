---
title: Compatibility and limitations
description: Security, ChatGPT account, API schema, context, tool, session, and deployment boundaries.
---

Codex through a ChatGPT subscription is the only upstream backend. Claude Code is the client harness; Anthropic and OpenAI API compatibility describe local schemas, not access to those providers' Platform APIs or native CLI features.

## Account and listener security

- Model access, regions, quotas, terms, and enforcement remain under OpenAI's control. Limits are shared with other clients on the same account.
- OpenAI has publicly welcomed Codex in other harnesses, but this does not guarantee future policy or account treatment.
- The proxy owns its ChatGPT OAuth tokens. It does not use Platform API keys or native Codex CLI credentials.
- Incoming clients are not authenticated. Keep the default `127.0.0.1` listener or protect it with an external firewall or authenticating reverse proxy. An exposed listener can consume account quota.

## Anthropic Messages

- Supports Claude Code's practical streaming and non-streaming usage, not complete Anthropic protocol equivalence. `?beta=true` uses the same implementation.
- Function tools and results translate to Codex; Claude Code executes its tools. Native Codex CLI agent tools are not added by the proxy.
- Base64 user images and supported base64 tool-result images map to Responses images. Remote URLs and malformed or unsupported nested images remain text placeholders.
- Reasoning summaries can become thinking blocks; Codex may omit a summary. Encrypted reasoning and compaction items remain opaque continuation data, not exposed chain of thought.
- Hosted web search supports mapped domain filters. Anthropic `max_uses` is not enforced because Codex exposes no equivalent limit.
- Strict JSON schema output translates; Anthropic-only fields without a mapping can be omitted.
- Token counts are local estimates. Background requests consume subscription tokens.

## Optional OpenAI-compatible routes

- `CCP_CODEX_RESPONSES_API=1` enables native Codex `/v1/responses` passthrough and `/v1/chat/completions` compatibility.
- Chat Completions supports text and model-supported `image_url` input parts, reasoning effort, verbosity, service tier, and output formats. Explicit parallel-call intent is validated, but function tools/calls are not supported. Output token limits, audio, log probabilities, and multiple choices remain unsupported.
- Responses forwards native Codex input and JSON or SSE output. It does not add stored response retrieval/deletion or WebSocket ingress.
- Unsupported non-null compatibility fields return errors rather than being silently ignored. These routes do not promise full public OpenAI API equivalence.

## Images

`CCP_CODEX_IMAGES_API=1` separately enables generation and JSON or multipart edits, restricted to `gpt-image-2` and base64 output. Masks, remote URLs, variations, and URL output are unsupported. This internal ChatGPT Codex interface consumes the account's image quota and can change independently of the public Platform Images API.

## Models, context, and session state

- Catalog advertising does not guarantee account entitlement or quota. Live subscription metadata guides menus, defaults, and capabilities—not admission. Well-formed opaque model IDs pass through for upstream validation even when absent from discovery. Hidden entries remain explicitly requestable; `supported_in_api: false` does not block subscription use.
- Discovery defaults on, using proxy-owned OAuth directly; no Codex CLI installation or automatic `~/.codex` / `CODEX_HOME` reads. Offline discovery opt-out and explicit metadata imports are documented in [Models and routing](/using/models-and-routing/).
- Family aliases map Fable to Astra, Opus to GPT-6.1 Sol, Sonnet to GPT-6 Sol, and Haiku to Luna. There is no hardcoded/catalog admission allowlist or silent model substitution; unknown IDs are validated upstream, while malformed IDs and incompatible known controls fail. `/model` switching does not require exit or global settings rewrites.
- Stock Claude Code `/fast` is Opus-only and may change the model before the proxy sees it. Messages `speed: "fast"` requests priority for the received model; other families can use supported `-fast` aliases without relying on that UI. Client fast-mode check bypasses do not grant upstream entitlement.
- Missing capability metadata is unknown, not `false`. Actual advertised service-tier IDs determine acceleration; default-tier metadata does not opt into paid service automatically.
- `[1m]` is a client hint, not a context increase. The launcher uses active context and metadata safety headroom; a 272,000-token active window has at most a 244,800 compaction window ceiling (actual compaction may occur earlier).
- Optional WebSocket continuation and server compaction state live in memory and are invalidated when their model or history assumptions change. Restarts clear them; portable Claude Code history remains the fallback.
- Automatic transport fallback happens only before a request is sent, avoiding replay of possible side effects.

## Diagnostics and privacy

Logs redact known credential keys, but user strings may still contain secrets. Error captures preserve redacted failed responses. Message/Responses traffic captures intentionally preserve prompts and tool content; use them only for focused local diagnosis. Image routes do not capture or persist prompts, uploads, generated data, or upstream error bodies.

See [Codex behavior](/providers/codex/), the [HTTP API](/reference/http-api/), and the historical [Changelog](/reference/changelog/).
