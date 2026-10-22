---
title: HTTP API
description: Local routes for health checks, Anthropic Messages, token counts, model discovery, OpenAI-compatible requests, and Codex images.
---

The server exposes Anthropic Messages and optional OpenAI-compatible client routes, all backed by the same proxy-owned ChatGPT OAuth credential and Codex subscription service. OpenAI-compatible schemas do not select the public Platform API or use Platform API keys.

<div class="security-callout">
<strong>No client authentication.</strong> The listener accepts requests without validating `Authorization` or `x-api-key`. Loopback is the default. Protect every non-loopback listener with a firewall or authenticating reverse proxy.
</div>

## `GET /healthz`

Liveness check:

```json
{"ok":true}
```

It does not verify provider credentials or upstream availability.

## `GET /monitor`

Read-only dashboard snapshot, available when monitor collection is enabled. The CLI enables collection in both plain and interactive `serve` modes. Only actual loopback peers are allowed; forwarded IP headers do not grant access. Other peers receive HTTP 403, and disabled collection returns HTTP 404. Responses use `Cache-Control: no-store`.

The JSON envelope contains `version: 1` and a `snapshot` with the proxy start time, sessions, active requests and recent requests. It includes the monitor's display metadata, timing, token usage and computed throughput. It does not include credentials or request/response bodies. Process-local monotonic clocks remain in the service; snapshots contain elapsed durations instead.

The `monitor` CLI polls this endpoint. There is no corresponding mutation or shutdown endpoint. Attaching or disconnecting a viewer does not change proxy lifetime or request accounting.

## `POST /v1/messages`

Accepts an Anthropic Messages request in streaming or non-streaming mode. `POST /v1/messages?beta=true` reaches the same route.

The request `model` selects a registered Codex model or Claude-style alias. The proxy translates supported message content, system prompts, thinking settings, tool definitions, tool choice, tool calls, tool results, images, output configuration, metadata, and streaming behavior to Codex.

Streaming responses use Anthropic SSE events such as `message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, and `message_stop`. Non-streaming requests are accumulated from the provider's stream.

Codex responses carry the quota headers Codex reports for the subscription: `x-codex-plan-type`, `x-codex-active-limit`, `x-codex-credits-*`, and, for every limit family, `-used-percent`, `-window-minutes`, `-reset-at`, `-reset-after-seconds`, `-limit-name`, and `-over-secondary-limit-percent`. Which family a window belongs to and how long that window runs come from the headers themselves: a plan may meter a weekly window as `primary` and no secondary window at all. The values are the ones Codex sent for the request being answered. A response that arrives before any telemetry does carries the previous values, or none.

Unknown models return HTTP 400 with the supported catalog. Missing provider auth returns HTTP 401.

## `POST /v1/messages/count_tokens`

Accepts the same basic Anthropic request shape and returns:

```json
{"input_tokens":1234}
```

Codex tokenizes text locally with `o200k_base` and adds estimates for images, encrypted reasoning, and protocol framing. Counts support Claude Code compaction behavior and are estimates rather than upstream billing values.

## `GET /v1/models`

Returns Anthropic-shaped model discovery:

```json
{
  "data": [
    {
      "type": "model",
      "id": "gpt-6-sol",
      "display_name": "gpt-6-sol (codex)"
    }
  ],
  "has_more": false,
  "first_id": "gpt-6-sol",
  "last_id": "gpt-6-sol"
}
```

An optional `limit` query truncates `data` and sets `has_more`. The route does not expose a pagination cursor.

Claude Code gateway discovery filters IDs according to its own model rules. See [Models and routing](/using/models-and-routing/).

## `POST /v1/chat/completions`

Enable this route with `CCP_CODEX_RESPONSES_API=1` or `codex.responsesApi: true`. The `model` field selects a Codex model. Incoming bearer credentials are replaced by the proxy's stored ChatGPT OAuth credential.

```sh
curl http://127.0.0.1:18765/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-6-sol","messages":[{"role":"user","content":"Hello"}]}'
```

Supports text messages, `reasoning_effort`, `response_format`, `stream_options.include_usage`, `temperature`, `top_p`, and `user`. Reasoning effort defaults to `medium`, and the proxy-wide Codex effort setting takes precedence. Function calls, images, audio, log probabilities, multiple choices, storage, and output token limits are not supported on this compatibility path.

Non-streaming requests return a `chat.completion` object. Streaming requests return `chat.completion.chunk` events followed by `data: [DONE]`. Unsupported non-null fields return `invalid_request_error` with the field named in `error.param`.

## `POST /v1/images/generations`

This route exists only when `CCP_CODEX_IMAGES_API=1` or `codex.imagesApi` is true. It reuses the proxy-owned ChatGPT/Codex OAuth session and forwards a bounded JSON request to the Codex image service:

```json
{
  "prompt": "A paper-cut fox in a moonlit forest",
  "model": "gpt-image-2",
  "background": "auto",
  "quality": "auto",
  "size": "auto"
}
```

`prompt` is required. `model` defaults to and is restricted to `gpt-image-2`; `background`, `quality`, and `size` default to `auto`. Optional `n` must be between 1 and 10. Unknown fields and URL response formats are rejected rather than silently forwarded. Successful responses contain `data[].b64_json`; the proxy never writes generated image data to traffic captures.

## `POST /v1/images/edits`

This route uses the same opt-in gate and accepts either:

- Codex JSON with `images: [{"image_url":"data:image/png;base64,..."}]`; or
- OpenAI-style `multipart/form-data` with one to five repeated `image` or `image[]` files and text fields `prompt`, `model`, `background`, `quality`, `size`, and `n`.

Multipart PNG, JPEG, WebP, and GIF signatures are validated and translated to Codex data URLs. The internal Codex edit contract is JSON, so multipart is an ingress compatibility adapter. Masks, remote image URLs, variations, unsupported fields, and other media types return a 4xx OpenAI error. Request bodies, individual files, aggregate inputs, responses, and concurrency are bounded to protect the proxy process.

The Images API is an internal ChatGPT Codex integration, not the public OpenAI Platform Images API. It consumes the signed-in ChatGPT account's entitlement and quota, and the internal contract can change independently of the public API.

## `POST /v1/responses`

Enable this route with `CCP_CODEX_RESPONSES_API=1` or `codex.responsesApi: true`. It forwards native Codex Responses requests and preserves JSON or SSE response bodies.

```sh
curl http://127.0.0.1:18765/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-6-sol","input":"Hello"}'
```

`parallel_tool_calls` is forwarded unchanged. Chat Completions rejects this field because that path does not support function tools. Anthropic Messages can express the corresponding restriction through `tool_choice.disable_parallel_tool_use`.

Stored response retrieval, deletion, and WebSocket client connections are not supported. This is a ChatGPT Codex endpoint adapter, not a full implementation of the public OpenAI Responses service.

## Model selection and errors

Both OpenAI-compatible routes strip a trailing `[1m]` and resolve Claude-style aliases to Codex. Unknown models return HTTP 400 with the supported catalog. Incoming authorization is not an upstream credential.

Authentication, permission, rate-limit, invalid-request, and upstream failures use the OpenAI error format. Rate-limit responses retain `Retry-After` when supplied.

The monitor and logs show the selected Codex model. Message/Responses traffic capture includes prompts and tool content, so treat captures as sensitive.

## Other routes

Unmatched paths return the proxy's not-found response. The server has no administrative mutation API, credential API, or remote shutdown route.
