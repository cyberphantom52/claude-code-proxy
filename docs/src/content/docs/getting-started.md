---
title: Getting started
description: Install claude-code-proxy, authenticate with Codex, start the server, and open one working Claude Code session.
---

This path connects the Claude Code client to the sole upstream backend, Codex, using proxy-owned ChatGPT OAuth. For this branch's changes, use the [source build guide](/using/chatgpt-integration/); existing releases may differ.

## 1. Install

On macOS or Linux with Homebrew:

```sh
brew install raine/claude-code-proxy/claude-code-proxy
```

Or use the release installer:

```sh
curl -fsSL https://raw.githubusercontent.com/raine/claude-code-proxy/main/scripts/install.sh | bash
```

Windows archives and binaries for every supported platform are on the [GitHub releases page](https://github.com/raine/claude-code-proxy/releases).

## 2. Sign in to Codex

Use a **ChatGPT Plus or Pro account**, not an OpenAI API account:

```sh
claude-code-proxy codex auth login
```

The proxy stores and refreshes its own tokens; it does not use Platform API keys or native Codex CLI credentials. For SSH or another headless environment, use `claude-code-proxy codex auth device` instead.

## 3. Start the proxy

Keep this process running:

```sh
claude-code-proxy serve
```

It listens on `127.0.0.1:18765`. In an interactive terminal it opens the monitor TUI.

## 4. Start Claude Code

Open another terminal and run:

```sh
claude-code-proxy launch
```

Send a prompt. The request appears in the monitor and the response streams into Claude Code.

The launcher targets the already-running proxy, sets tool search and model-aware context limits for the child only, and leaves global settings untouched. It prefers Astra for main work and Luna for small requests when listed in the authenticated catalog. Use `launch --model fable --small-model haiku` for explicit family selection, or `launch --print-env` to inspect the environment without starting Claude Code.

Discovery is default-on and uses the proxy's ChatGPT OAuth directly; no installed Codex CLI, `~/.codex`, or `CODEX_HOME` configuration is needed. For offline catalog selection use `CCP_CODEX_MODEL_DISCOVERY=0` on the proxy/launcher (generation still needs network access). See [Models and routing](/using/models-and-routing/) for the built-in fallback and explicit optional metadata imports.

Switch inside Claude Code without exiting:

```text
/model fable
/model opus
/model sonnet
```

Fable targets Astra, Opus GPT-6.1 Sol, Sonnet GPT-6 Sol, and Haiku Luna. Well-formed explicit IDs work even when absent from discovery; upstream validates availability and access. Remove conflicting `CCP_CODEX_MODEL` / `codex.model` forced overrides before launching. Model or capability errors never cause substitution with another model.

A 272,000-token active context gives a compaction ceiling of 244,800, reduced further by model metadata when necessary. `[1m]` is not needed and cannot increase upstream capacity. If context metadata is missing, pass a verified safe `--context-window <tokens>`.

Continue to [Configure Claude Code](/using/configure-claude-code/) for launch options and conservative context budgets.
