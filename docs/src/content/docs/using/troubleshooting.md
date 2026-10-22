---
title: Troubleshooting
description: Diagnose listener, ChatGPT authentication, Codex model selection, streaming, context, transport, rate-limit, and diagnostic problems.
---

## Proxy is unreachable

Check the process and liveness route:

```sh
curl http://127.0.0.1:18765/healthz
```

A healthy listener returns `{"ok":true}`. Confirm `ANTHROPIC_BASE_URL` uses the same address and port. `claude-code-proxy serve --port 11435` and `PORT=11435` change the listener port. `--port` wins for that command.

## Authentication error

Inspect the proxy's ChatGPT OAuth credential:

```sh
claude-code-proxy codex auth status
claude-code-proxy codex auth login
```

For a headless host, use `codex auth device`. Codex requires ChatGPT subscription auth, not a Platform API key. Native Codex CLI login is independent of the proxy's credentials.

## Model returns HTTP 400

Run:

```sh
claude-code-proxy models
```

Malformed IDs and incompatible known effort/tier/capability parameters fail locally. Well-formed opaque IDs are forwarded even when absent from discovery; upstream decides availability, entitlement, region, and quota. The catalog supplies menus/defaults/metadata, not an admission allowlist. Fable maps to Astra, Opus to GPT-6.1 Sol, Sonnet to GPT-6 Sol, and Haiku to Luna. Errors never cause substitution with another model.

If `/model` appears not to change the upstream model, remove conflicting `CCP_CODEX_MODEL` / `codex.model` forced overrides from the proxy before launch, and restart the proxy after changing its configuration. No global Claude settings rewrite is needed.

## `/fast` switches to Opus or fails client checks

Stock Claude Code currently restricts `/fast` to Opus and can change the selected family before making a request. The proxy does not guess the previous family or reverse that client choice. Use supported `/model fable-fast`, `/model sonnet-fast`, or `/model haiku-fast` aliases for other families. Messages `speed: "fast"` requests priority for the actual received model, not a model substitution.

There is no documented all-family gateway fast-mode flag. Codex OAuth, tier support, entitlement, and quota still apply. See [fast-mode limits](/using/models-and-routing/#claude-code-fast-is-opus-only).

## Model discovery is unavailable

The proxy fetches the catalog directly using its own OAuth login; native Codex CLI login or installation will not repair it. Check `codex auth status`, endpoint reachability, and whether `CCP_CODEX_MODEL_DISCOVERY=0` / `codex.modelDiscovery: false` explicitly disabled discovery. Failures use a bounded same-account cache, then built-in/explicit-import fallback. Use `CCP_CODEX_MODELS_FILE` / `codex.modelsFile` only for an intentional metadata import; `~/.codex` and `CODEX_HOME` are not automatically read.

## Background requests fail

Set `ANTHROPIC_SMALL_FAST_MODEL` to a concrete routable ID. Claude Code sends title and small background tasks through that model independently of the main model.

## A tool runs twice

Set `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` for Claude Code. Retrying a partially completed stream as non-streaming can duplicate tool calls.

## Session reaches context limits

`[1m]` cannot enlarge upstream context, and a larger `max_context_window` is not the active subscription window. Claude Code controls client compaction independently of the proxy's model metadata. Use `/compact` early when approaching the upstream limit or switching to a smaller model.

## Codex WebSocket fails

For a non-TUN local HTTP proxy, set both destination-scheme variables before starting the proxy:

| Variable | Value |
| --- | --- |
| `HTTP_PROXY` | `http://127.0.0.1:7890` |
| `HTTPS_PROXY` | `http://127.0.0.1:7890` |

Set them through the operating system, service manager, or shell, then start `claude-code-proxy serve` in the same environment.

The default `wss://chatgpt.com` connection uses `HTTPS_PROXY`; a working proxy should show `CONNECT chatgpt.com:443`. The `http://` value is normal: it describes how to reach the proxy, while `HTTPS_PROXY` describes which destinations use it. Restart claude-code-proxy after changing these variables because the client and pooled WebSocket route are created at startup.

Check `NO_PROXY` when the proxy sees no request. Proxy connection, authentication, or CONNECT failure is returned as an error and never retried directly. Environment variables are supported; OS proxy settings and PAC/WPAD discovery are not automatic.

Use HTTP SSE to isolate transport behavior:

```sh
CCP_CODEX_TRANSPORT=http claude-code-proxy serve
```

`auto` falls back only when WebSocket setup fails before sending the request. It does not replay an in-flight request.

## Rate limited

Codex limits are shared with other clients on the same ChatGPT account. Limit responses surface as HTTP 429 with `retry-after`. Wait for the indicated interval or reduce concurrent traffic.

## Find the complete error

The monitor shows request detail. `proxy.log` contains structured JSONL events, and `errors/` stores complete redacted failed-response payloads. A `request_failed` event includes the error file path.

For a source checkout, run the isolated helper:

```sh
./scripts/debug-proxy
```

It prints a random local URL, a sourceable `client.env`, and artifact paths. Keep the terminal open, reproduce once, then press Ctrl-C. Captures contain sensitive prompt and tool content.

## Focused traffic capture

```sh
CCP_LOG_VERBOSE=1 \
CCP_TRAFFIC_LOG=1 \
  claude-code-proxy serve --no-monitor
```

Traffic capture writes the inbound request, translated upstream request, upstream headers and events, and downstream events in emission order. Known credentials are redacted, but prompts and tool content are preserved. Disable capture and delete artifacts after diagnosis.
