# Codex integration review

Reviewed 2026-10-22 against `f0df085` on `chatgpt-codex-improvements`.

## Product boundary

The only upstream backend should be **Codex authenticated by a ChatGPT subscription**. Keep proxy-owned browser/device OAuth and refresh; do not add OpenAI Platform API-key authentication. Anthropic Messages is the Claude Code client protocol, not another upstream backend. OpenAI-shaped Responses, Chat Completions, Images and Transcriptions remain compatible ingress APIs backed by the same subscription.

Remove Kimi, Grok, Cursor Agent and OpenCode Go, their configuration/CLI/model routing, and their otherwise-unused adapters/dependencies. Do not delete users' existing credential files. Preserve Claude aliases, Codex model catalog discovery, reasoning replay, search, strict agent ownership, diagnostics and safe transport fallback.

## What “native” should mean

1. Setup needs one subscription login and a reliable launch command, not a collection of undocumented environment flags.
2. Tool calls, arguments, ordering, errors and termination have identical semantics in streaming and buffered modes. A malformed stream must never look like a successful tool turn.
3. Main conversations, sibling/nested agents, subscription accounts and upstream endpoints never share the wrong continuation or socket.
4. Long sessions retain model-native reasoning and compaction state when safe, with portable-history fallback when not.
5. Model capabilities determine context budgets, effort, service tiers and tool behavior; accepting a model ID is not enough.
6. Latency, cache reuse, quota and recovery are measurable. Reconnects must not replay committed actions.

The proxy should remain a protocol adapter. Claude Code must continue owning tool execution, permissions, sandboxing, hooks and agent orchestration. Reimplementing the Codex CLI inside the proxy would defeat that boundary.

## Existing strengths to preserve

- Actual ChatGPT Codex subscription endpoint and proxy-owned OAuth; no supported Codex API-key fallback.
- HTTP SSE and WebSocket transports, proxy support, adaptive connection pacing and fallback only before a request is sent.
- Conservative socket-provenance and append-only checks for `previous_response_id`, typed main/agent identities and stale-turn fencing.
- Opaque encrypted reasoning replay and optional server compaction with exact summary anchoring, bounded in-memory state and portable fallback.
- Incremental deferred-tool schemas that avoid rewriting earlier Lite input and invalidating prompt-cache prefixes.
- Native search and image paths, subscription quota telemetry, request monitoring and substantial local mock-server coverage.

## Findings: fix before enabling more native features by default

These are static code-path findings, not live ChatGPT reproductions. References identify the reviewed snapshot; lines can move during consolidation. This review records follow-up work, not fixes already applied.

### 1. High: complete and validate tools before reporting success

`src/providers/codex/translate/live_stream.rs:1124-1148,1181-1207` closes open tool blocks on `response.completed` without requiring final, validated arguments. The buffered reducer rejects open blocks at `translate/reducer.rs:869-885`. An upstream stream ending after a partial function-argument delta can therefore look successful to the streaming client.

The Read whitespace-recovery path at `live_stream.rs:711-737` terminates the entire response while sibling parallel calls may still be open; the buffered counterpart at `reducer.rs:694-714` has the same early-return hazard. Buffered mode also drops done-only function calls at `reducer.rs:792-796`, despite live support, and malformed accumulated arguments can become string-valued tool input at `translate/accumulate.rs:250-260`.

**Smallest useful fix:** reuse the existing completed-function validation for both paths and enforce one terminal tool contract. Feed the same event fixtures through live translation, buffered reduction and reconstructed Anthropic SSE. Include done-only calls, unfinished JSON, final-argument mismatches and a repaired Read alongside another open tool.

### 2. High: bind reusable sockets and continuation to subscription identity

When optional continuation/WebSocket reuse is enabled, `src/providers/codex/websocket.rs:311-338` stores pooled sockets by conversation identity only, without endpoint/account/credential generation. Pool acquisition at `:856-901` can reuse a socket authenticated as account A after durable credentials change to account B. Ping/Pong validates liveness, not account ownership.

**Smallest useful fix:** record endpoint and a nonsecret auth-generation fingerprint with the socket; discard mismatched state before sending and fall back to full translated history. Test switching account and endpoint while keeping the same conversation owner. Do not enable continuation by default until this is covered.

### 3. High: prevent refresh from resurrecting logout or replacing a new login

`src/providers/codex/auth/manager.rs:147-156` unconditionally saves a successful refresh. If account A is refreshing while another process logs out or logs into B, A's successful refresh can overwrite the newer storage state. The changed-storage protection exists for an unauthorized refresh response, not the success branch.

**Smallest useful fix:** use conditional credential replacement with cross-process coordination and logout generation/tombstone semantics. A read immediately before an unconditional write does not close the race. Test successful refresh racing logout and account replacement.

### 4. High: reject unsupported input instead of silently losing it

`src/providers/translate_shared.rs:93-108,147-230` silently discards unknown top-level content blocks; the Codex request translator consumes that normalization at `src/providers/codex/translate/request.rs:1009-1062`. A document/PDF attachment can disappear while the model still answers the user's question.

**Smallest useful fix:** validate supported block types at the request boundary and return an actionable message/block-index error. Implement native document mapping only when a real supported upstream contract is established. Never claim to have read an omitted attachment.

### 5. Medium: bound streaming buffers and avoid unconditional transcript copies

`src/providers/codex/websocket.rs:2300-2301,2388-2393` retains the full SSE-equivalent transcript even when no traffic capture is enabled. Pending presemantic WebSocket events and buffered HTTP responses also lack aggregate caps. Codex Chat buffering has unbounded pending SSE/text/body storage at `chat_completions/stream.rs:134-152`, `response.rs:68-74` and `mod.rs:106-145`. Generic live tool-argument accumulation lacks the buffered path's early size check.

**Smallest useful fix:** do not build disabled capture output; reuse existing HTTP pending-event/frame budgets for the corresponding WebSocket and Chat buffers. Bound both individual frames and aggregate buffered responses, including actively arriving malformed streams that never trigger an idle timeout.

### 6. Medium: preserve requested reasoning and OpenAI usage semantics

`src/providers/codex/chat_completions/request.rs:136-140` removes explicit `reasoning_effort: "none"`, allowing the upstream default instead. `chat_completions/stream.rs:214-233` attaches usage to a choices-bearing finish chunk rather than emitting the separate `choices: []` usage chunk OpenAI clients expect. Completion metadata can also change when terminal metadata arrives after the first chunk.

**Smallest useful fix:** serialize explicit `effort: "none"`; freeze emitted completion identity; test the exact chunk/usage contract with a standard OpenAI client. Chat is currently deliberately text-only: reject unsupported tools honestly rather than advertising agent-client parity.

### 7. Medium: make login/retry deadlines real

- Browser token exchange uses an unbounded blocking client at `src/providers/codex/auth/pkce.rs:79-102`; the outer login deadline cannot interrupt it.
- Device auth sleeps the issuer's interval without capping it to remaining login time at `auth/device.rs:165-204`; an enormous interval can outlive the five-minute deadline by hours.
- `src/retry.rs:19-39` accepts only numeric `Retry-After`, not the valid HTTP-date form. Its described jitter is deterministic and does not spread concurrent retries.

**Smallest useful fix:** use explicit remaining-deadline bounds, saturating interval arithmetic and both Retry-After forms. Add stalled exchange, oversized interval and future/past HTTP-date tests. Keep recovery budgets and the no-replay-after-commit rule.

### 8. Medium: isolate quota and outcome accounting

`src/providers/codex/rate_limits.rs:37-38,95-113` holds one global snapshot; `codex/mod.rs:581-592` stamps it onto responses. An account change, telemetry-free response or interleaved completion can advertise the wrong request/account's quota. `src/providers/codex/native.rs:490-507` also misses nested terminal failures already understood by `events.rs`.

**Smallest useful fix:** retain request-local telemetry and optional last-known snapshots keyed by account generation. Reuse the shared terminal-event classifier for monitoring. Do not broaden telemetry allowlists to include arbitrary private turn-state headers.

## Native-quality enhancements after correctness

### Model-aware defaults and context policy

`src/providers/codex/translate/model_catalog.rs:23-49` primarily consumes model IDs, visibility/API support and Lite routing. Supported efforts, context budgets and tier/tool capabilities are not generally consumed; tier support remains hard-coded in `model_allowlist.rs:49-65,113-120`.

Read only the metadata needed for actual validation and launch policy. Prefer authenticated upstream model discovery, with built-in fallback and the existing local catalog as optional metadata, so using the proxy does not require installing Codex CLI. Native Codex fetches `GET models?client_version=…` and caches identity/version-aware results; consume supported efforts, active context and tool/tier capabilities rather than accepting IDs alone.

Provide one explicit launch/setup command that selects accessible main/small models, enables deferred tool search and configures a conservative model-specific compaction threshold. Preserve opt-in deployment rather than silently modifying global Claude settings. A `[1m]` suffix does not increase the subscription's context limit. Fail clearly on incompatible requested capabilities; do not silently change the user's model. Native Codex's current default compaction policy is 90% of the resolved window (244,800 for 272,000), not waiting until the absolute model maximum; this is a useful benchmark, not a guaranteed ChatGPT entitlement.

### Keep alias routing and translation in sync

The versioned Claude alias lists are duplicated in `src/registry.rs:7-21` (post-cutover) and `src/providers/codex/translate/model_allowlist.rs:27-41`. They match only registered names, not wildcard families. For example, `claude-fable-5-1` is currently rejected despite the earlier documentation's `claude-fable-*` wording. The docs now state this exact-name boundary.

Use one alias definition for routing and translation, with tests for the actual model IDs emitted by supported Claude Code releases. Keep short aliases and explicit Codex IDs working; don't hide future client incompatibility behind an undocumented fallback.

### Safe continuation and compaction rollout

After the isolation and tool-validation tests pass, measure continuation hit/miss reasons, bytes replayed, cached-input ratio, first-token latency, reconnects and recovered failures. Compare stateless/full-history versus safe socket reuse on the same workloads before considering defaults. Keep existing owner/socket/append-only guards; don't relax them to manufacture hit rates.

Server compaction should remain a bounded optimization over portable Claude history. Fix retained image budgeting first: `src/providers/codex/compaction.rs:460-480` retains images without debiting the token budget even though estimation charges each image 2,000 tokens. Do not add unbounded disk persistence as a shortcut to resume support.

Native Codex also replays the opaque `x-codex-turn-state` response header within the **same turn**, including retries, and clears it across turns/auth ownership changes. The proxy currently has no such replay path. Model this only when an ingress request/turn boundary is unambiguous; do not store it indiscriminately for an entire conversation or expose it as quota telemetry. Compare current session/attempt headers against the official source without pretending to be a different first-party client.

### Narrow instruction/tool adaptation, not wholesale prompt rewriting

Claude Code's system instructions currently pass through to Codex (`translate/request.rs:495,511-559`). A short explicit adaptation for known client boilerplate may improve tool/model expectations, but preserve user/project/security instructions byte-for-byte where possible. Never globally replace the word “Claude”, inject Codex-only executable tools or introduce a second agent loop.

The Read rewrite at `translate/read_rewrite.rs:26-75` intentionally drops very large offsets and empty pages. It can turn a legitimate large-file seek into an initial read; narrow or make this heuristic explicit rather than calling it universally faithful. Schema `pattern` stripping also weakens constraints and should be capability-tested rather than expanded indiscriminately.

### Smaller follow-ups

- Select standalone search domain filters from the chosen tool, not always the first search tool (`src/providers/codex/search.rs:400-406`).
- Fix the shared Windows path discriminator: Rust reports `windows`, while `src/paths.rs:28,50` checks `win32`; test real platform strings.
- Extend CI platform coverage and add a consented, opt-in live subscription smoke script. Do not run paid/quota-consuming live tests in ordinary CI or commit credentials/captures.

## Official native benchmark sources

Compared against OpenAI Codex source commit [`b337fe06b7d8d6e5b42295e04c5e92c2b9c46567`](https://github.com/openai/codex/commit/b337fe06b7d8d6e5b42295e04c5e92c2b9c46567), observed 2026-10-22:

- [Request assembly, continuation compatibility and turn-state scope](https://github.com/openai/codex/blob/b337fe06b7d8d6e5b42295e04c5e92c2b9c46567/codex-rs/core/src/client.rs).
- [Model discovery](https://github.com/openai/codex/blob/b337fe06b7d8d6e5b42295e04c5e92c2b9c46567/codex-rs/codex-api/src/endpoint/models.rs) and [capability/context-budget definitions](https://github.com/openai/codex/blob/b337fe06b7d8d6e5b42295e04c5e92c2b9c46567/codex-rs/protocol/src/openai_models.rs).
- [Credential refresh and concurrent auth ownership](https://github.com/openai/codex/blob/b337fe06b7d8d6e5b42295e04c5e92c2b9c46567/codex-rs/login/src/auth/manager.rs).
- [Native function-tool conversion](https://github.com/openai/codex/blob/b337fe06b7d8d6e5b42295e04c5e92c2b9c46567/codex-rs/tools/src/responses_api.rs): dynamic/MCP tools use `strict: false`; blanket strict-schema enforcement is not native parity.
- [WebSocket semantics](https://developers.openai.com/api/docs/guides/websocket-mode), [reasoning](https://developers.openai.com/api/docs/guides/reasoning) and [compaction](https://developers.openai.com/api/docs/guides/compaction).

These sources establish a comparison baseline, not a public compatibility guarantee for the private ChatGPT subscription endpoint. Public Platform API limits and features must not be assumed to be subscription entitlements.

## Delivery order and acceptance checks

1. Record this review and scope; consolidate configuration, commands, routing and API ingress to Codex only, with rejection tests for removed backends.
2. Remove unreachable backend implementations, unused OpenAI translation adapters and backend-only dependencies; leave user credential storage untouched.
3. Update current documentation and examples to subscription-only operation.
4. Follow up separately with tool-stream parity, account-bound sockets/refresh, input validation and bounded buffers, each with a focused regression and commit.
5. Add model-aware launch/context defaults and measured continuity improvements only after those correctness checks.

Baseline checks executed before consolidation: `cargo test --all --locked` passed **1,205 tests**; `cargo fmt --all -- --check` passed. Tests use local mocks/synthetic credentials. Live ChatGPT OAuth, account access, generation quality and latency were **not** verified.

## Consolidation delivered

The backend-removal request was implemented separately from the native-quality findings above:

- `7c11773`: record the review and subscription-only scope before implementation.
- `5139008`: Codex-only routing, configuration, commands and API ingress; explicit stale-alias migration errors; rejection and shutdown/stream-monitor regressions. Its full suite passed **821 tests**.
- `d3a948c`: subscription-only documentation and installer guidance, obsolete page/screenshot removal and redirects for renamed guide URLs. Historical PR provenance remains intact.
- `6c0bedc`: delete disconnected backend implementations and backend-only dependencies; remove OpenCode-only response policies while retaining strict Codex error behavior.

Only Codex remains as a built-in upstream. Claude Code's Anthropic API and the optional OpenAI-compatible ingress APIs remain. Browser/device OAuth still uses the proxy's own ChatGPT subscription credentials; no Platform API-key authentication was added. Existing user credential files were not removed. At this consolidation snapshot, retired model IDs and backend commands were tested as rejected. The model-aware follow-up below removes model admission gates; retired backend commands remain unavailable.

The native-quality correctness findings are **follow-up work**, not fixes included in this consolidation. The lower test count reflects removal of retired-provider suites and seven alternate-provider policy tests, not a claim that native conformance is complete. Relevant Codex transport, continuation, compaction, tool, auth and cancellation coverage remains.

Documentation syntax/navigation/local links/JSON examples and installer shell syntax passed static checks. A full Astro build was skipped because Bun and the documentation dependencies are not installed. Live subscription login/generation was not verified.

A final test-isolation review found two redundant route-only smoke tests that constructed a real provider against ambient credentials. They were removed in `3eff18d` rather than adding global environment mutation; isolated mock-upstream tests already verify actual Codex routing and generation. Ordinary validation must not implicitly opt a logged-in developer into quota-consuming live calls.

### Final validation result

On the final source/test state, `cargo test --all --locked` passed **812 tests, zero failures**. Formatting, Clippy with `-D warnings`, and the locked release build all passed. The release binary starts and reports version `0.1.45`. Documentation's full Astro build and live subscription behavior remain unverified as noted above.

### Reproduce final validation

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked
cargo build --release --locked
./target/release/claude-code-proxy --version
```

The release CLI was also checked with isolated `CCP_CONFIG_DIR`, `CODEX_HOME` and `HOME`: model discovery lists only Codex, and all four retired backend commands return exit code 2. No subscription credentials are needed for those checks.

## Model-aware integration follow-up

This section records the `fe1b5db` delivery snapshot. The subsequent cleanup removes its optional launcher and startup context policy; authenticated discovery, alias routing and model-aware request capabilities remain.

Implemented after the review above, using OpenAI Codex source `61f34c35af77777db4d77d0ea2aeb8547a6b4523` and discovery compatibility version `0.163.0` (stable source `83086188ae0614bd882818ff1aa23f3194962012`):

- Authenticated subscription `/models` discovery uses proxy-owned OAuth. There is no automatic `~/.codex`, `CODEX_HOME`, or native CLI credential/configuration dependency. Optional metadata imports must be explicitly configured.
- Discovery uses bounded account/endpoint/version caches and immutable operation snapshots, including pinned authentication. Model metadata controls defaults, capabilities and menus, **not admission**: well-formed opaque model IDs still reach Codex for validation. Built-in model data is an offline hint, not an allowlist. `supported_in_api` does not exclude subscription models.
- Final family mappings: **Fable → GPT-6 Astra; Opus → GPT-6.1 Sol; Sonnet → GPT-6 Sol; Haiku → GPT-6 Luna**. Versioned family aliases use the same resolver. Explicit Codex IDs are preserved; configuration conflicts fail rather than silently replacing a requested model.
- Requests consume advertised effort, summary, verbosity, lane, service-tier and input capabilities, preserving unknown metadata. Native effort aliases are normalized; paid acceleration is not enabled merely because a catalog advertises a default tier.
- The explicit `launch` command modifies only the child environment. It enables tool search, model-family defaults and conservative context/compaction settings. For the known 272,000 active window, the default usable cap is 258,400 and the compaction-window ceiling is 244,800. Launcher policy deliberately requires an explicit safe cap when only a larger maximum, not active capacity, is known.
- Anthropic `speed: "fast"` requests priority on the same received model; `speed: "standard"` clears acceleration. Response `usage.speed` is based only on a confirmed upstream tier. Stock Claude Code `/fast` remains Opus-only and can switch other families before sending; the proxy does not guess and undo that client-side action. Other families have explicit `-fast` aliases. The user explicitly approved child-only Anthropic fast-availability check skips; Codex OAuth and upstream validation remain enforced.
- Legacy passes fixed real Rust platform-name handling, removed unused private test helpers and a direct dependency, and corrected obsolete build/tooling descriptions. Credential migration behavior was retained. An initial server test exposed ambient discovery credentials; server fixtures now inject explicit missing-auth storage instead of relying on developer shell settings.

These changes do **not** claim to fix the separate original tool-stream validation, pooled-WebSocket account binding, or cross-process refresh/logout race findings. Live generation quality, subscription quota consumption and real CLI fast-mode UI behavior are not established by local mock tests.

### Follow-up commits and validation

- `8e02150`: replace legacy path handling and remove unused private test helpers.
- `c876b48`: remove obsolete tooling assumptions and an unused direct dependency.
- `e72ac46`: independent authenticated discovery, account-scoped model/auth snapshots, family routing and model-aware request capabilities. Regressions cover OAuth refreshes longer than the metadata timeout, persisted rotation, and numeric effort budgets serialized as unsigned JSON integers.
- `4de849a`: confirmed upstream service-tier reporting with live/buffered/accumulated usage parity and reconnect isolation.
- `c7f481f`: model-aware launcher, asynchronous CLI discovery and isolated subprocess/HTTP regressions.

On this final source/test state, `cargo fmt --all -- --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --all --locked`, and `cargo build --release --locked` all passed. The full suite passed **852 tests, zero failures** with normal test parallelism.

The release binary reports `0.1.45`. An additional isolated CLI check used a fresh synthetic `HOME`, `CCP_CONFIG_DIR`, `CODEX_HOME`, authentication file and explicit metadata import, with discovery disabled. It verified model listing, all four family launch environments, the 258,400/244,800 context defaults, an unknown explicit model with a safe override, rejection of all four retired backend commands, and unchanged synthetic Claude settings, auth and native cache files. This check started neither Claude Code nor an upstream network request.

Documentation syntax, examples and local navigation passed static checks. The full Astro build remains skipped because Bun/documentation dependencies are absent. No controlled live generation, quota/latency benchmark, or actual Claude Code `/fast` UI verification was performed. The child-only availability skips do not establish upstream fast-tier entitlement.

## Full legacy cleanup sweep

Starting from `fe1b5db`, five subagents swept the tracked repository across runtime/UI/storage, Codex transports/auth, protocol translators, integration tests, and manifests/tooling/documentation. Changes remove proven leftovers rather than redesign active features.

- **Remove the optional launcher:** at the user's explicit direction, delete `launch`, its module/dispatch, startup-only context calculations and dedicated tests. Current examples invoke normal `claude` with connection variables; no replacement wrapper, automatic client configuration, or global settings writes. Proxy-owned discovery, raw advertised metadata, family routing and request capability handling remain.
- **Delete retired translation policy:** remove the unused generic OpenAI-compatible translator entrypoint and its alternate-backend configuration branches. The sole content-normalization caller already required a null missing-tool-input default; remove that selectable parameter while preserving the default and explicit values.
- **Reuse active protocol helpers:** consolidate duplicate Read-repair and web-search parsing/ID helpers. Delete a write-only buffered block map and unread tool-state field; existing reducer validation and live-stream behavior remain unchanged. This is not a fix for the original tool-stream findings.
- **Trim transport scaffolding:** delete an unreachable buffered-stream response branch, private forwarding aliases, an unproduced legacy retry marker, an unused retry-after extractor/test setter and unused internal WebSocket context arguments. Preserve actual typed handshake retry/fallback behavior, socket ownership and no-replay guards.
- **Simplify runtime internals:** use existing server entrypoints/logging paths and standard collection/time/file helpers; remove redundant private marker/state, repeated directory setup and overwritten demo assignments. New regressions preserve the intentionally different legacy auth-schema fallback paths and sorted/unique catalog listings. Synthetic demo labels now match current Sonnet routing.
- **Remove false/redundant coverage:** delete nine WebSocket tests that only exercised handwritten requests, their own SSE helper or the third-party socket library, including swallowed failures and a tautological assertion. Production WebSocket unit tests, proxy/continuation/smoke integrations and public API compatibility tests remain. Merge duplicate auth-path and health checks; strengthen actual token-count/log-redaction assertions and retain both meaningful fixtures.
- **Isolate test side effects:** env-cleared subprocesses exercise catalog imports, logging/traffic factories and CLI operations. Existing serialized transport suites use their current RAII guards to clear ambient model/traffic settings and keep state under temporary roots. No new shared-process mutation pattern or runner framework was introduced; the existing guards still serialize and restore affected variables. The shared-process server suite still needs an externally clean profile for its production logging/config reads; its injected missing-auth client prevents ambient discovery.
- **Remove unused build/documentation residue:** drop three unused direct dependencies (`thiserror`, `tracing`, `tracing-subscriber`), unused Clap/Axum/UUID features and ten unreachable lockfile packages without version bumps; move test-only Tower to dev dependencies. Remove the root Bun-install pane, unused screenshot/image-zoom/provider-stack assets, stale user-agent pin and obsolete helper examples. Correct the generated `llms.txt` full-body description while preserving build behavior, release targets, historical provenance and redirect migrations.

Intentional public Codex/client compatibility, session-only APIs, keychain and credential migrations, legacy config/environment aliases, account-scoped discovery/authentication and explicit model-ID passthrough remain. Separate original correctness/security findings are still follow-up work.

### Sweep validation

On macOS, the final unified run passed formatting, strict all-target Clippy (`-D warnings`), the locked offline full suite and the locked offline release build. Validation used a clean temporary `HOME`, `CCP_CONFIG_DIR` and state root, retained only the existing Rust tool/cache locations, and kept normal test parallelism. **842 top-level tests passed, zero failed**: 667 library tests, 6 binary unit tests and 169 integration tests; four nested subprocess summaries were not counted again. The decrease from 852 reflects deleted launcher/math, false-coverage and duplicate cases, with new focused regressions added for active behavior.

The release binary reports `0.1.45`. An isolated direct CLI/server check confirmed that `launch` is rejected, offline model/family listings still work, the real release server serves health/models routes and shuts down cleanly, and synthetic auth, Claude settings and native-cache files remain unchanged. It started no Claude process and made no live upstream request.

Shell/workflow/configuration syntax, offline dependency metadata, documentation examples and 65 local links/anchors passed static checks. The full Astro build remains skipped because Bun/docs dependencies are absent. Cross-platform compilation, actual Claude Code `/fast` UI, live generation and quota/latency remain unverified; full offline cross-platform metadata could not resolve uncached `crossterm_winapi` without a download. No dependency versions were upgraded, user credentials/settings changed, or remote commits pushed.

Incremental source/test/tooling commits:

- `5f4ba8b`: remove the optional launcher and startup context policy.
- `65e127a`: remove retired translator policy and duplicate protocol helpers.
- `056aae5`: delete unreachable transport paths and private compatibility residue.
- `725d4c4`: simplify runtime storage, registry and monitoring internals.
- `2db81dd`: remove false legacy coverage and isolate integration fixtures.
- `8cf0beb`: prune unused dependencies, features and obsolete build assets.
