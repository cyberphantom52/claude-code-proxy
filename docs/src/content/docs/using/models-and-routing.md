---
title: Models and routing
description: Discover subscription Codex models and select family aliases and supported service tiers.
---

All model requests use Codex. Claude-style names are compatibility aliases, not access to Anthropic models.

## Discover models

```sh
claude-code-proxy models
curl http://127.0.0.1:18765/v1/models
```

Discovery is enabled by default. The proxy uses its own ChatGPT OAuth login to fetch `GET https://chatgpt.com/backend-api/codex/models?client_version=0.163.0`. The version is the pinned Codex protocol version, not the proxy's package version. With a custom Codex Responses endpoint, discovery uses its sibling `/models` endpoint.

No installed Codex CLI is required. The proxy does not automatically read `~/.codex`, `CODEX_HOME`, native Codex configuration, or native credentials. Built-in models provide an offline fallback. Import a model metadata file only by explicitly setting `CCP_CODEX_MODELS_FILE` or `codex.modelsFile`.

To disable network discovery, set `CCP_CODEX_MODEL_DISCOVERY=0` on the proxy, or `codex.modelDiscovery: false` in its configuration. This uses the built-in/explicit-import fallback; generation still needs ChatGPT authentication and network access.

The bounded in-memory cache holds immutable snapshots scoped to the authenticated account/owner, endpoint, and protocol version. Entries are fresh for 300 seconds; failed refreshes have a 30-second cooldown. A stale snapshot can be reused only for that same owner/scope and only within 900 seconds of its last validation. Otherwise the proxy uses the fallback, never another account's catalog. There is no background polling.

The current live catalog is authoritative for discovered metadata, menus, and automatic defaults, **not model admission**. There is no hardcoded or catalog-membership allowlist: a well-formed opaque model ID passes through unchanged for upstream validation, even if absent from discovery. Built-in entries are temporary fallback metadata hints, not an access policy. `visibility: "hide"` hides a returned model from discovery, not explicit requests. `supported_in_api: false` describes Platform API-key support and does **not** block ChatGPT-subscription use. Catalog presence is not a guarantee of entitlement, regional availability, or remaining quota; upstream generation can still reject a request.

## Claude-style aliases

| Alias family | Codex target |
| --- | --- |
| `fable*`, `claude-fable-*` | `gpt-6-astra` |
| `opus*`, `claude-opus-*` | `gpt-6.1-sol` |
| `sonnet*`, `claude-sonnet-*` | `gpt-6-sol` |
| `haiku*`, `claude-haiku-*` | `gpt-6-luna` |

Family resolution includes versioned names such as `claude-opus-5-5`, and preserves recognized service-tier and context suffixes. For example, `fable-fast` requests Astra's priority tier if supported. Explicit opaque model IDs are not remapped merely because they are absent from discovery. Unknown or unavailable models are validated upstream; incompatible known parameters can fail locally. Neither case causes substitution with another model.

## Service tiers and model capabilities

Exact discovered slugs take precedence over suffix interpretation. Local `-fast` / `-ultrafast` aliases apply to known model bases (and mapped family aliases); an opaque unknown ID ending in `-fast` is forwarded unchanged, not guessed to be another model. Known incompatible tier requests produce a helpful error.

`-fast` requests the actual `priority` service tier; `-ultrafast` requests `ultrafast`. Live `service_tiers` IDs determine which accelerated forms are supported: not every model offers either tier. Built-in fallback knowledge applies only when live metadata is unavailable. A catalog's `default_service_tier` does not automatically opt you into paid acceleration: request a tier explicitly.

On `/v1/messages`, explicit `speed: "fast"` requests priority for the current resolved model; `speed: "standard"` clears acceleration. This interactive choice takes precedence over `codex.serviceTier` / `CCP_CODEX_SERVICE_TIER`, which otherwise override suffixes. OpenAI-compatible routes use the suffix or the request's own `service_tier`. An unsupported requested accelerated tier fails rather than silently becoming another tier or model.

### Claude Code `/fast` is Opus-only

Stock Claude Code currently supports `/fast` only on Opus and may switch another selected family to Opus **before the proxy receives the request**. The proxy does not heuristically undo that client-side model change. For Astra, Sol, or Luna priority service without the Opus switch, use supported tier aliases instead:

```text
/model fable-fast
/model sonnet-fast
/model haiku-fast
```

The proxy's `speed` mapping preserves whatever model the client actually sends; it does not make stock `/fast` an all-family control. There is no documented gateway flag that enables fast mode for every family. Client availability checks do not bypass Codex OAuth, model/tier support, entitlement, billing, or quota. See Claude Code's [fast mode documentation](https://code.claude.com/docs/en/fast-mode).

The proxy uses model metadata for default effort, reasoning summaries, verbosity, Responses lane, and context limits. Explicit incompatible controls fail. Missing metadata means unknown, not unsupported; it retains the compatibility fallback rather than treating an absent flag as `false`. Discovery supplies model metadata only: it does not fetch or execute prompts or take over Claude Code's tools.

## Context and small models

`ANTHROPIC_MODEL` selects the main model and `ANTHROPIC_SMALL_FAST_MODEL` independently selects the background model when starting `claude`; see [Configure Claude Code](/using/configure-claude-code/).

`[1m]` is only a Claude Code compatibility hint, stripped before upstream matching. It does not enlarge Codex capacity. Claude Code's client compaction is independent of upstream metadata; a larger advertised `max_context_window` does not establish the active subscription window. If you switch to a smaller model during a session, compact early with `/compact`.

## Switching and gateway discovery

Within Claude Code, use `/model fable`, `/model opus`, `/model sonnet`, or an explicit Codex ID. No exit or global `settings.json` rewrite is needed. See [Switching models](/using/switching-models/) for examples and forced-model override conflicts.

`/v1/models` includes Codex IDs and Claude-style aliases, but picker enumeration depends on the client build and its own filters; it is not a guarantee that every raw Codex ID appears. Typed `/model` aliases and Codex IDs work independently of the picker. Client gateway discovery is separate from the proxy's default-on upstream catalog discovery.
