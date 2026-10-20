---
title: ChatGPT subscription integration
description: Build and use the local Codex integration branch and review its pull request selections.
---

The local `chatgpt-codex-improvements` branch combines 14 of the 36 pull requests
open on 21 October 2026. It targets the Codex backend authenticated with a
ChatGPT subscription. These are local Git merges; the upstream GitHub pull
requests remain open.

## Build and sign in

Run these commands from the repository root to use this branch's binary.
A Homebrew or release installation does not include these local changes.

```sh
cargo build --release --locked
./target/release/claude-code-proxy codex auth login
./target/release/claude-code-proxy codex auth status
./target/release/claude-code-proxy models
./target/release/claude-code-proxy serve
```

Sign in using your ChatGPT account. The proxy stores its own OAuth credentials;
an OpenAI Platform API key is not required for this backend. For a headless
login, use `codex auth device` in place of `codex auth login`.

### Use this build instead of Homebrew

To make the local source build the command your shell runs:

```sh
cargo install --path . --locked --force
export PATH="$HOME/.cargo/bin:$PATH"
rehash
which claude-code-proxy
```

The resolved path should be `~/.cargo/bin/claude-code-proxy`. Add the `export`
line to `~/.zshrc` to preserve that order in new terminals. Re-run the Cargo
install command after updating this checkout.

If Homebrew is running a background instance, stop it with
`brew services stop claude-code-proxy`, then run `claude-code-proxy serve`
to start the source build. Restart any already-running proxy after installation.

## Launch Claude Code

In another terminal, launch Claude Code:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-6-sol \
ANTHROPIC_SMALL_FAST_MODEL=gpt-6-luna \
ENABLE_TOOL_SEARCH=true \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
  claude
```

The example uses the branch's default Codex models. Account access is determined
by the upstream service; choose another supported model if your account rejects
one. This setup uses the normal local context policy without a `[1m]` override.

Transport defaults to `auto`: WebSocket first, with HTTP fallback if setup fails
before a request is sent. Compaction summaries use HTTP. Tool search appends
loaded schemas without rewriting earlier input, preserving prompt cache reuse.

Optional features remain explicit:

- Set `CCP_CODEX_PREVIOUS_RESPONSE_ID=1` on the **proxy process** for compatible
  append-only WebSocket continuation. Its state is held in memory.
- Set `CCP_CODEX_FULL_LANE=1` on the proxy for parallel tool calls with
  `gpt-5.6-sol` and `gpt-5.6-terra`. It does not force other models off Lite.
- Server compaction remains opt-in; see [Codex](/providers/codex/#server-compaction).

## Included pull requests

| PR | Benefit |
| --- | --- |
| [#167](https://github.com/raine/claude-code-proxy/pull/167) | Browser login survives unrelated localhost requests. |
| [#173](https://github.com/raine/claude-code-proxy/pull/173) | HTTP fallback when WebSocket setup fails. |
| [#168](https://github.com/raine/claude-code-proxy/pull/168) | Detect compaction prompts with trailing context and route summaries over HTTP. |
| [#158](https://github.com/raine/claude-code-proxy/pull/158) | Bound empty-response retries and reconnect interrupted thinking without replaying answer text or tool actions. |
| [#120](https://github.com/raine/claude-code-proxy/pull/120) | Recover truncated HTTP tool streams before tool commit and validate completed tool arguments. |
| [#132](https://github.com/raine/claude-code-proxy/pull/132) | Give sibling agents independent standalone web search sessions. |
| [#135](https://github.com/raine/claude-code-proxy/pull/135) | Adaptive WebSocket connection pacing instead of an unconditional one-second delay. |
| [#184](https://github.com/raine/claude-code-proxy/pull/184) | Preserve prompt caching as tool search loads deferred tools. |
| [#183](https://github.com/raine/claude-code-proxy/pull/183) | Give auto-review requests low default effort and a separate prompt cache key. |
| [#149](https://github.com/raine/claude-code-proxy/pull/149) | Include reasoning tokens and subscription quota snapshots in response usage. |
| [#127](https://github.com/raine/claude-code-proxy/pull/127) | Forward Codex quota telemetry headers to local clients. |
| [#130](https://github.com/raine/claude-code-proxy/pull/130) | Extend model support using the local Codex CLI model catalog. |
| [#77](https://github.com/raine/claude-code-proxy/pull/77) | Optional full Responses lane for parallel tools on GPT-5.6 Sol and Terra. |
| [#187](https://github.com/raine/claude-code-proxy/pull/187) | Fix current Rust/Clippy compatibility. |

PRs #183 and #184 were drafts when selected. Their local regression tests are
part of the combined test suite.

## Integration corrections

- Preserve current model aliases, built-in GPT-6 model lanes, ultrafast tiers,
  and subscription quota error metadata when merging older branches.
- Await connection-gate updates under contention so upgrade rejections are
  retained; a regression test covers a busy gate.
- Route hidden API-supported catalog models explicitly, including `-fast`
  variants; an isolated integration test covers listing, routing, native model
  validation, and preservation of built-in tiers.
- Apply the full-lane override only to the two models named by #77, preserving
  Lite routing for Luna and newer models.

Quota telemetry reports the most recently observed upstream snapshot. It is
not a fresh account-usage query. Subscription model access and live OAuth or
generation acceptance require signing in and trying the service.

## Validation

Verified on Rust 1.98.1:

- `cargo test --all --locked`: 1,205 tests passed; baseline main had 1,119.
- `cargo clippy --all-targets --locked -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `cargo build --release --locked`: passed; the release binary starts and prints its version.

Provider tests use synthetic credentials and local mock servers. Live ChatGPT
login and generation are not verified by these checks.

## Excluded pull requests

| PRs | Reason |
| --- | --- |
| [#42](https://github.com/raine/claude-code-proxy/pull/42) | Main already serializes token refresh, reloads durable credentials, and protects peer token rotations. |
| [#30](https://github.com/raine/claude-code-proxy/pull/30), [#66](https://github.com/raine/claude-code-proxy/pull/66), [#121](https://github.com/raine/claude-code-proxy/pull/121) | Main already handles exhausted subscription quota and preserves reset information. Older approaches or an external quota-state file are unnecessary. |
| [#178](https://github.com/raine/claude-code-proxy/pull/178) | The included recovery work already logs empty responses and retry exhaustion; avoid duplicate logging paths. |
| [#114](https://github.com/raine/claude-code-proxy/pull/114) | Experimental model addition; #130 accepts the model if the local catalog marks it API-supported. |
| [#122](https://github.com/raine/claude-code-proxy/pull/122) | Accepts repeated empty completions after any successful tool result, including file reads; that can silently stop unfinished coding work. |
| [#164](https://github.com/raine/claude-code-proxy/pull/164) | Draft disk persistence also relaxes replay invalidation and loads persisted history without the existing in-memory size and expiry checks. Keep the current compaction boundary safeguards. |
| [#62](https://github.com/raine/claude-code-proxy/pull/62) | Adds a separate persisted quota store, endpoint, and monitor state that overlap the selected telemetry work. Keep one telemetry path. |
| [#63](https://github.com/raine/claude-code-proxy/pull/63) | Dedicated-model compaction requires external one-shot marker hooks; the selected compaction transport fix works with ordinary Claude Code requests. |
| [#74](https://github.com/raine/claude-code-proxy/pull/74) | Changes the default visibility of reasoning summaries; the existing `CCP_CODEX_REASONING_SUMMARY=off` already provides that preference. |
| [#75](https://github.com/raine/claude-code-proxy/pull/75) | Adds an optional way to discard encrypted reasoning signatures; retain reasoning replay for conversation continuity. |
| [#188](https://github.com/raine/claude-code-proxy/pull/188), [#185](https://github.com/raine/claude-code-proxy/pull/185), [#179](https://github.com/raine/claude-code-proxy/pull/179), [#161](https://github.com/raine/claude-code-proxy/pull/161), [#146](https://github.com/raine/claude-code-proxy/pull/146), [#101](https://github.com/raine/claude-code-proxy/pull/101), [#34](https://github.com/raine/claude-code-proxy/pull/34), [#11](https://github.com/raine/claude-code-proxy/pull/11), [#8](https://github.com/raine/claude-code-proxy/pull/8) | Other providers, subscriptions, or historical TypeScript implementations outside this ChatGPT-backed Rust setup. |
| [#21](https://github.com/raine/claude-code-proxy/pull/21) | Linux-only systemd helper; this checkout is on macOS. |
