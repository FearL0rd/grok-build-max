# Multi-Provider Support + Ordered Failover for Grok Build

**Date:** 2026-08-25
**Status:** Approved design (brainstormed with user)
**Scope:** Let the `grok` TUI work with 10 AI providers, configured from a new
`/providers` menu, with automatic ordered rollover when a provider fails.

## 1. Problem

Grok Build ships as a terminal AI coding agent bound to SpaceXAI's service.
The user needs it to work with: Anthropic, OpenAI, Gemini, NVIDIA, Ollama,
GitHub Copilot, OpenRouter, DeepSeek, Cerebras, and any OpenAI-compatible
local API — with a menu to configure them and automatic rollover to the next
provider in a user-ordered list when one fails or runs out of tokens.

### Existing capability (baseline, not rebuilt)

- `[model.<name>]` sections in `~/.grok/config.toml` already support
  `api_backend` ∈ {`chat_completions` (OpenAI default), `responses`
  (OpenAI Responses), `messages` (Anthropic Messages)}, plus `model`,
  `base_url`, `api_key`, `extra_headers`, `query_params`,
  `env_http_headers`, `context_window`, `temperature`, `top_p`,
  `max_completion_tokens`.
- Therefore **9 of the 10 target providers need no new protocol code** —
  they are config-only (see preset table, §5). Only **Gemini** needs a new
  native backend.
- Model selection UI exists: Ctrl+M picker, `/model`, `grok models`,
  `[models] default`.
- Retry machinery exists in `xai-grok-sampler` (`src/retry.rs`):
  `classify_error` → `RetryDecision` {Retry, RetryWithBackoff,
  RetryWithImageStrip, RetryWithClientRebuild, EmitToSession, Fatal},
  consumed in `src/actor/request_task.rs`. Client rebuild via
  `SamplingClient::new(config)` already exists (HTTP/1.1 retry path).

### The three real gaps

1. No Gemini native protocol (different `contents/parts` request/response
   shape; no OpenAI-compatible path for it).
2. No TUI menu for configuring providers (config.toml editing only).
3. No cross-provider rollover (retries are per-model only; a fatal error
   ends the request).

## 2. Agreed decisions (from clarification)

| # | Decision |
|---|----------|
| Q1 | Rollover triggers on **any fatal failure** (400/401/403/404/422, 429-exhausted, 5xx-exhausted, timeouts, connection refused, `IdleTimeout`, serialization failure) |
| Q2 | **Per-request reset**: every request re-walks the chain from entry 0. No sticky state, no cooldown |
| Q3 | **Native Gemini backend** (full: tool calls included) |
| Q4 | **`/providers` TUI menu** that edits `config.toml`; keys stored in `config.toml` `api_key` field — **no env vars, no `.env`** |
| Q5 | Chain entries = **model names**, and the **built-in Grok is a first-class entry** (resolves via session-token auth) |
| A1 | Failover chain lives **inside the sampler actor** (reuses retry + client-rebuild machinery) |
| — | Strictly **sequential** requests — one connection at a time, no parallel provider probing (user constraint) |
| — | Panel must show the **current active model in use** |

## 3. Architecture

```
~/.grok/config.toml
  [model.anthropic-claude] [model.openai-gpt] [model.ollama-llama] [model.gemini-gem] ...
  [failover]
    order = ["grok", "anthropic-claude", "openai-gpt", "ollama-llama", "gemini-gem"]
      │
      ▼  xai-grok-config: parse [failover] → FailoverConfig { order: Vec<String> }
xai-grok-shell (session): resolve each name → SamplerConfig
      │  (existing resolve_model_to_sampling_config per name)
      ▼
xai-grok-sampler: Vec<SamplerConfig> chain
      │  per request: walk entries 0..N sequentially, rebuild client on rollover
      ▼
xai-grok-pager: /providers panel — status, reorder, add/edit/remove,
      │  writes config.toml (toml_edit round-trip), hot-applies
      ▼
TUI scrollback: rollover/skip events; active-model line in /providers
```

### Crate touch map

| Crate | Change |
|-------|--------|
| `xai-grok-config` | New `[failover]` section: `order: Vec<String>` (model names). Absent/empty → chain = `[models.default]` (today's behavior, zero regression). Unknown names collected as startup warnings, not errors |
| `xai-grok-sampler` | Chain field on the request path; rollover loop in `src/actor/request_task.rs`; new `SamplingEvent::ProviderRolledOver` + `ProviderSkipped` + `ProviderFailed`; **new `ApiBackend::Gemini`** + `src/stream/gemini.rs` |
| `xai-grok-shell` | Build `Vec<SamplerConfig>` chain from config; expose active-provider + last-rollover state to the TUI (same channel the `/settings` pane uses) |
| `xai-grok-pager` | `/providers` slash command + modal panel (see §6) |

### Chain rules

- Chain = `[failover].order` verbatim. **No sticky state** (Q2): nothing
  from a previous request is remembered — every request re-derives its
  start point and walk from scratch.
- **Chain start point**: if the currently selected model (via `/model` or
  Ctrl+M) is a member of `order`, the walk starts at that entry and
  proceeds forward in list order (no wrap). If the selected model is not
  in `order`, the walk starts at entry 0. Strictly forward, no
  re-attempts within a request.
- Entry with missing `api_key` → skipped at request time with one
  `ProviderSkipped` event, not an error.
- Built-in Grok entry needs no key (session token auth).
- Disabled entries (`enabled = false`) skipped with `ProviderSkipped`.
- Sequential only: entry N+1 is attempted only after entry N is Fatal.

## 4. Request lifecycle (sampler actor)

```
submit request
  for entry in chain[0..N]:
    if entry disabled or key missing → emit ProviderSkipped {name, reason}; continue
    client = SamplingClient::new(entry.config)        # existing ctor
    run existing retry loop (classify_error)
      ├─ success → stream response; record entry as active; done
      ├─ Fatal BEFORE any output emitted
      │    ├─ more entries → emit ProviderRolledOver {from, to, reason}; next entry
      │    └─ none left    → emit ProviderFailed (chain exhausted); final error
      └─ Fatal AFTER partial output
           → NO rollover. EmitToSession path (partial text already visible;
             resubmitting elsewhere would duplicate output)
```

- **Per-entry retry budget**: each entry gets its own full budget
  (default 15 / per-model `max_retries`). 429 entries roll over fast via
  the existing `RATE_LIMIT_RETRY_THRESHOLD` = 2 (~2 waits, not 14).
- **Strict forward walk**: an entry failed in this request is never
  re-attempted within it.
- **Duplication guard** is the one deviation from "rollover on anything":
  mid-stream failure with visible output never rolls over. This matches
  the existing `retry_only_before_output` semantics.
- **Context window follows the active entry**: each `[model.*]` carries
  its own `context_window`; compaction decisions use the entry actually
  serving, not entry 0.
- Rollover sends the same full conversation to the next provider.

### New events (TUI-visible)

| Event | Scrollback rendering |
|-------|---------------------|
| `ProviderSkipped { name, reason }` | dim: `skipped: gemini-gem (key missing)` |
| `ProviderRolledOver { from, to, reason }` | `✳ anthropic-claude failed: 429 after 2 retries → openai-gpt` |
| `ProviderFailed` (chain end) | existing error UI prefixed: `all providers failed: grok, anthropic-claude, openai-gpt` |

## 5. Gemini native backend

New `ApiBackend::Gemini` variant + `src/stream/gemini.rs`, wired in
`client.rs` dispatch (same shape as the existing three backends).

- Endpoint: `POST {base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse`
  with `x-goog-api-key` header built from the entry's `api_key` (not from
  `extra_headers`, so the key is never duplicated).
- SSE loop reuses the chat_completions streaming pattern (one
  `GenerateContentResponse` JSON per SSE line).

| Grok concept | Gemini mapping |
|--------------|----------------|
| system prompt | `systemInstruction.parts[]` |
| user/assistant messages | `contents[]`, `role: "user" \| "model"` (consecutive same-role merged) |
| assistant tool call | `functionCall` part |
| tool result | `functionResponse` part (user turn) |
| mixed text + tool calls | multiple `parts[]` in one content |
| `model`, `temperature`, `top_p`→`topP`, `max_completion_tokens`→`maxOutputTokens` | `generationConfig` |
| tool definitions | `tools[0].function_declarations[]` (same definitions grok serializes for OpenAI, field-renamed) |

- **Error mapping**: Gemini SSE `error: {code, message, status}` →
  `SamplingError::Api { status, .. }` identical to other backends, so
  `classify_error`/rollover semantics are uniform (a Gemini 429 behaves
  exactly like an OpenAI 429).
- **Documented limitations** (config docs): no reasoning/thinking budget,
  no caching, no code-execution tool.

### Provider preset table (endpoints/auth learned from the DIYTravel reference)

| Preset | base_url | api_backend | key (stored in `api_key`) | extra |
|--------|----------|-------------|---------------------------|-------|
| Grok | built-in | built-in | session token | — |
| Anthropic | `https://api.anthropic.com/v1` | `messages` | Anthropic API key | header `anthropic-version: 2023-06-01` |
| OpenAI | `https://api.openai.com/v1` | `chat_completions` | OpenAI API key | — |
| Gemini | `https://generativelanguage.googleapis.com` | `gemini` | Gemini API key | `x-goog-api-key` from `api_key` |
| NVIDIA | `https://integrate.api.nvidia.com/v1` | `chat_completions` | NVIDIA API key | — |
| Ollama | `http://localhost:11434/v1` | `chat_completions` | none (local) | — |
| GitHub Copilot | `https://api.githubcopilot.com` | `chat_completions` | GitHub token | dynamic header `X-Request-Id: <uuid>` per request |
| OpenRouter | `https://openrouter.ai/api/v1` | `chat_completions` | OpenRouter API key | header `HTTP-Referer` |
| DeepSeek | `https://api.deepseek.com/v1` | `chat_completions` | DeepSeek API key | — |
| Cerebras | `https://api.cerebras.ai/v1` | `chat_completions` | Cerebras API key | — |
| OpenAI-compatible (local) | user-typed | `chat_completions` | optional | — |

- **Copilot dynamic header**: `extra_headers` only sends static values.
  The Copilot preset marks `X-Request-Id` as a *dynamic* header; the
  client generates a fresh UUID per request at build/dispatch time
  (~10 lines in client code).

## 6. `/providers` TUI panel

Slash command `/providers` opens a modal (existing modal pattern in
`xai-grok-pager`):

```
 Providers                              /providers — Esc to close
 ┌─────────────────────────────────────────────────────────┐
 │ Active: openai-gpt (gpt-4o)                             │
 │ Last rollover: anthropic-claude 429 → openai-gpt        │
 ├─────────────────────────────────────────────────────────┤
 │  1  grok                 built-in      ● session token   │
 │  2  anthropic-claude     claude-opus   ● key in config   │
 │  3  openai-gpt           gpt-4o        ● key in config   │
 │  4  ollama-llama         llama3.1:70b  ○ keyless         │
 │  5  gemini-gem           gemini-2.5    ○ key missing     │
 └─────────────────────────────────────────────────────────┘
  ↑/↓ select   ←/→ reorder   x enable/disable
  a add provider   r remove provider
  e edit provider (model id, base_url, api key, temperature, context window)
  Enter save & close
```

- **Active line** = entry that last served a successful request. Before
  any success, shows the chain's current top entry.
- **Last rollover line** = most recent `ProviderRolledOver`; cleared when
  a request succeeds at entry 0.
- **Status column**: `● key in config` (api_key present) /
  `○ key missing` / `○ keyless` (Ollama-style) / `● session token`
  (Grok login state). Key values are never printed — masked `sk-…****`
  in the edit form.
- **Add flow** (`a` → inline form in the same modal):
  1. Pick preset (table above; "OpenAI-compatible (local)" leaves
     `base_url` blank for user input). Preset pre-fills base_url,
     api_backend, required headers, default model id.
  2. Enter name (config key, e.g. `local-llama`), model id, context
     window (default 200000).
  3. Enter API key value → saved to `[model.<name>].api_key` in
     `config.toml`. Keyless presets (Ollama, local) skip the field.
  4. Save → writes `[model.<name>]` block, appends name to
     `[failover].order`, hot-applies to the running session (no restart).
- **Edit** (`e`): same form pre-filled; writes the block in place.
- **Remove** (`r`): deletes the `[model.*]` block and drops the name
  from `order`, with a confirm prompt.
- **Write path**: `config.toml` edited via toml_edit round-trip so
  comments and unrelated sections survive. Only `[model.*]` blocks and
  `[failover]` are touched.
- **Hot-apply**: panel talks to shell state through the same commands the
  `/settings` pane uses (no new IPC surface).
- **Ctrl+M picker unchanged**: selecting a model that is in the chain
  makes it the start of the walk (see §3 chain start point); selecting a
  model outside the chain does not modify `order`.

### Key storage rule (explicit user decision)

All API keys are stored in `~/.grok/config.toml` (`api_key` field).
No env vars, no `.env` files. Consequence, documented: config.toml holds
plaintext keys — it lives in the per-user `~/.grok/` directory and must
not be committed to VCS or synced to backups.

## 7. Error handling

| Situation | TUI output |
|-----------|-----------|
| Entry skipped (no key / disabled) | dim: `skipped: gemini-gem (key missing)` |
| Rollover | `✳ anthropic-claude failed: 429 after 2 retries → openai-gpt` |
| Chain exhausted | existing error UI prefixed: `all providers failed: grok, anthropic-claude, openai-gpt` |
| Fatal after partial output | error shown, no rollover, existing within-provider retry semantics |
| Unknown model name in `order` | startup warning `failover: unknown model "x" skipped`; config still loads |
| config.toml write fails (permissions) | panel shows error; old order kept in memory; no crash |

## 8. Testing

Per-crate, existing `cargo test -p <crate>` pattern; unit tests in
`#[cfg(test)]` modules.

- **xai-grok-config**: `[failover]` parse — absent → default chain;
  empty list; unknown names collected; order preserved; toml_edit
  round-trip preserves comments.
- **xai-grok-sampler** (chain walk, fake `SamplingClient` factory):
  1. entry 1 429-exhausted → entry 2 succeeds; event order correct
  2. all entries fail → final `ProviderFailed` lists all
  3. partial output then error → no rollover
  4. skipped entries never build a client
  5. per-entry retry budgets independent
- **Gemini backend**: request-mapping tests (messages→contents, role
  merging, tool-call round-trip), SSE chunk parse from recorded fixtures,
  error-mapping test (SSE error → `SamplingError::Api` with correct
  status).
- **xai-grok-pager**: panel state logic as pure functions (reorder /
  enable / remove / add → correct config mutation); rendering smoke
  test. No TUI E2E framework.
- **Manual checklist** (documented): one real end-to-end per protocol
  (chat_completions, messages, gemini) + one real rollover (bad key at
  entry 1 → entry 2 serves).

## 9. Out of scope (YAGNI, deliberate)

- Provider health probing / warm-up
- Parallel failover (violates user's sequential constraint anyway)
- Per-provider token/cost tracking
- Auto-retry of a failed provider mid-session (per-request reset covers it)
- Key rotation UX / key vault integration
- Reasoning/thinking budgets, caching, code execution on the Gemini backend

## 10. Constraints honored

- All provider traffic strictly sequential — one model connection at a
  time; no parallel requests, no multi-agent usage.
- Root `Cargo.toml` treated as read-only (generated); all dependency
  edits per-crate. No new external crates expected (UUID for Copilot
  header: use `getrandom`-based or existing workspace UUID dep — verify at
  plan time; stdlib fallback is a formatted random counter, acceptable
  since the header is an idempotency tag).
- `config.toml` stays the single source of truth; no new storage files.
