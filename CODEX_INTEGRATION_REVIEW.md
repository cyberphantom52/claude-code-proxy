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

Only Codex remains as a built-in upstream. Claude Code's Anthropic API and the optional OpenAI-compatible ingress APIs remain. Browser/device OAuth still uses the proxy's own ChatGPT subscription credentials; no Platform API-key authentication was added. Existing user credential files were not removed. Retired model IDs and backend commands are tested as rejected, not silently rerouted.

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
