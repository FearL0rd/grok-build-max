# Multi-Provider Failover Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. (superpowers:subagent-driven-development is NOT usable for this plan — the user requires strictly sequential execution: "run one query per time in sequence no multiple connections with the model. don't use multiple agents".) Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let grok-build talk to non-Grok providers (Anthropic, OpenAI, Gemini, NVIDIA, Ollama, Copilot, OpenRouter, DeepSeek, Cerebras, any OpenAI-compatible local API) and roll over through an ordered provider list when one fails fatally before emitting output.

**Architecture:** Failover chain `Vec<(String /*display name*/, SamplerConfig)>` lives inside the sampler actor. Per request a forward-only chain walk wraps the existing per-entry retry loop; rollover happens only on a fatal error BEFORE any output was observed. New `ProviderSkipped` / `ProviderRolledOver` / `ProviderFailed` sampling events surface the walk to the TUI. Shell builds the chain from `[failover].order` in `~/.grok/config.toml`; pager gets a `/providers` panel to add/edit/remove/reorder providers with hot-apply.

**Tech Stack:** Rust (workspace crates `xai-grok-sampler`, `xai-grok-sampling-types`, `xai-grok-shell`, `xai-grok-pager`), tokio, reqwest SSE, axum mock servers in tests, `toml_edit` for comment-preserving config writes, existing `uuid` v4.

## Global Constraints

- **STRICTLY SEQUENTIAL**: "run one query per time in sequence no multiple connections with the model. don't use multiple agents." Never dispatch subagents; never parallel tool calls against the model.
- Root `/Cargo.toml` is GENERATED/READ-ONLY — dependency changes go in per-crate `Cargo.toml` only. **No new dependencies are needed anywhere in this plan.**
- API keys live ONLY in `config.toml`: user override — "nothing should be store in .env keys everything should be in the config.toml". Keys are stored as plaintext `api_key = "..."` under `[model.<name>]`. The config file must NEVER be committed to VCS. No env-var key lookup for new providers.
- All `config.toml` writes use `toml_edit` round-trip so user comments survive, under `SAVE_LOCK`, followed by `atomic_write_string` (pattern from `crates/codegen/xai-grok-shell/src/util/config/persist.rs`).
- Rollover ONLY on fatal error and only before ANY output was observed for the request (`output_observed` guard). After partial output, fail the request normally — never restart mid-answer.
- Forward-only chain walk: no wrap-around, one pass per request. Chain start = selected model's position in `[failover].order` if present, else entry 0.
- Entries without a resolvable api_key or explicitly disabled are skipped with `ProviderSkipped` (never retried).
- Chain exhausted → `SamplingError::ProviderFailed { providers }` listing every attempted name.
- Branch: `feat/multi-provider-failover`. Never commit directly to `main`. Conventional commits (`feat:`, `test:`, `fix:`).
- All code/docs/comments in English.
- Quality gates after each task: `cargo fmt --all`, `cargo clippy -p <crate> -- -D warnings`, `cargo test -p <crate>`.
- Spec of record: `docs/superpowers/specs/2026-08-25-multi-provider-failover-design.md`.

---

### Task 1: `[failover]` config section in shell

**Files:**
- Create: nothing
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (near `pub models: ModelsConfig` at ~line 1370)
- Test: unit tests inside same file's existing `#[cfg(test)]` module area (append at end of file)

**Interfaces:**
- Consumes: serde derive patterns already used by `Config`.
- Produces:
  ```rust
  #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
  pub struct FailoverConfig {
      #[serde(default, skip_serializing_if = "Vec::is_empty")]
      pub order: Vec<String>,          // display names, e.g. ["grok", "openai", "ollama-local"]
  }
  // On Config:
  #[serde(default)]
  pub failover: FailoverConfig,
  ```

- [ ] **Step 1: Write failing round-trip tests** (append to the test module at the bottom of `config.rs`; if none exists, create `#[cfg(test)] mod tests { use super::*; ... }`):

```rust
#[test]
fn failover_section_round_trips() {
    let raw = r#"
[failover]
order = ["grok", "openai", "ollama-local"]
"#;
    let cfg: Config = toml::from_str(raw).expect("parse");
    assert_eq!(cfg.failover.order, vec!["grok", "openai", "ollama-local"]);

    let out = toml::to_string_pretty(&Config::default()).unwrap();
    assert!(out.contains("failover"), "default Config must serialize [failover] section");
}

#[test]
fn missing_failover_section_defaults_to_empty() {
    let cfg: Config = toml::from_str("[model.grok]\nname = 'grok'\n").expect("parse");
    assert!(cfg.failover.order.is_empty());
}
```

- [ ] **Step 2: Run test, verify FAIL**

Run: `cargo test -p xai-grok-shell --lib failover`
Expected: FAIL — compile error "no field `failover` on type `Config`" (or missing struct).

- [ ] **Step 3: Implement**

In `agent/config.rs`, next to the other section structs (e.g. above `ModelsConfig`):

```rust
/// Ordered provider failover chain. Names reference `[model.<name>]` keys
/// plus the built-in Grok session entry.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FailoverConfig {
    /// Provider names tried in order when the selected model fails fatally
    /// before producing output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
}
```

On `Config` (~line 1370), beside `models`:

```rust
#[serde(default)]
pub failover: FailoverConfig,
```

- [ ] **Step 4: Run tests, verify PASS**

Run: `cargo test -p xai-grok-shell --lib failover && cargo clippy -p xai-grok-shell -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/config.rs
git commit -m "feat: add [failover] order section to shell config"
```

---

### Task 2: Chain walk inside sampler actor + new events

**Files:**
- Modify: `crates/codegen/xai-grok-sampling-types/src/events.rs` (append variants to `SamplingEvent`)
- Modify: `crates/codegen/xai-grok-sampling-types/src/types.rs` (add variant to `SamplingError`)
- Modify: `crates/codegen/xai-grok-sampler/src/commands.rs` (`SamplerCommand::UpdateChain`)
- Modify: `crates/codegen/xai-grok-sampler/src/actor/state.rs` (chain field)
- Modify: `crates/codegen/xai-grok-sampler/src/actor/mod.rs` (Submit arm passes chain entry)
- Modify: `crates/codegen/xai-grok-sampler/src/handle.rs` (`update_chain`)
- Modify: `crates/codegen/xai-grok-sampler/src/actor/request_task.rs` (chain walk wrapper)
- Modify: `crates/codegen/xai-grok-sampler/src/lib.rs` (exports)
- Test: `crates/codegen/xai-grok-sampler/tests/test_actor.rs` (append; reuse its `MockServer`, `test_config`, `user_request`, `sse_events_to_axum`, `text_chunk_event` helpers)

**Interfaces:**
- Consumes: `run_request_task(request_id, request, config, retry_policy, event_tx, cancel_token, completion_tx) -> RequestId` (`request_task.rs:84`); `AttemptOutcome::{Completed, Failed{error}, Cancelled}` (`request_task.rs:50`); `MockServer` harness from `tests/test_actor.rs`.
- Produces (used by Tasks 3–6):
  ```rust
  // events.rs — appended to SamplingEvent:
  ProviderSkipped { request_id: RequestId, name: Arc<str>, reason: Arc<str> },
  ProviderRolledOver { request_id: RequestId, from: Arc<str>, to: Arc<str>, reason: String },
  ProviderFailed { request_id: RequestId, providers: Vec<String> },
  // types.rs — appended to SamplingError:
  #[error("all providers failed: {}", providers.join(" -> "))]
  ProviderFailed { providers: Vec<String> },
  // handle.rs:
  pub async fn update_chain(&self, chain: Vec<(String, SamplerConfig)>);
  // commands.rs:
  UpdateChain { chain: Box<Vec<(String, SamplerConfig)>> },
  ```
  Chain type used everywhere: `pub type FailoverChain = Vec<(String, SamplerConfig)>;` (defined in `sampler/src/lib.rs`, re-exported). Also adds ONE field to `SamplerConfig`: `#[serde(default)] pub keyless: bool` (skip-rule escape hatch for Ollama-style entries; default false keeps all existing configs unchanged).

- [ ] **Step 1: Write the five failing chain-walk integration tests** (append to `tests/test_actor.rs`; helpers already exist there):

```rust
use xai_grok_sampler::FailoverChain;

fn cfg_named(base_url: &str, model: &str) -> SamplerConfig {
    let mut c = test_config(base_url, model);
    c.model = model.to_string();
    c
}

async fn collect_terminal(
    event_rx: &mut mpsc::UnboundedReceiver<SamplingEvent>,
) -> Option<SamplingEvent> {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .ok()??;
        match ev {
            SamplingEvent::Failed { .. }
            | SamplingEvent::ProviderFailed { .. }
            | SamplingEvent::Cancelled { .. } => return Some(ev),
            _ => continue,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_first_provider_success_never_rolls_over() {
    let server = MockServer::spawn(sse_events_to_axum(vec![text_chunk_event("hi", "stop")])).await;
    let mut good = test_config(&server.base_url(), "grok");
    good.model = "grok".into();
    let dead = "http://127.0.0.1:9"; // closed port => connection refused => Fatal
    let chain: FailoverChain =
        vec![("primary".into(), good), ("backup".into(), test_config(dead, "backup"))];

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle =
        SamplerActor::spawn(test_config(dead, "unused"), RetryPolicy::default(), event_tx.clone());
    handle.update_chain(chain).await;
    handle.submit(user_request("say hi"), None);

    let mut saw_completed = false;
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await.ok() {
        match ev {
            SamplingEvent::Completed { .. } => { saw_completed = true; }
            SamplingEvent::ProviderRolledOver { .. } | SamplingEvent::ProviderFailed { .. } =>
                panic!("first provider succeeded; no rollover noise expected"),
            _ => {}
        }
    }
    assert!(saw_completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_rolls_over_on_fatal_before_output() {
    let dead = "http://127.0.0.1:9";
    let server = MockServer::spawn(sse_events_to_axum(vec![text_chunk_event("saved", "stop")])).await;
    let chain: FailoverChain = vec![
        ("dead".into(), test_config(dead, "dead")),
        ("alive".into(), test_config(&server.base_url(), "alive")),
    ];
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle =
        SamplerActor::spawn(test_config(dead, "seed"), RetryPolicy::default(), event_tx.clone());
    handle.update_chain(chain).await;
    handle.submit(user_request("hello"), None);

    let mut saw_rollover = false;
    let mut saw_completed = false;
    while let Some(ev) =
        tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await.ok()
    {
        match ev {
            SamplingEvent::ProviderRolledOver { from, to, .. } => {
                assert_eq!((from.as_ref(), to.as_ref()), ("dead", "alive"));
                saw_rollover = true;
            }
            SamplingEvent::Completed { .. } => { saw_completed = true; break; }
            SamplingEvent::ProviderFailed { providers, .. } =>
                panic!("chain should not be exhausted, got {providers:?}"),
            _ => {}
        }
    }
    assert!(saw_rollover && saw_completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_does_not_roll_over_after_partial_output() {
    // First provider streams one chunk then dies mid-stream (connection aborted).
    // The request must FAIL against that provider — no rollover, because output
    // was already observed.
    async fn chunk_then_die() -> axum::response::Response {
        let sse = concat!(
            "data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
        );
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(sse))
            .unwrap()
        // Stream ends abruptly without [DONE] => transport error mid-stream,
        // after a text chunk was already emitted as output.
    }
    let killer =
        MockServer::spawn(axum::Router::new().route("/v1/chat/completions", axum::routing::post(chunk_then_die)))
            .await;
    let chain: FailoverChain = vec![
        ("partial".into(), test_config(&killer.base_url(), "m")),
        ("next".into(), test_config("http://127.0.0.1:9", "n")),
    ];
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        test_config(&killer.base_url(), "m"),
        RetryPolicy::default(),
        event_tx.clone(),
    );
    handle.update_chain(chain).await;
    handle.submit(user_request("go"), None);

    match collect_terminal(&mut event_rx).await.expect("terminal") {
        SamplingEvent::Failed { error, .. } => {
            assert!(
                !matches!(&*error, xai_grok_sampling_types::SamplingError::ProviderFailed { .. }),
                "must be the underlying transport failure, not chain exhaustion"
            );
        }
        SamplingEvent::ProviderFailed { providers, .. } =>
            panic!("must NOT roll over after partial output; got exhausted {providers:?}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_exhaustion_emits_provider_failed_error() {
    let chain: FailoverChain = vec![
        ("a".into(), test_config("http://127.0.0.1:9", "ma")),
        ("b".into(), test_config("http://127.0.0.1:9", "mb")),
    ];
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(test_config("http://127.0.0.1:9", "seed"), RetryPolicy::default(), event_tx.clone());
    handle.update_chain(chain).await;
    handle.submit(user_request("hi"), None);

    match collect_terminal(&mut event_rx).await.expect("terminal") {
        SamplingEvent::ProviderFailed { providers, .. } =>
            assert_eq!(providers, vec!["a", "b"]),
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_entry_without_api_key_is_skipped() {
    let server = MockServer::spawn(sse_events_to_axum(vec![text_chunk_event("ok", "stop")])).await;
    let mut keyed = test_config(&server.base_url(), "g");
    keyed.api_key = Some("sk-test".into());

    // A keyless entry: SamplerConfig with no api_key. The walker's skip rule
    // (implemented in Step 5) is: api_key.is_none() => ProviderSkipped.
    let mut keyless = test_config(&server.base_url(), "k");
    keyless.api_key = None;
    let chain: FailoverChain = vec![("keyless".into(), keyless), ("keyed".into(), keyed)];

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let seed = test_config("http://127.0.0.1:9", "seed");
    let handle = SamplerActor::spawn(seed, RetryPolicy::default(), event_tx.clone());
    handle.update_chain(chain).await;
    handle.submit(user_request("hi"), None);

    let mut saw_skip = false;
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await.ok() {
        match ev {
            SamplingEvent::ProviderSkipped { name, .. } => {
                assert_eq!(name.as_ref(), "keyless");
                saw_skip = true;
            }
            SamplingEvent::Completed { .. } => break,
            SamplingEvent::ProviderFailed { .. } => panic!("should have completed via 'keyed'"),
            _ => {}
        }
    }
    assert!(saw_skip, "ProviderSkipped must be emitted for keyless entry");
}
```

Note: `SamplerConfig` field names (`api_key`, `model`) and `SamplingEvent` variant shapes must match `src/config.rs` and `src/events.rs` exactly — check them if the compiler disagrees; adjust the TESTS' construction syntax only, never weaken the assertions.

**Skip rule decision:** the walker treats `cfg.api_key.is_none()` as skippable EXCEPT when the entry's base URL points at a known-local/keyless backend. Encode this as a single predicate so Ollama-style entries survive: add `#[serde(default)] pub keyless: bool` to `SamplerConfig` in the SAME task (`src/config.rs`, default false, no other field changes) — skip iff `cfg.api_key.is_none() && !cfg.keyless`. The skip test above then sets `keyless.api_key = None; keyless.keyless = false;` explicitly.

- [ ] **Step 2: Run tests, verify FAIL**

Run: `cargo test -p xai-grok-sampler --test test_actor chain_ 2>&1 | tail -40`
Expected: FAIL — `FailoverChain`, `update_chain`, `ProviderSkipped` etc. do not exist.

- [ ] **Step 3: Add `keyless` field + implement events and error**

`src/config.rs` — one field on `SamplerConfig`, next to `api_key`:

```rust
/// Provider needs no API key (e.g. local Ollama). Skip-rule escape hatch.
#[serde(default)]
pub keyless: bool,
```

Update the `cfg()` test helper in `actor/state.rs` (it enumerates every field) with `keyless: false`.

`sampling-types/src/events.rs` — append to `SamplingEvent`:

```rust
/// A chain entry was skipped before any attempt (missing key).
ProviderSkipped { request_id: RequestId, name: Arc<str>, reason: Arc<str> },
/// Request rolled from one provider to the next (fatal pre-output error).
ProviderRolledOver { request_id: RequestId, from: Arc<str>, to: Arc<str>, reason: String },
/// Every provider in the chain failed; carries all attempted names.
ProviderFailed { request_id: RequestId, providers: Vec<String> },
```

(`RequestId` and `Arc<str>` usage follows existing `SamplingEvent` variants in that file.)

`sampling-types/src/types.rs` — append to `SamplingError`:

```rust
/// Every provider in the failover chain failed.
#[error("all providers failed: {}", .providers.join(" -> "))]
ProviderFailed { providers: Vec<String> },
```

Export both crates' new items from their `lib.rs` if not glob-exported.

- [ ] **Step 4: Implement chain plumbing (actor/handle/state/commands)**

`src/lib.rs`:

```rust
/// Ordered failover chain: (display name, per-provider config).
pub type FailoverChain = Vec<(String, SamplerConfig)>;
pub use crate::handle::SamplerHandle; // ensure existing exports stay intact
```

`src/commands.rs`:

```rust
UpdateChain { chain: Box<Vec<(String, SamplerConfig)>> },
```

`src/actor/state.rs` — add field + method:

```rust
pub failover_chain: FailoverChain,   // default: Vec::new()
```

with update method mirroring `update_config`:

```rust
pub fn update_chain(&mut self, chain: Vec<(String, SamplerConfig)>) {
    self.failover_chain = chain;
}
```

`src/handle.rs` — mirror `update_config` exactly (copy its send/ack channel discipline verbatim; only the command variant differs):

```rust
/// Replace the failover chain used for subsequent requests.
pub async fn update_chain(&self, chain: Vec<(String, SamplerConfig)>) {
    // Same body shape as update_config: send UpdateChain { chain: Box::new(chain) }
    // through self.cmd_tx with the same ack pattern update_config uses.
}
```

`src/actor/mod.rs` — Submit arm: resolve chain vs per-request override:

```rust
match config {
    // Per-request explicit config wins: bypass the chain entirely.
    Some(overridden) => {
        self.tasks.spawn(request_task::run_request_task(
            request_id, *request_inner, *overridden, retry_policy,
            event_tx.clone(), cancel_token, completion_tx,
        ));
    }
    None => {
        let mut chain = self.state.failover_chain.clone();
        if chain.is_empty() {
            // No chain configured: behave exactly like today.
            chain = vec![("provider".into(), self.state.config.clone())];
        }
        // Start at the position matching the request's model, else entry 0.
        let start_index = chain
            .iter()
            .position(|(_, c)| Some(c.model.as_str()) == request_inner.model.as_deref())
            .unwrap_or(0);
        self.tasks.spawn(request_task::run_chain_task(
            request_id, *request_inner, chain, start_index, retry_policy,
            event_tx, cancel_token, completion_tx,
        ));
    }
}
```

(`request_inner.model: Option<String>` — verify against the actual `ConversationRequest` field; the intent is: match chain entries whose `SamplerConfig.model` equals the requested model slug.)

- [ ] **Step 5: Implement the chain walker in request_task.rs**

Rename nothing; ADD an outer function that owns the walk and delegates the existing body:

The walker reuses the existing per-entry loop verbatim by extracting it. Concrete shape:

**5a. Split `run_request_task` (`request_task.rs:84`) into an inner entry-runner.** Keep the public `run_request_task(request_id, request, config, retry_policy, event_tx, cancel_token, completion_tx) -> RequestId` signature intact — it becomes a thin wrapper spawning the walker with a single-entry chain:

```rust
pub(crate) fn run_request_task(
    request_id: RequestId,
    request: ConversationRequest,
    config: SamplerConfig,
    retry_policy: RetryPolicy,
    event_tx: mpsc::UnboundedSender<SamplingEvent>,
    cancel_token: CancellationToken,
    completion_tx: Option<CompletionTx>,
) -> RequestId {
    run_chain_task(
        request_id, request, vec![("provider".into(), config)], 0,
        retry_policy, event_tx, cancel_token, completion_tx,
    )
}
```

This keeps all existing call sites untouched: `actor/mod.rs`, `shell/src/session/acp_session_impl/spawn.rs:1266`, and test spawners (`cancel_running_task_tests.rs:2394`, `auth_retry_budget_tests.rs:137`, `chat_history_integrity_tests.rs:133`, `disk_full_tests.rs:97`, `rate_limit_backoff_tests.rs:99`, `test_doom_loop_recovery.rs:64`).

**5b. Extract today's retry-loop body into `run_one_provider`.** Move the whole existing body of `run_request_task` (client construction → InitFailed early return → sampling_span → retry loop over `run_one_attempt`/`apply_retry_decision`) into:

```rust
/// Outcome of one chain entry (the existing retry loop against ONE config).
enum EntryOutcome {
    Completed(ConversationResponse, InferenceLatencyStats),
    Failed(SamplingError),   // after that entry's own retry budget was spent
}
async fn run_one_provider(
    request_id: RequestId,
    request: ConversationRequest,
    config: SamplerConfig,
    retry_policy: RetryPolicy,
    event_tx: &mpsc::UnboundedSender<SamplingEvent>,
    cancel_token: CancellationToken,
    output_observed: Arc<AtomicBool>,
    completion_tx: &mut Option<CompletionTx>,  // None when running under the walker
) -> EntryOutcome
```

Mechanics of the extraction:
- The existing `send_completion(...)` calls inside the body become: if `completion_tx.is_some()` send through it and return `EntryOutcome::Completed`; else (walker mode) return the outcome without sending.
- The Fatal arm of `apply_retry_decision` (:475) currently emits failed-span + `emit_failed` + `send_completion(Err(fatal_err))`. Under the walker it must instead return `EntryOutcome::Failed(fatal_err)` WITHOUT emitting the terminal `Failed` event or completing — the walker decides whether to roll over or emit terminal failure. Keep the span emission.
- `output_observed` is passed IN (created fresh per entry by the walker) so the walker can read it afterwards.

**5c. The walker:**

```rust
pub(crate) fn run_chain_task(
    request_id: RequestId,
    request: ConversationRequest,
    chain: Vec<(String, SamplerConfig)>,
    start_index: usize,
    retry_policy: RetryPolicy,
    event_tx: mpsc::UnboundedSender<SamplingEvent>,
    cancel_token: CancellationToken,
    completion_tx: Option<CompletionTx>,
) -> RequestId {
    tokio::spawn(async move {
        let mut attempted: Vec<String> = Vec::new();
        for (idx, (name, cfg)) in chain.iter().enumerate().skip(start_index) {
            if cancel_token.is_cancelled() { return; }

            // Skip rule: no key and not marked keyless.
            if cfg.api_key.is_none() && !cfg.keyless {
                let _ = event_tx.send(SamplingEvent::ProviderSkipped {
                    request_id,
                    name: Arc::from(name.as_str()),
                    reason: "missing api_key".into(),
                });
                continue;
            }

            let output_observed = Arc::new(AtomicBool::new(false));
            let outcome = run_one_provider(
                request_id, request.clone(), cfg.clone(), retry_policy.clone(),
                &event_tx, cancel_token.child_token(), output_observed.clone(),
                &mut None,
            ).await;
            attempted.push(name.clone());

            match outcome {
                EntryOutcome::Completed(resp, stats) => {
                    if let Some(tx) = completion_tx {
                        let _ = tx.send(Ok((resp, stats)));
                    }
                    return;
                }
                EntryOutcome::Failed(err) => {
                    if cancel_token.is_cancelled() { return; }
                    let saw_output = output_observed.load(Ordering::Relaxed);
                    let fatal = matches!(classify_error(&err), RetryDecision::Fatal);
                    let has_next = idx + 1 < chain.len();

                    if fatal && !saw_output && has_next {
                        let next_name = chain[idx + 1].0.clone();
                        let _ = event_tx.send(SamplingEvent::ProviderRolledOver {
                            request_id,
                            from: Arc::from(name.as_str()),
                            to: Arc::from(next_name.as_str()),
                            reason: err.to_string(),
                        });
                        continue; // next chain entry
                    }
                    if fatal && !saw_output && !has_next {
                        let _ = event_tx.send(SamplingEvent::ProviderFailed {
                            request_id, providers: attempted.clone(),
                        });
                        if let Some(tx) = completion_tx {
                            let _ = tx.send(Err(SamplingError::ProviderFailed {
                                providers: attempted.clone(),
                            }));
                        }
                        return;
                    }
                    // Non-fatal-after-budget OR output already observed:
                    // surface the underlying error as-is (no rollover).
                    let _ = event_tx.send(SamplingEvent::Failed {
                        request_id, error: Box::new(err),
                    }); // match the exact shape used by emit_failed today
                    if let Some(tx) = completion_tx {
                        let _ = tx.send(Err(err));
                    }
                    return;
                }
            }
        }
        // Walked off the end with nothing completed (all skipped / cancelled path).
        let _ = event_tx.send(SamplingEvent::ProviderFailed {
            request_id, providers: attempted.clone(),
        });
        if let Some(tx) = completion_tx {
            let _ = tx.send(Err(SamplingError::ProviderFailed { providers: attempted }));
        }
    });
    request_id
}
```

(`CompletionTx` = the existing `Option<oneshot::Sender<...>>` type alias already in scope in request_task.rs — reuse whatever alias exists or write the full type.)

**Concrete decisions locked in (do not deviate):**

1. `classify_error` / `RetryDecision` come from the crate's own classifier module (grep `fn classify_error` under `src/`) — no duplicated heuristics. Fatal ⇒ roll-over candidate. This covers rate-limit exhaustion and auth failures; context-length errors are also Fatal ⇒ roll (next provider may have a bigger window).
2. Fresh `output_observed` AtomicBool per chain entry; read AFTER the entry finishes; true ⇒ never roll, emit the underlying error as-is.
3. Forward-only, no wrap-around. Chain start index computed by the actor's Submit arm (Step 4): position of the requested model among chain configs' `model` field, else 0.
4. The walker clones `request` per entry — correctness over micro-perf.
5. When the actor passes `Submit.config: Some(..)` (per-request explicit config), the Submit arm bypasses the chain entirely and spawns `run_request_task` directly (single-entry semantics preserved).

- [ ] **Step 6: Run tests, verify PASS**

Run: `cargo test -p xai-grok-sampler 2>&1 | tail -20`
Expected: all new chain tests PASS; all pre-existing sampler tests still PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/codegen/xai-grok-sampler crates/codegen/xai-grok-sampling-types
git commit -m "feat: ordered failover chain walk in sampler with provider events"
```

---

### Task 3: Gemini native backend

**Files:**
- Modify: `crates/codegen/xai-grok-sampling-types/src/types.rs:1013` (`ApiBackend` enum — add `Gemini`)
- Create: `crates/codegen/xai-grok-sampler/src/stream/gemini.rs` (L2 transform)
- Create: `crates/codegen/xai-grok-sampler/src/stream/gemini_build.rs` (request builder, pure fn)
- Modify: `crates/codegen/xai-grok-sampler/src/stream/mod.rs` (module decls + `stream_gemini` export)
- Modify: `crates/codegen/xai-grok-sampler/src/client.rs` (`conversation_stream_gemini` + dispatch arms)
- Modify: `crates/codegen/xai-grok-sampler/src/actor/request_task.rs` (`run_one_attempt` Gemini arm)
- Modify: `crates/codegen/xai-grok-sampler/src/lib.rs` (export `stream_gemini`)
- Test: `crates/codegen/xai-grok-sampler/tests/test_gemini.rs` (new integration-test binary using `MockServer` pattern copied from `tests/test_actor.rs`)

**Interfaces:**
- Consumes: `stream_messages<'a>(raw_stream, model_metadata, request_id, idle_timeout) -> impl Stream<Item=SamplingEvent>` shape (`stream/messages.rs`); `ConversationItem::{System, User, Assistant, ToolResult}` fields; `ToolSpec { name, description, parameters }`; `ConversationRequest { items, tools, model, temperature, max_output_tokens, top_p, .. }`; `ApiBackend` enum at `sampling-types/src/types.rs:1013`; `SamplingClient::conversation_collect` dispatch at `client.rs:2088`.
- Produces:
  ```rust
  // stream/gemini_build.rs
  pub(crate) fn build_gemini_request(req: &ConversationRequest) -> serde_json::Value;
  // stream/gemini.rs
  pub fn stream_gemini<'a>(
      raw_stream: BoxStream<'a, Result<MessageStreamEvent, SamplingError>>,
      model_metadata: Option<ResponseModelMetadata>,
      request_id: RequestId,
      idle_timeout: Duration,
  ) -> impl Stream<Item = SamplingEvent> + Send + 'a;
  // client.rs
  pub async fn conversation_stream_gemini(&self, request: ConversationRequest)
      -> Result<(BoxStream<'static, Result<messages::MessageStreamEvent>>, Option<ResponseModelMetadata>)>;
  ```

- [ ] **Step 1: Add `ApiBackend::Gemini`** to the enum at `types.rs:1013`:

```rust
pub enum ApiBackend {
    #[default]
    ChatCompletions,
    Responses,
    Messages,
    /// Google Gemini native generateContent API (SSE streaming).
    Gemini,
}
```

- [ ] **Step 2: Write failing mapping tests** — new file `tests/test_gemini.rs`, reusing the `MockServer` + `test_config` harness (copy those helpers from `tests/test_actor.rs` into a small `tests/common/mod.rs` shared module OR duplicate locally in `test_gemini.rs` — duplication is fine here, prefer local copy to avoid touching the other test binary):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_maps_system_user_assistant_and_streams_text() {
    // Server asserts the Gemini wire format and replies with Gemini SSE.
    let app = axum::Router::new().route(
        "/v1beta/models/gemini-pro:streamGenerateContent",
        axum::routing::post(assert_and_sse),
    );
    let server = MockServer::spawn(app).await;

    let mut cfg = test_config(&server.base_url(), "gemini-pro");
    cfg.api_backend = ApiBackend::Gemini;
    cfg.api_key = Some("goog-key".into());

    let req = user_request_with_system("be terse", "say hi");
    let resp = submit_and_collect(cfg, req).await;

    assert_eq!(resp.items.len(), 1);
    // text content assertion on items[0] per AssistantItem shape
}

async fn assert_and_sse(
    axum::extract::State(captures): axum::extract::State<Arc<Mutex<Vec<serde_json::Value>>>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> impl axum::response::IntoResponse {
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    captures.lock().unwrap().push(json.clone());
    assert_eq!(headers.get("x-goog-api-key").unwrap(), "goog-key");

    // Wire-shape assertions:
    assert_eq!(json["systemInstruction"]["parts"][0]["text"], "be terse");
    assert_eq!(json["contents"][0]["role"], "user");
    assert_eq!(json["contents"][0]["parts"][0]["text"], "say hi");

    let sse = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"he\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"llo\"}],\"role\":\"model\"}},",
        "\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}\n\n",
        "data: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[],\"role\":\"model\"}}]}\n\n",
    );
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(sse))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_tool_call_round_trip() {
    let captured: SharedCaptures = Arc::default();
    let app = axum::Router::new().route(
        "/v1beta/models/gemini-pro:streamGenerateContent",
        axum::routing::post(move |State(c): State<SharedCaptures>, body: String| async move {
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            c.lock().unwrap().push(json.clone());
            assert!(json["tools"][0]["function_declarations"][0]["name"] == "get_weather");

            let sse = concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[",
                "{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Lisbon\"}}}",
                ",{\"text\":\"checking\"}],\"role\":\"model\"}}]}\n\n",
                "data: {\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[],\"role\":\"model\"}},",
                "\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"totalTokenCount\":13}}]}\n\n",
            );
            sse_response(sse)
        }),
    );
    let server = MockServer::spawn(app).await;
    let mut cfg = test_config(&server.base_url(), "gemini-pro");
    cfg.api_backend = ApiBackend::Gemini;
    cfg.api_key = Some("goog-key".into());

    let mut req = user_request_with_system("you are a weather bot", "weather in Lisbon?");
    req.tools = vec![ToolSpec {
        name: "get_weather".into(),
        description: Some("Get current weather".into()),
        parameters: serde_json::json!({"type":"object","properties":{"city":{"type":"string"}}}),
    }];
    let resp = submit_and_collect(cfg, req).await;

    let item = resp.items.iter().find_map(|i| match i {
        ConversationItem::Assistant(a) if !a.tool_calls.is_empty() => Some(a),
        _ => None,
    }).expect("assistant tool call item");
    let tc = &item.tool_calls[0];
    assert_eq!(tc.name, "get_weather");
    assert!(tc.arguments_json().contains("Lisbon"));
    assert_eq!(item.content.as_ref(), "checking");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_sse_error_event_fails_attempt() {
    let sse = "data: {\"error\":{\"code\":429,\"message\":\"quota exceeded\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n";
    let app = axum::Router::new().route(
        "/v1beta/models/gemini-pro:streamGenerateContent",
        axum::routing::post(move || async move { sse_response(sse) }),
    );
    let server = MockServer::spawn(app).await;
    let mut cfg = test_config(&server.base_url(), "gemini-pro");
    cfg.api_backend = ApiBackend::Gemini;
    cfg.api_key = Some("goog-key".into());

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);
    handle.submit(user_request("hi"), None);

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await.ok().flatten();
        match ev.expect("terminal event") {
            SamplingEvent::Failed { error, .. } => {
                assert!(error.to_string().contains("quota exceeded"));
                break;
            }
            SamplingEvent::ProviderFailed { .. } => panic!("single-entry chain must surface Api error"),
            _ => continue,
        }
    }
}
```

Fill `submit_and_collect` using the existing actor harness: spawn actor with cfg, `handle.submit_and_collect(user_request_with_system(..))` awaiting `(ConversationResponse, _)`.

Shared helpers to define at the top of `test_gemini.rs`:

```rust
type SharedCaptures = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

fn sse_response(body: &str) -> axum::response::Response {
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

fn user_request_with_system(system: &str, user: &str) -> ConversationRequest {
    // Mirror tests/test_actor.rs `user_request`, plus a leading
    // ConversationItem::System { content: Arc::from(system) }.
    let mut req = user_request(user);
    req.items.insert(0, ConversationItem::System(
        xai_grok_sampling_types::conversation::SystemItem { content: system.into() },
    ));
    req
}

async fn submit_and_collect(cfg: SamplerConfig, req: ConversationRequest) -> ConversationResponse {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);
    let (resp, _stats) = handle.submit_and_collect(req).await.expect("collect");
    resp
}
```

(Adjust struct-literal names — `SystemItem`, `ToolCall` accessors like `arguments_json()` — against `xai-grok-sampling-types/src/conversation.rs` as compiled; fix test-side names only, never weaken assertions.)

- [ ] **Step 3: Run, verify FAIL** — `cargo test -p xai-grok-sampler --test test_gemini` → compile error: no `Gemini` backend route.

- [ ] **Step 4: Build `stream/gemini_build.rs`** (pure mapping, unit-testable):

```rust
//! ConversationRequest -> Gemini streamGenerateContent JSON body.

use serde_json::{json, Value};
use xai_grok_sampling_types::conversation::{ConversationItem, ConversationRequest};

pub(crate) fn build_gemini_request(req: &ConversationRequest) -> Value {
    let mut system_parts: Vec<Value> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    // ToolResult carries tool_call_id, but Gemini functionResponse needs the
    // tool NAME — remember id -> name from prior assistant tool_calls.
    let mut tool_name_by_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for item in &req.items {
        match item {
            ConversationItem::System(sys) => {
                system_parts.push(json!({ "text": sys.content.as_ref() }));
            }
            ConversationItem::User(u) => {
                let parts: Vec<Value> = u
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text } => Some(json!({ "text": text.as_ref() })),
                        // Image input unsupported in this pass (text-only failover)
                        ContentPart::Image { .. } => None,
                    })
                    .collect();
                contents.push(json!({ "role": "user", "parts": parts }));
            }
            ConversationItem::Assistant(a) => {
                let mut parts: Vec<Value> = Vec::new();
                if !a.content.is_empty() {
                    parts.push(json!({ "text": a.content.as_ref() }));
                }
                for tc in &a.tool_calls {
                    tool_name_by_id.insert(tc.id.to_string(), tc.name.clone());
                    // `arguments` is a JSON-encoded string (Arc<str>); Gemini
                    // wants a live object — parse, fall back to {} on junk.
                    let args: Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
                    parts.push(json!({
                        "functionCall": { "name": tc.name, "args": args }
                    }));
                }
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            ConversationItem::ToolResult(tr) => {
                let name = tool_name_by_id
                    .get(&tr.tool_call_id)
                    .cloned()
                    .unwrap_or_default();
                contents.push(json!({
                    "role": "user",
                    "parts": [{ "functionResponse": {
                        "name": name,
                        "response": { "result": tr.content.as_ref() }
                    }}]
                }));
            }
            ConversationItem::Reasoning(_) => {}       // dropped: no Gemini equivalent in v1
            ConversationItem::BackendToolCall(_) => {} // dropped: hosted-tool-only path
        }
    }
    merge_consecutive_same_role(&mut contents);

    let mut generation_config = json!({});
    if let Some(t) = req.temperature { generation_config["temperature"] = json!(t); }
    if let Some(p) = req.top_p { generation_config["topP"] = json!(p); }
    if let Some(m) = req.max_output_tokens { generation_config["maxOutputTokens"] = json!(m); }

    let mut body = json!({ "contents": contents });
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({ "parts": system_parts });
    }
    if generation_config.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        body["generationConfig"] = generation_config;
    }
    if !req.tools.is_empty() {
        body["tools"] = json!([{ "function_declarations": req.tools.iter().map(|t| json!({
            "name": t.name,
            "description": t.description.clone().unwrap_or_default(),
            "parameters": t.parameters,
        })).collect::<Vec<_>>() }]);
    }
    body
}

fn merge_consecutive_same_role(contents: &mut Vec<Value>) {
    contents.dedup_by(|b, a| {
        if a["role"] == b["role"] && !a["parts"].as_array().map(|p| p.is_empty()).unwrap_or(true) {
            let extra = b["parts"].as_array().cloned().unwrap_or_default();
            a["parts"].as_array_mut().unwrap().extend(extra);
            true
        } else {
            false
        }
    });
}
```

Notes: `ContentPart::text()` / `ToolCall::arguments_json()` / `ToolResultItem` naming — verify exact accessor names in `xai-grok-sampling-types/src/conversation.rs` and adapt (e.g. maybe `ContentPart::Text(t)` needs matching instead of a `.text()` helper). Images: skip in v1 (`ponytail: images not mapped for Gemini yet; add inlineData parts when needed`).

- [ ] **Step 5: Implement `stream/gemini.rs`** — copy the structure of `stream/messages.rs` (600-line template): yield `StreamStarted`, optional `ModelMetadata`, idle-timeout guard via `tokio::time::timeout(idle_timeout, stream.next())`, FirstToken dedup, accumulate `ChannelToken` chunks into text, map Gemini JSON fields:

- `candidates[0].content.parts[].text` → text tokens
- `candidates[0].content.parts[].functionCall { name, args }` → tool-call accumulation
- `usageMetadata { promptTokenCount, candidatesTokenCount, totalTokenCount }` → final `TokenUsage { prompt_tokens, completion_tokens: candidatesTokenCount, total_tokens, reasoning_tokens: 0, cached_prompt_tokens: 0, cache_creation_prompt_tokens: 0 }`
- `candidates[0].finishReason`: `"STOP"` → normal stop; `"MAX_TOKENS"` / `"SAFETY"` → `MaxTokensTruncation` Failed path like messages.rs StopReason::Length
- top-level `{"error": {code, message, status}}` → yield `Failed` with `SamplingError::Api { status: StatusCode::INTERNAL_SERVER_ERROR, message, model_metadata: None, retry_after_secs: None, should_retry: None, error_code: Some(status_string), }` then return
- finish: build `AssistantItem`/`ConversationResponse`/`InferenceLatencyStats::from_timestamps` exactly as messages.rs does; yield `Completed`

The raw HTTP layer parses each SSE `data:` line as Gemini JSON and feeds the L2 stream — mirror how `create_message_stream` works in client.rs: the Gemini raw stream can reuse `MessageStreamEvent` only if convenient; otherwise define a private `GeminiStreamEvent` enum inside gemini.rs and make `conversation_stream_gemini` return that. Prefer the private-event approach for honesty about wire shapes:

```rust
pub(crate) enum GeminiStreamEvent {
    Chunk(serde_json::Value),
    Error(SamplingError),
}
```

Then `stream_gemini` takes `BoxStream<'static, Result<GeminiStreamEvent, SamplingError>>` (adjust the Produces signature accordingly).

- [ ] **Step 6: Client plumbing in `client.rs`**

Add near `conversation_stream_messages` (:2023):

```rust
pub async fn conversation_stream_gemini(
    &self,
    mut request: ConversationRequest,
) -> Result<(
    BoxStream<'static, Result<crate::stream::gemini::GeminiStreamEvent, SamplingError>>,
    Option<ResponseModelMetadata>,
)> {
    self.apply_conversation_defaults(&mut request)?;
    let model = request.model.clone().unwrap_or_else(|| self.config.model.clone());
    let base = self.config.base_url.trim_end_matches('/');
    let url = format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse");
    let body = crate::stream::gemini_build::build_gemini_request(&request);

    let mut headers = HeaderMap::new();
    if let Some(key) = &self.config.api_key {
        // Bad header chars => InvalidConfiguration, same as extra_headers path.
        headers.insert("x-goog-api-key", HeaderValue::from_str(key)?);
    }

    // POST + status check: reuse `self.post(url)` (client.rs:738, returns a
    // SentRequest builder) exactly as sibling backends do — set .json(body),
    // send, map status errors with the same error mapping used by
    // conversation_stream_messages.
    let http_request = self.post(&url).json(&body);
    let response = http_request.send().await.map_err(/* same transport-error mapping as siblings */)?;
    let stream = parse_gemini_sse(response.bytes_stream());
    Ok((stream.boxed(), None))
}

/// Split SSE `data:` lines into Gemini JSON chunks; transport failures map to
/// the same SamplingError variants the sibling backends produce.
fn parse_gemini_sse(
    bytes: impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> BoxStream<'static, Result<crate::stream::gemini::GeminiStreamEvent, SamplingError>> {
    // Mirror how create_message_stream consumes SSE lines.
}
```

Dispatch arms to update:
- `conversation_collect` (:2088): add `ApiBackend::Gemini =>` mirroring the messages arm but calling `stream_gemini`.
- `run_one_attempt` (`request_task.rs:~530`): add matching arm calling `client.conversation_stream_gemini(request)`.

Auth note: `SamplingClient::new` (:572-589) inserts generic `x-api-key`/`Authorization`. Add one-line guard there: skip generic auth when `config.api_backend == ApiBackend::Gemini` (Gemini auth is the per-request `x-goog-api-key`).

- [ ] **Step 7: Run, verify PASS**

Run: `cargo test -p xai-grok-sampler --test test_gemini && cargo test -p xai-grok-sampler 2>&1 | tail -5`
Expected: gemini tests PASS; full sampler suite PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/codegen/xai-grok-sampler crates/codegen/xai-grok-sampling-types
git commit -m "feat: native Gemini streaming backend"
```

---

### Task 4: Copilot dynamic X-Request-Id header

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (attach injector when preset = copilot)
- Create: `crates/codegen/xai-grok-shell/src/util/copilot_headers.rs`
- Test: unit tests in `copilot_headers.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `HeaderInjector` trait (`sampler/src/config.rs:186`): `fn inject(&self, headers: &mut reqwest::header::HeaderMap)`; `SharedHeaderInjector = Arc<dyn HeaderInjector>`; `SamplerConfig.header_injector: Option<SharedHeaderInjector>` (`#[serde(skip)]`).
- Produces:
  ```rust
  pub struct CopilotHeaderInjector;
  // impl HeaderInjector: sets X-Request-Id: <uuid::Uuid::new_v4()>
  ```

- [ ] **Step 1: Failing test** in `copilot_headers.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    #[test]
    fn injects_unique_request_ids() {
        let inj = CopilotHeaderInjector;
        let mut h1 = HeaderMap::new();
        let mut h2 = HeaderMap::new();
        inj.inject(&mut h1);
        inj.inject(&mut h2);
        let a = h1.get("X-Request-Id").unwrap().to_str().unwrap();
        let b = h2.get("X-Request-Id").unwrap().to_str().unwrap();
        assert_ne!(a, b, "each request gets a fresh uuid v4");
        assert_eq!(a.len(), 36);
    }
}
```

- [ ] **Step 2: Run, verify FAIL** (`cargo test -p xai-grok-shell --lib copilot` → missing module).

- [ ] **Step 3: Implement**

`crates/codegen/xai-grok-shell/src/util/copilot_headers.rs`:

```rust
//! GitHub Copilot requires a unique X-Request-Id per inference call.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use xai_grok_sampler::HeaderInjector;

pub struct CopilotHeaderInjector;

impl std::fmt::Debug for CopilotHeaderInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CopilotHeaderInjector")
    }
}

impl HeaderInjector for CopilotHeaderInjector {
    fn inject(&self, headers: &mut HeaderMap) {
        let id = uuid::Uuid::new_v4().to_string();
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_static("x-request-id"),
            HeaderValue::from_str(&id),
        ) {
            headers.insert(name, val);
        }
    }
}
```

Wire-up: wherever Task 5 constructs the copilot `SamplerConfig` (preset application site), set `cfg.header_injector = Some(std::sync::Arc::new(CopilotHeaderInjector));`. Register the module in `util/mod.rs`. Check `xai-grok-shell/Cargo.toml`: `uuid` (v4) is present; if `reqwest` is not a direct dependency, add `reqwest = { workspace = true }` to that crate's `[dependencies]` only — root Cargo.toml untouched.

- [ ] **Step 4: Run, PASS, commit**

Run: `cargo test -p xai-grok-shell --lib copilot && cargo clippy -p xai-grok-shell -- -D warnings`

```bash
git add crates/codegen/xai-grok-shell
git commit -m "feat: copilot dynamic X-Request-Id header injector"
```

---

### Task 5: Presets + chain building + config.toml writers (shell)

**Files:**
- Create: `crates/codegen/xai-grok-shell/src/util/providers.rs` (preset table + chain builder)
- Create: `crates/codegen/xai-grok-shell/src/util/config/providers_io.rs` (toml_edit writers)
- Modify: `crates/codegen/xai-grok-shell/src/util/mod.rs` (module decls)
- Test: unit tests inside both new files

**Interfaces:**
- Consumes: `FailoverConfig.order` (Task 1); `resolve_model_list(cfg, prefetched) -> IndexMap<String, ModelEntry>` (config.rs:3520); `ModelEntry { info: ModelInfo, api_key, env_key, auth_provider, api_base_url }` (~config.rs:4380); `resolve_credentials(model, session_key) -> ResolvedCredentials { api_key, base_url, auth_type, auth_scheme }` (config.rs:4844); `sampling_config_for_model(model, credentials, alpha_test_key, client_version, deployment_id, user_id) -> SamplerConfig` (config.rs:5218); persist primitives `lock_config_writes()` + `atomic_write_string(path, content)` (persist.rs); `CopilotHeaderInjector` (Task 4).
- Produces (consumed by Task 6):
  ```rust
  pub struct ProviderPreset {
      pub label: &'static str,        // menu label, e.g. "Anthropic"
      pub short_key: &'static str,    // [model.<short_key>] config name, e.g. "anthropic"
      pub base_url: &'static str,
      pub api_backend: ApiBackend,
      pub auth_scheme: AuthScheme,     // Bearer | XApiKey
      pub keyless: bool,               // Ollama: true
      pub needs_dynamic_id: bool,      // Copilot: true
      pub extra_headers: &'static [(&'static str, &'static str)], // OpenRouter/Anthropic
      pub suggested_model: &'static str,
  }
  pub const PRESETS: &[ProviderPreset] = &[ /* short_keys: anthropic, openai, gemini, nvidia,
      ollama-local, copilot, openrouter, deepseek, cerebras, custom */ ];
  pub fn build_failover_chain(
      cfg: &Config,
      session_key: Option<&str>,
      client_version: &str,
  ) -> (FailoverChain, Vec<String> /*warnings*/);
  // providers_io.rs — all take &Config snapshot + write via toml_edit under SAVE_LOCK:
  pub fn upsert_model_entry(cfg_name: &str, fields: ModelFields) -> anyhow::Result<()>;
  pub struct ModelFields { pub base_url: String, pub api_key: Option<String>, pub api_backend: Option<String>, pub model: String, pub temperature: Option<f64>, pub max_completion_tokens: Option<u32>, pub extra_headers: Vec<(String, String)> };
  pub fn remove_model_entry(cfg_name: &str) -> anyhow::Result<()>;
  pub fn reorder_failover(order: Vec<String>) -> anyhow::Result<()>;
  ```

Preset table values (verbatim from spec §5):

| label | base_url | api_backend | notes |
|---|---|---|---|
| Anthropic | `https://api.anthropic.com/v1` | messages | `anthropic-version: 2023-06-01` extra header |
| OpenAI | `https://api.openai.com/v1` | chat_completions | |
| Gemini | `https://generativelanguage.googleapis.com` | gemini | |
| NVIDIA | `https://integrate.api.nvidia.com/v1` | chat_completions | |
| Ollama (local) | `http://localhost:11434/v1` | chat_completions | keyless |
| GitHub Copilot | `https://api.githubcopilot.com` | chat_completions | dynamic X-Request-Id |
| OpenRouter | `https://openrouter.ai/api/v1` | chat_completions | `HTTP-Referer: https://github.com/` extra header |
| DeepSeek | `https://api.deepseek.com/v1` | chat_completions | |
| Cerebras | `https://api.cerebras.ai/v1` | chat_completions | |
| Custom (OpenAI-compatible) | user-typed | chat_completions | |

- [ ] **Step 1: Failing preset-table test** (`providers.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_spec_providers_present_with_exact_urls() {
        let want = [
            ("Anthropic", "https://api.anthropic.com/v1"),
            ("OpenAI", "https://api.openai.com/v1"),
            ("Gemini", "https://generativelanguage.googleapis.com"),
            ("NVIDIA", "https://integrate.api.nvidia.com/v1"),
            ("Ollama (local)", "http://localhost:11434/v1"),
            ("GitHub Copilot", "https://api.githubcopilot.com"),
            ("OpenRouter", "https://openrouter.ai/api/v1"),
            ("DeepSeek", "https://api.deepseek.com/v1"),
            ("Cerebras", "https://api.cerebras.ai/v1"),
            ("Custom (OpenAI-compatible)", ""),
        ];
        for (label, url) in want {
            let p = PRESETS.iter().find(|p| p.label == label)
                .unwrap_or_else(|| panic!("preset {label} missing"));
            assert_eq!(p.base_url, url);
        }
        assert!(PRESETS.iter().find(|p| p.label == "Ollama (local)").unwrap().keyless);
        assert!(PRESETS.iter().find(|p| p.label == "GitHub Copilot").unwrap().needs_dynamic_id);
        assert_eq!(PRESETS.iter().find(|p| p.label == "Gemini").unwrap().api_backend, ApiBackend::Gemini);
    }
}
```

- [ ] **Step 2: Run FAIL, implement table + `build_failover_chain`, PASS**

Preset rows (one struct literal each; `auth_scheme`: XApiKey for Anthropic/Gemini-style, Bearer for the rest; Ollama sets `keyless: true`; Copilot sets `needs_dynamic_id: true`; Anthropic extra header `("anthropic-version", "2023-06-01")`, OpenRouter extra header `("HTTP-Referer", "https://github.com/")`; suggested models: `claude-sonnet-4-5`, `gpt-5`, `gemini-2.5-pro`, `meta/llama-3.3-70b-instruct`, `llama3.1`, `gpt-4o`, user's choice, `deepseek-chat`, `llama3.1-8b`, `custom-model`). Custom preset has empty base_url and is filled by the add flow.

Implementation core:

```rust
pub fn build_failover_chain(
    cfg: &Config,
    session_key: Option<&str>,
    client_version: &str,
) -> (FailoverChain, Vec<String>) {
    let mut chain: FailoverChain = Vec::new();
    let mut warnings = Vec::new();

    for name in &cfg.failover.order {
        // Preset override wins for well-known names; otherwise trust the
        // [model.<name>] entry resolved through existing machinery.
        let credentials = resolve_credentials(name, session_key);
        // NOTE: read the exact parameter list of sampling_config_for_model at
        // config.rs:5218 before writing this call — pass the session's stored
        // alpha_test_key/deployment_id/user_id values, not invented ones.
        let mut sc = sampling_config_for_model(
            name, &credentials, /*alpha_test_key*/ None, client_version,
            /*deployment_id*/ None, /*user_id*/ None,
        );

        if let Some(p) = PRESETS.iter().find(|p| name.eq_ignore_ascii_case(p.short_key)) {
            sc.base_url = p.base_url.to_string();
            sc.api_backend = p.api_backend;
            sc.auth_scheme = p.auth_scheme;
            if p.keyless { sc.keyless = true; sc.api_key = None; }
            sc.extra_headers.extend(p.extra_headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
            if p.needs_dynamic_id {
                sc.header_injector = Some(Arc::new(CopilotHeaderInjector));
            }
        }

        if sc.api_key.is_none() && !sc.keyless {
            warnings.push(format!("{name}: skipped, no api_key configured"));
            continue;
        }
        chain.push((name.clone(), sc));
    }
    (chain, warnings)
}
```

Built-in Grok entry: `[failover].order` may list `"grok"`. `resolve_credentials("grok", ..)` already resolves the session/XAI key path (own_credential > auth_provider > session_key > env); no preset row exists for it, so nothing overrides — chain gets whatever the active session uses.

Unit test for the builder (uses a `Config` built in-memory):

```rust
#[test]
fn chain_builder_skips_keyless_unmarked_entries_and_warns() {
    let cfg: Config = toml::from_str(concat!(
        "[failover]\n",
        "order = [\"ollama-local\", \"openai\", \"ghost\"]\n\n",
        "[model.ollama-local]\n",
        "base_url = \"http://localhost:11434/v1\"\n",
        "api_backend = \"chat_completions\"\n",
        "keyless = true\n",
        "[model.openai]\n",
        "base_url = \"https://api.openai.com/v1\"\n",
        "api_key = \"sk-test\"\n",
        "model = \"gpt-5\"\n",
    )).unwrap();

    let (chain, warnings) = build_failover_chain(&cfg, None, "test");
    assert_eq!(chain.len(), 2, "'openai' has key, 'ollama-local' is keyless-marked");
    assert_eq!(chain[0].0, "ollama-local");
    assert_eq!(chain[0].1.api_backend, ApiBackend::ChatCompletions);
    assert!(chain[1].1.api_key.as_deref().unwrap().starts_with("sk-test"));
    assert!(warnings.iter().any(|w| w.contains("ghost")));
}
```

Note on `ConfigModelOverride.keyless`: Task 5 also needs `[model.<name>] keyless = true` to deserialize — add `#[serde(default)] pub keyless: bool` to `ConfigModelOverride` (`agent/config.rs:4017`) and thread it into the resolved `SamplerConfig` wherever its other bool fields are applied.

- [ ] **Step 3: Failing writers tests** (`providers_io.rs`) — round-trip preserving comments:

```rust
#[test]
fn upsert_preserves_comments_and_appends_order() {
    let dir = std::env::temp_dir().join(format!("grok-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, "# my comments\n[model.grok]\nname = \"grok\"\n\n# keep me\n").unwrap();

    upsert_model_entry_at(&path, "openai", ModelFields {
        base_url: "https://api.openai.com/v1".into(),
        api_key: Some("sk-x".into()),
        api_backend: Some("chat_completions".into()),
        model: "gpt-5".into(),
        temperature: None,
        max_completion_tokens: None,
        extra_headers: vec![],
    });

    let out = std::fs::read_to_string(&path).unwrap();
    assert!(out.contains("# my comments"), "leading comment must survive");
    assert!(out.contains("# keep me"), "trailing comment must survive");
    assert!(out.contains("[model.openai]"));
    assert!(out.contains("api_key = \"sk-x\""));
    assert!(out.contains("\"openai\""), "name appended to failover.order");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn upsert_existing_entry_does_not_duplicate_order() {
    let dir = std::env::temp_dir().join(format!("grok-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "[failover]\norder = [\"openai\"]\n\n[model.openai]\nmodel = \"old\"\n",
    )
    .unwrap();

    upsert_model_entry_at(&path, "openai", ModelFields {
        base_url: "https://api.openai.com/v1".into(),
        api_key: Some("sk-y".into()),
        api_backend: Some("chat_completions".into()),
        model: "gpt-5".into(),
        temperature: None,
        max_completion_tokens: None,
        extra_headers: vec![],
    });

    let out = std::fs::read_to_string(&path).unwrap();
    assert_eq!(out.matches("\"openai\"").count(), 1, "no duplicate in order");
    assert!(out.contains("gpt-5"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn remove_and_reorder_work() {
    let dir = std::env::temp_dir().join(format!("grok-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "[failover]\norder = [\"a\", \"b\"]\n\n[model.a]\nmodel = \"ma\"\n\n[model.b]\nmodel = \"mb\"\n",
    )
    .unwrap();

    remove_model_entry_at(&path, "a").unwrap();
    let out = std::fs::read_to_string(&path).unwrap();
    assert!(!out.contains("[model.a]"));
    assert!(out.contains("[model.b]"));
    assert!(!out.contains("\"a\""), "'a' removed from order too");

    reorder_failover_at(&path, vec!["b", "c"]).unwrap();
    let out = std::fs::read_to_string(&path).unwrap();
    assert!(out.contains("order = [\"b\", \"c\"]"));
    let _ = std::fs::remove_dir_all(dir);
}
```

Every writer function comes in two flavors: `_at(path)` (pure, tested) and public wrapper that resolves the real config path, takes `lock_config_writes()`, reads file, edits via `toml_edit`, and `atomic_write_string`s. Implementation skeleton:

```rust
pub fn upsert_model_entry(cfg_name: &str, fields: ModelFields) -> anyhow::Result<()> {
    let path = config_file_path()?;
    let _guard = lock_config_writes()?;
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = raw.parse::<toml_edit::DocumentMut>()?;
    let tbl = &mut doc["model"][cfg_name];
    // toml_edit auto-creates implicit tables; set fields:
    set_str(tbl, "base_url", &fields.base_url);
    set_str(tbl, "model", &fields.model);
    if let Some(k) = &fields.api_key { set_str(tbl, "api_key", k); }
    if let Some(b) = &fields.api_backend { set_str(tbl, "api_backend", b); }
    // ... remaining Optionals; extra_headers written as [[model.<name>.extra_headers]] array-of-tables
    atomic_write_string(&path, doc.to_string())
}
```

Also: every mutation of `[model.<name>]` via add/remove must append/remove the name in `[failover].order` atomically in the SAME document edit (single write). Helper for that:

```rust
fn order_append(doc: &mut toml_edit::DocumentMut, name: &str) {
    let arr = doc["failover"]["order"]
        .or_insert(toml_edit::Item::Value(toml_edit::Array::new().into()))
        .as_array_mut()
        .expect("failover.order must be an array");
    if !arr.iter().any(|v| v.as_str() == Some(name)) {
        arr.push(name);
    }
}

fn order_remove(doc: &mut toml_edit::DocumentMut, name: &str) {
    if let Some(arr) = doc["failover"]["order"].as_array_mut() {
        let keep: Vec<toml_edit::Item> = arr
            .iter()
            .filter(|v| v.as_str() != Some(name))
            .map(|s| toml_edit::Item::Value(s.clone().into()))
            .collect();
        *arr = keep.into_iter().collect::<toml_edit::Array>().into();
    }
    // remove_model_entry_at additionally drops the table itself:
    // if let Some(t) = doc["model"].as_table_mut() { t.remove(name); }
}
```

(Adapt the exact toml_edit item-conversion incantations to what compiles — `toml_edit::value`, `.or_insert`, `Array` construction have version-specific idioms; the behavioral contract is: comment preservation + single atomic write + order sync.)

- [ ] **Step 4: Run all shell tests + clippy, commit**

Run: `cargo test -p xai-grok-shell --lib providers && cargo clippy -p xai-grok-shell -- -D warnings`

```bash
git add crates/codegen/xai-grok-shell
git commit -m "feat: provider presets, failover chain builder, toml_edit config writers"
```

---

### Task 6: `/providers` slash command + TUI panel

**Files:**
- Create: `crates/codegen/xai-grok-pager/src/slash/commands/providers.rs`
- Create: `crates/codegen/xai-grok-pager/src/views/providers_modal.rs`
- Modify: `crates/codegen/xai-grok-pager/src/slash/commands/mod.rs` (registry entry in `builtin_commands()` ~:79)
- Modify: `crates/codegen/xai-grok-pager/src/app/acp_handler/session_notification.rs` (~:1464 fallthrough arm — render `ProviderSkipped/RolledOver/Failed` updates into scrollback)
- Test: pure-state unit tests in `providers_modal.rs`; notification rendering covered by manual checklist step below

**Interfaces:**
- Consumes: `SlashCommand` trait (`slash/command.rs:250`); `McpsCommand` shape (`mcps.rs`, returns `CommandResult::Action(Action::OpenExtensionsModal{..})`); shell fns from Task 5 (`build_failover_chain`, `upsert_model_entry`, `remove_model_entry`, `reorder_failover`); sampler events (Task 2) arriving via `XaiSessionUpdate` alias in `session_notification.rs`; `scrollback.push_block(RenderBlock::system(msg))` render pattern (ImageDropped arm :1469).
- Produces: working `/providers` modal: Active-model line, Last-rollover line, per-provider rows with ●/○ status dot, keys ↑↓ move selection, ←→/x swap/reorder, `a` add (opens add flow), `r` remove, `e` edit, Enter confirm; all mutations write config.toml via Task 5 writers then hot-apply `sampler_handle.update_chain(build_failover_chain(..))`.

- [ ] **Step 1: Slash command registration** (follow mcps.rs verbatim):

```rust
pub struct ProvidersCommand;

impl crate::slash::SlashCommand for ProvidersCommand {
    fn name(&self) -> &str { "providers" }
    fn description(&self) -> &str { "Configure AI providers and failover order" }
    fn usage(&self) -> &str { "/providers" }
    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenProvidersModal)  // NEW Action variant; add to the Action enum where OpenExtensionsModal lives
    }
}
```

Register in `builtin_commands()` alongside `McpsCommand`.

- [ ] **Step 2: Modal state machine (pure, tested first)** — `views/providers_modal.rs`:

```rust
#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub name: String,
    pub base_url: String,
    pub has_key: bool,
    pub keyless: bool,   // Ollama-style: no key required
    pub is_active: bool, // matches the session's currently-selected model
}

pub struct ProvidersModalState {
    pub entries: Vec<ProviderRow>,
    pub selected: usize,
    pub active_model: String,
    pub last_rollover: Option<String>,
    pub mode: ModalMode,
}

pub enum ModalMode {
    Normal,
    Editing(EditField),
    Adding(usize /*preset index*/, EditField),
}
pub enum EditField { BaseUrl, ApiKey, ModelName, Temperature, MaxTokens }

impl ProvidersModalState {
    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() { return; }
        let len = self.entries.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len.max(1));
        self.selected = (next as usize).min(self.entries.len() - 1);
    }

    /// Swap the selected row with the one below; returns the swapped indices
    /// for `reorder_failover`, or None when at/past the last row.
    pub fn swap_with_next(&mut self) -> Option<(usize, usize)> {
        if self.selected + 1 >= self.entries.len() { return None; }
        self.entries.swap(self.selected, self.selected + 1);
        let pair = (self.selected, self.selected + 1);
        self.selected += 1;
        Some(pair)
    }

    pub fn begin_add(&mut self, preset_idx: usize) {
        self.mode = ModalMode::Adding(preset_idx, EditField::BaseUrl);
    }
}
```

(`confirm_add` stays out of the pure state struct — the modal controller calls `upsert_model_entry` then rebuilds `entries` from `build_failover_chain` output. State stays testable with zero I/O.)

Tests:

```rust
fn state_with_rows(n: usize) -> ProvidersModalState {
    ProvidersModalState {
        entries: (0..n)
            .map(|i| ProviderRow {
                name: format!("prov{i}"),
                base_url: format!("http://p{i}.example"),
                has_key: i % 2 == 1,
                is_active: false,
                keyless: false,
            })
            .collect(),
        selected: 0,
        active_model: "grok".into(),
        last_rollover: None,
        mode: ModalMode::Normal,
    }
}

#[test]
fn selection_moves_and_clamps() {
    let mut st = state_with_rows(3);
    st.move_selection(5);
    assert_eq!(st.selected, 2);
    st.move_selection(-9);
    assert_eq!(st.selected, 0);
}

#[test]
fn swap_updates_order_and_reports_indices() {
    let mut st = state_with_rows(2);
    let swapped = st.swap_with_next();
    assert_eq!(swapped, Some((0, 1)));
    assert_eq!(st.entries[0].name, "prov1");
    assert_eq!(st.entries[1].name, "prov0");
}

#[test]
fn swap_at_last_row_is_noop() {
    let mut st = state_with_rows(2);
    st.selected = 1;
    assert_eq!(st.swap_with_next(), None);
}
```

- [ ] **Step 3: Render + input wiring** — follow the existing extensions-modal structure (`views/extensions_modal.rs`) for layout/rendering conventions: title line `Providers — Active: {active_model}`, `Last rollover: {...}` line, rows `● name  base_url  [key set/keyless]`, footer key hints. Key handling routes ←→ or `x` to `swap_with_next`, `a` cycles preset picker, `r` removes with confirmation, `e` opens inline edit of selected field, Enter commits (writes via Task 5 writers, then `update_chain`). Copy exact widget/style idioms from the neighboring modal file rather than inventing new ones.

- [ ] **Step 4: Session-notification rendering** — in `session_notification.rs` fallthrough arm (~:1464), replace part of `_ => false` with three new `XaiSessionUpdate` variants (add them in shell `notification.rs` `SessionUpdate` enum next to `RetryState(RetryState)` :457):

```rust
SessionUpdate::ProviderSkipped { name, reason } => {
    scrollback.push_block(RenderBlock::system(format!("⏭ skipped {name}: {reason}")));
    true
}
SessionUpdate::ProviderRolledOver { from, to, reason } => {
    scrollback.push_block(RenderBlock::system(format!("↪ rolled over {from} → {to} ({reason})")));
    true
}
SessionUpdate::ProviderFailed { providers } => {
    scrollback.push_block(RenderBlock::system(format!("✗ all providers failed: {}", providers.join(", "))));
    true
}
```

Shell side: wherever `SamplingEvent::Provider*` events reach the shell's event pump (same place `RetryState` updates are translated), forward them as these `SessionUpdate` variants. Grep for `RetryState` emission sites in shell and mirror.

- [ ] **Step 5: Run gates + manual smoke**

Run: `cargo test -p xai-grok-pager && cargo clippy -p xai-grok-pager -- -D warnings && cargo test -p xai-grok-shell`
Manual: launch app, `/providers`, add Ollama entry with dummy URL, drag Grok after it, send a prompt with network cut to Grok endpoint → observe skip/rollover lines in scrollback and answer from fallback.

- [ ] **Step 6: Commit**

```bash
git add crates/codegen/xai-grok-pager crates/codegen/xai-grok-shell
git commit -m "feat: /providers panel with failover ordering and live rollover notices"
```

---

## Final Verification

- [ ] Full gates: `cargo fmt --all && cargo clippy --workspace -- -D warnings 2>&1 | tail -5` then `cargo test -p xai-grok-sampler -p xai-grok-sampling-types -p xai-grok-shell -p xai-grok-pager`
- [ ] Manual end-to-end: configure two fake OpenAI-compatible endpoints (one dead), set `[failover] order`, watch rollover notice + successful answer from survivor
- [ ] Confirm `~/.grok/config.toml` comments survive add/edit/reorder operations
- [ ] Confirm no `.env` usage and no keys outside config.toml
