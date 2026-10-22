---
title: Configuration
description: Canonical claude-code-proxy configuration keys, environment variables, defaults, precedence, boolean parsing, and example config.json.
---

Proxy settings come from environment variables or `config.json`. Precedence is **environment variable, config file, built-in default**. The `serve --port` option takes precedence over all port settings.

These settings configure the proxy process. Claude Code client settings such as `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`, and `CLAUDE_CODE_*` are documented separately in [Configure Claude Code](/using/configure-claude-code/).

## Example `config.json`

```json
{
  "bindAddress": "127.0.0.1",
  "port": 18765,
  "codex": {
    "originator": "claude-code-proxy",
    "modelDiscovery": true,
    "baseUrl": "https://chatgpt.com/backend-api/codex/responses",
    "transport": "auto",
    "headerTimeoutMs": 300000,
    "previousResponseId": false,
    "serverCompaction": false,
    "responsesApi": false,
    "imagesApi": false,
    "imagesBaseUrl": "https://chatgpt.com/backend-api/codex"
  },
  "log": {
    "stderr": false,
    "verbose": false
  }
}
```

All keys are optional. An unreadable file, malformed JSON, or incompatible field type causes the file configuration to be ignored and built-in or environment values to apply.

## Core and diagnostics

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_BIND_ADDRESS` | `bindAddress` | `127.0.0.1` | Listener IP address. |
| `PORT` | `port` | `18765` | Listener port. |
| `CCP_CONFIG_DIR` | none | Platform config directory | Replaces the configuration and file-backed auth root. |
| `CCP_AUTO_REVIEW_MODEL` | `autoReviewModel` | `gpt-6-luna` for Codex | Routes Claude Code's non-streaming, tool-free Bash security-review classifier through the selected model. |
| `CCP_LOG_STDERR` | `log.stderr` | `false` | Mirrors logs to stderr when present in the environment, regardless of its value. |
| `CCP_LOG_VERBOSE` | `log.verbose` | `false` | Preserves full string fields in structured logs when present, regardless of its value. |
| `CCP_TRAFFIC_LOG` | none | `false` | Enables full request captures for `1`, `true`, or `yes`. |
| `XDG_STATE_HOME` | none | `~/.local/state` | State base on macOS and Linux. |

`CCP_CONFIG_DIR` affects `config.json` and file-backed provider auth. It does not relocate the state directory.

Codex auto-review classifier requests use `gpt-6-luna` by default. `CCP_AUTO_REVIEW_MODEL` or `autoReviewModel` selects an explicit model for detected classifier requests; well-formed IDs need not appear in discovery. Normal messages, streaming requests, tool-using requests, and token counting retain their requested model. Detected classifier requests also default to `low` effort and use a `guardian:<session id>` prompt cache key, as Codex's own approval reviewer does.

## Outbound proxies

Outbound HTTP requests and Codex WebSocket setup inherit standard proxy environment variables when each provider client is created:

| Environment | Applies to | Purpose |
| --- | --- | --- |
| `HTTP_PROXY` / `http_proxy` | `http://` and `ws://` destinations | Routes plain HTTP and WebSocket Upgrade requests through the configured proxy. |
| `HTTPS_PROXY` / `https_proxy` | `https://` and `wss://` destinations | Routes TLS destinations through the configured proxy, normally with HTTP CONNECT. |
| `ALL_PROXY` / `all_proxy` | Any destination without a scheme-specific proxy | Provides the fallback proxy. |
| `NO_PROXY` / `no_proxy` | Matching destination hosts and IPs | Bypasses proxy routing. Supports comma-separated domains, subdomains, IP addresses, CIDR ranges, and `*`. |

On case-sensitive platforms, uppercase names are checked before lowercase names when both spellings exist. Windows treats environment names as case-insensitive, so each uppercase/lowercase pair identifies one variable. Set these variables before starting claude-code-proxy; changing them requires a restart because clients and pooled WebSocket connections retain their startup route. For CGI safety, proxy environment variables are ignored when `REQUEST_METHOD` is present. The variable name describes the **destination** scheme, so this default-WSS configuration is valid even though the local proxy URL uses `http://`:

| Variable | Value |
| --- | --- |
| `HTTP_PROXY` | `http://127.0.0.1:7890` |
| `HTTPS_PROXY` | `http://127.0.0.1:7890` |

After setting both variables through the operating system, service manager, or shell, start `claude-code-proxy serve` in the same environment.

Proxy URLs may use `http`, `https`, `socks4`, `socks4a`, `socks5`, or `socks5h`. HTTP proxy URLs can contain percent-encoded Basic credentials, for example `http://user:password@127.0.0.1:7890`; SOCKS5 and SOCKS5H URLs can contain username/password credentials for the SOCKS handshake. SOCKS4 and SOCKS4A are supported without URL credentials. Prefer a secret-management mechanism when available because environment variables may be visible to other local processes. WSS certificate verification uses both bundled public roots and the platform native root store, including locally installed enterprise proxy CAs. Malformed or unsupported proxy URLs fail provider startup rather than being ignored. Proxy failures do not silently retry with a direct connection; only `NO_PROXY` selects direct routing. OS proxy settings, PAC files, and WPAD are not read automatically.

## Codex

| Environment | Config key | Default | Purpose |
| --- | --- | --- | --- |
| `CCP_CODEX_MODEL` | `codex.model` | unset | Legacy model pin checked by Anthropic Messages: conflicting selections fail rather than silently replacing the requested model. Remove it for independent main/small selection and `/model` switching. OpenAI-compatible requests preserve their own model IDs. |
| `CCP_CODEX_MODEL_DISCOVERY` | `codex.modelDiscovery` | `true` | Fetches authenticated subscription model metadata. Set `0` / `false` for built-in/explicit-import fallback without discovery network requests. |
| `CCP_CODEX_MODELS_FILE` | `codex.modelsFile` | unset | Explicit optional model metadata import path; no automatic native Codex cache/config reads. |
| `CCP_CODEX_EFFORT` | `codex.effort` | model default | Forces reasoning effort; the selected model must support it. Known values include `none`, `low`, `medium`, `high`, `xhigh`, and `max`; advertised native values are model-dependent. |
| `CCP_COMPACT_EFFORT` | none | `low` | Caps Codex reasoning effort for Claude Code summary compaction requests. `off` disables the cap and `none` removes reasoning. |
| `CCP_CODEX_REASONING_SUMMARY` | `codex.reasoningSummary` | unset | Overrides summary mode. `off` and `none` suppress summaries. |
| `CCP_CODEX_SERVICE_TIER` | `codex.serviceTier` | unset | Explicit `/v1/messages` tier override when the request has no interactive `speed` choice. `fast` means `priority`; accelerated/custom IDs must be supported by the model. Unsupported acceleration errors rather than downgrading. No paid tier is enabled from catalog defaults alone. |
| `CCP_CODEX_BASE_URL` | `codex.baseUrl` | ChatGPT Codex Responses URL | Changes the Codex endpoint. |
| `CCP_CODEX_TRANSPORT` | `codex.transport` | `auto` | Selects `websocket`, `http`, or `auto`. `auto` falls back to HTTP SSE only when WebSocket setup fails before a request is sent. |
| `CCP_CODEX_HEADER_TIMEOUT_MS` | `codex.headerTimeoutMs` | `300000` | Sets how long an HTTP-transport request waits for the Codex response headers. Values below `1000` are ignored. |
| `CCP_CODEX_WS_CONNECT_SPACING_MS` | `codex.websocketConnectSpacingMs` | `0` | Sets the minimum spacing between WebSocket connection starts. Rejected upgrades increase spacing adaptively; sustained successes relax it toward this floor. |
| `CCP_CODEX_PREVIOUS_RESPONSE_ID` | `codex.previousResponseId` | `false` | Enables append-only WebSocket continuation for `1`, `true`, or `yes`. |
| `CCP_CODEX_FULL_LANE` | `codex.fullLane` | `false` | Enables parallel tool calls by using the full Responses lane for `gpt-5.6-sol` and `gpt-5.6-terra`. |
| `CCP_CODEX_SERVER_COMPACTION` | `codex.serverCompaction` | `false` | Enables or disables native compaction for standard boolean words. |
| `CCP_CODEX_RESPONSES_API` | `codex.responsesApi` | `false` | Enables native Codex `/v1/responses` and `/v1/chat/completions` compatibility. Accepts `1`, `true`, or `yes`. |
| `CCP_CODEX_IMAGES_API` | `codex.imagesApi` | `false` | Enables `/v1/images/generations` and `/v1/images/edits` for `1`, `true`, or `yes`. |
| `CCP_CODEX_IMAGES_BASE_URL` | `codex.imagesBaseUrl` | `https://chatgpt.com/backend-api/codex` | Sets the trusted Codex Images API root; production use is restricted to HTTPS `chatgpt.com/backend-api/codex`. |
| `CCP_CODEX_ORIGINATOR` | `codex.originator` | `claude-code-proxy` | Changes the Codex `originator` header. |
| `CCP_CODEX_USER_AGENT` | `codex.userAgent` | `claude-code-proxy/<version>` | Changes the Codex user-agent. |

Discovery uses the proxy-owned ChatGPT OAuth credential and pinned native query `client_version=0.163.0` at the configured Responses endpoint's sibling `/models`. Its in-memory snapshots are account/owner-, endpoint-, and version-scoped: 300-second freshness, 30-second failure cooldown, same-scope stale reuse bounded to 900 seconds. A current live catalog supplies authoritative discovered metadata for menus, default choices, and capabilities, not an admission allowlist. Well-formed opaque IDs pass through for upstream validation even when absent from discovery. Built-ins are temporary fallback metadata hints. Hidden models remain explicitly requestable, and `supported_in_api: false` does not block subscription access. Missing metadata is unknown, not an unsupported capability. Catalog advertising does not guarantee entitlement or quota. See [Models and routing](/using/models-and-routing/) for details.

`codex.modelsFile` / `CCP_CODEX_MODELS_FILE` is an explicit fallback import, not an instruction to read `CODEX_HOME`, `~/.codex`, or native credentials. To opt out of discovery:

```json
{
  "codex": {
    "modelDiscovery": false
  }
}
```

`CLAUDE_CODE_PROXY_CODEX_BASE_URL` remains an accepted fallback for the Codex base URL. `CCP_CODEX_BASE_URL` takes precedence.

## Compatibility fallbacks

`CCP_USER_AGENT` is a fallback when `CCP_CODEX_USER_AGENT` is absent. Prefer `CCP_CODEX_USER_AGENT` in durable configuration.

Endpoint and client identity overrides can break Codex compatibility. They do not enable OpenAI Platform API-key authentication. Use defaults unless a controlled integration or focused diagnostic requires an override.
