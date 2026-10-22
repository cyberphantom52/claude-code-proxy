---
title: Files and storage
description: Configuration, ChatGPT OAuth credential, log, error, traffic-capture, and service-log paths on macOS, Linux, and Windows.
---

claude-code-proxy separates configuration and credentials from runtime state.

## Directory roots

| Platform | Configuration root | State root |
| --- | --- | --- |
| macOS | `~/.config/claude-code-proxy` | `${XDG_STATE_HOME:-~/.local/state}/claude-code-proxy` |
| Linux | `${XDG_CONFIG_HOME:-~/.config}/claude-code-proxy` | `${XDG_STATE_HOME:-~/.local/state}/claude-code-proxy` |
| Windows | `%APPDATA%/claude-code-proxy` | `%LOCALAPPDATA%/claude-code-proxy` |

Windows falls back to `%USERPROFILE%/AppData/Roaming` and `%USERPROFILE%/AppData/Local` when the corresponding environment variable is absent.

`CCP_CONFIG_DIR` replaces the configuration root for the current process. It does not change the state root.

## Configuration

`config.json` lives directly under the configuration root. See [Configuration](/reference/configuration/) for its schema and precedence.

## ChatGPT OAuth credentials

On macOS, Codex uses the Keychain service `claude-code-proxy.codex`.
On Linux and Windows, credentials use `<configuration-root>/codex/auth.json`, written with restrictive permissions where supported.

When `CCP_CONFIG_DIR` is set, credentials use `<CCP_CONFIG_DIR>/codex/auth.json` on every platform, including macOS.

The proxy owns these tokens independently of native Codex CLI credentials. No OpenAI Platform API key is used.

Credential lifecycle coordination also uses `<configuration-root>/codex/auth.json.lock` and `auth.json.revision`, including when the tokens themselves are in macOS Keychain. The lock inode is stable; the revision stores a durable generation/logout tombstone, not token contents. Keep these files with the credential store, not with disposable logs/caches, and do not remove them while proxy/auth processes are running. The built-in store needs a writable coordination directory; unreadable or corrupt state fails closed.

Cooperating processes use atomic snapshots and conditional updates so an old refresh cannot overwrite a newer login or undo logout, including identical-credential relogin. A rejected refresh does not erase credentials while a successful peer may still be rotating them. External programs directly editing the raw file/Keychain without this coordination are outside that guarantee. Existing credential JSON and legacy migration formats remain readable.

## Model metadata

Default-on discovery fetches subscription model metadata directly using the proxy's OAuth login. Its immutable cache is bounded and in memory, scoped to account/owner, endpoint, and protocol version; it is not a native Codex disk cache. Freshness is 300 seconds, failure cooldown 30 seconds, and same-scope stale reuse is bounded to 900 seconds after validation.

The proxy does not automatically read `~/.codex`, `CODEX_HOME`, native configuration, or `models_cache.json`. Only `CCP_CODEX_MODELS_FILE` / `codex.modelsFile` explicitly imports an optional metadata file for the built-in fallback. `CCP_CODEX_MODEL_DISCOVERY=0` / `codex.modelDiscovery: false` disables discovery network requests. See [Models and routing](/using/models-and-routing/).

## Structured log

`proxy.log` lives under the state root. It uses JSON Lines and rotates at 20 MiB. Known credential keys, including authorization, access tokens, refresh tokens, ID tokens, and account headers, are redacted before writing.

A Homebrew service also writes `service.log` under the state root.

## Failed responses

`errors/` under the state root contains JSON files for failed proxy responses. A `request_failed` log event includes `errorFile`, which points to the complete redacted payload.

Error payloads are safer to share than raw traffic captures, but inspect their prompt-derived content and paths before publishing them.

## Traffic captures

Set `CCP_TRAFFIC_LOG=1` to create captures under `traffic/` in the state root. Requests are grouped by Claude Code session and request sequence. A request directory can include:

- inbound Anthropic request and metadata
- translated upstream request
- upstream URL and redacted headers
- raw or decoded upstream events
- translated downstream events and response
- transport or reducer error details

Event filenames use monotonic sequence numbers so lexical order preserves emission order.

<div class="security-callout">
<strong>Traffic captures preserve content.</strong> Header and token redaction does not remove prompts, source code, tool definitions, tool inputs, tool results, images, or provider output. Treat the entire directory as sensitive user data.
</div>

Enable capture only for a focused reproduction, keep the directory local, and delete it after the investigation.
