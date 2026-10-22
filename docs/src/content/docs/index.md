---
title: What is claude-code-proxy?
description: Run Claude Code with OpenAI Codex through your ChatGPT subscription.
---

<div class="hero-copy">
claude-code-proxy connects Claude Code to OpenAI Codex using your ChatGPT subscription. Claude Code remains the client harness; Codex is the only upstream backend.
</div>

<div class="route-rail" aria-label="Claude Code connects through the proxy to ChatGPT Codex">
  <div class="route-node route-client"><span class="route-kicker">Client harness</span><strong>Claude Code</strong></div>
  <div class="route-connector" aria-hidden="true"><span>→</span></div>
  <div class="route-node route-proxy"><span class="route-kicker">Local bridge</span><strong>claude-code-proxy</strong><span>ChatGPT OAuth and API translation</span></div>
  <div class="route-connector" aria-hidden="true"><span>→</span></div>
  <div class="route-node route-providers"><span class="route-kicker">Subscription backend</span><strong>OpenAI Codex</strong></div>
</div>

## What it keeps

- Claude Code's tools, skills, hooks, subagents, editor integrations, and terminal interface stay on the client side.
- The proxy translates Anthropic Messages, tools, images, reasoning summaries, and streaming to Codex.
- Optional native Codex Responses, text-only Chat Completions compatibility, and image generation/editing reuse the same proxy-owned ChatGPT login.
- The monitor shows sessions, requests, errors, token use, and throughput.

OpenAI-compatible schemas do not mean OpenAI Platform access. The proxy uses ChatGPT OAuth, not Platform API keys, and does not reuse native Codex CLI credentials.

## Next steps

[Get started](/getting-started/), read the [Codex backend details](/providers/codex/), or follow [how requests flow](/how-it-works/).
