//! The actor task itself is single-threaded: it processes one command at a time.
//! The actual streaming work happens in `tokio::spawn` per-request tasks, so multiple requests can be in flight concurrently.

pub(crate) mod request_metadata;
pub(crate) mod request_task;
pub(crate) mod state;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::commands::SamplerCommand;
use crate::config::{RetryPolicy, SamplerConfig};
use crate::events::SamplingEvent;
use crate::handle::SamplerHandle;
use state::{ActiveRequest, ActorState};

use crate::types::RequestId;

/// Construct via [`SamplerActor::spawn`]; the returned [`SamplerHandle`] is the only supported way to interact with it.
pub struct SamplerActor {
    cmd_rx: mpsc::UnboundedReceiver<SamplerCommand>,
    event_tx: mpsc::UnboundedSender<SamplingEvent>,
    /// Chain tasks report rate-limited providers on this channel so the
    /// actor can cooldown-skip them for later `Submit`s.
    cooldown_rx: mpsc::UnboundedReceiver<(String, Option<u64>)>,
    /// Clone of the cooldown channel's sender, handed to each spawned
    /// chain task.
    cooldown_tx: mpsc::UnboundedSender<(String, Option<u64>)>,
    state: ActorState,
    /// The actor's run loop selects on `cmd_rx.recv()` and `tasks.join_next()`.
    /// A finished task returns its `RequestId` so the actor can clean up `active_requests`.
    tasks: JoinSet<RequestId>,
}

impl SamplerActor {
    /// Spawn the actor on the current tokio runtime and return a handle.
    /// The actor stops when the returned handle (and all its clones) are dropped.
    pub fn spawn(
        config: SamplerConfig,
        retry_policy: RetryPolicy,
        event_tx: mpsc::UnboundedSender<SamplingEvent>,
    ) -> SamplerHandle {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (cooldown_tx, cooldown_rx) = mpsc::unbounded_channel();
        let actor = Self {
            cmd_rx,
            event_tx,
            cooldown_rx,
            cooldown_tx: cooldown_tx.clone(),
            state: ActorState::new(config, retry_policy),
            tasks: JoinSet::new(),
        };
        tokio::spawn(actor.run());
        SamplerHandle::new(cmd_tx)
    }

    fn cooldown_tx_handle(&self) -> mpsc::UnboundedSender<(String, Option<u64>)> {
        self.cooldown_tx.clone()
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                // Prefer cleaning up finished tasks before processing new commands, so `active_requests` does not stay stale longer than necessary
                Some(joined) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    match joined {
                        Ok(request_id) => {
                            // Task finished normally; remove from active set unless the user has already cancelled it (Cancel removes it too)
                            self.state.remove(&request_id);
                        }
                        Err(join_err) => {
                            tracing::warn!(
                                error = %join_err,
                                "request task panicked or was aborted"
                            );
                        }
                    }
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => self.handle_command(cmd),
                        None => break, // all handles dropped
                    }
                }
            }
        }

        // Cancel any still-running tasks before exiting so they don't leak
        // The cancellation token shutdown is best-effort
        for (_, active) in self.state.active_requests.drain() {
            active.cancel_token.cancel();
        }
        self.tasks.shutdown().await;
    }

    /// Drain every pending rate-limit report the chain tasks queued into
    /// the cooldown store. Bounded, cheap, and run before command handling
    /// so `Submit` sees the freshest skips.
    fn drain_cooldown_reports(&mut self) {
        while let Ok((provider, retry_after)) = self.cooldown_rx.try_recv() {
            self.state.note_rate_limited(&provider, retry_after);
        }
    }

    fn handle_command(&mut self, cmd: SamplerCommand) {
        // Chain tasks may have reported a rate-limited provider since the
        // last command; fold those in before building the next walk.
        self.drain_cooldown_reports();
        match cmd {
            SamplerCommand::Submit {
                request_id,
                request,
                config,
                completion_tx,
            } => {
                let cancel_token = CancellationToken::new();
                let active = ActiveRequest {
                    cancel_token: cancel_token.clone(),
                };
                if let Some(prev) = self.state.register(request_id.clone(), active) {
                    // Caller submitted a duplicate id; cancel the previous one so we don't leak its task
                    prev.cancel_token.cancel();
                }
                let event_tx = self.event_tx.clone();
                let retry_policy = self.state.retry_policy.clone();
                let request_inner = *request;
                // Per-request config overrides bypass the failover chain;
                // without an override we walk the installed chain (or the
                // plain config when no chain is set).
                let has_override = config.is_some();
                let mut chain: crate::FailoverChain = match config {
                    Some(config) => vec![("provider".to_string(), *config)],
                    None if !self.state.failover_chain.is_empty() => {
                        self.state.failover_chain.clone()
                    }
                    None => vec![("provider".to_string(), self.state.config.clone())],
                };
                // Start at the selected model's entry so switching models
                // does not replay providers earlier in the chain.
                let mut start_index = request_inner
                    .model
                    .as_ref()
                    .and_then(|m| chain.iter().position(|(_, c)| &c.model == m));
                if request_inner.model.is_some()
                    && start_index.is_none()
                    && !has_override
                    && !self.state.failover_chain.is_empty()
                {
                    // The selected model is not a chain entry — typically the
                    // built-in GROK selection, which lives in the session
                    // config (first-party endpoint, session auth, the user's
                    // exact model id and effort), not in [failover].order.
                    // Lead the walk with it so GROK is always the first
                    // provider tried, then the providers list; every request
                    // re-walks from the top, so a limit reset is recovered.
                    let mut led = vec![("grok".to_string(), self.state.config.clone())];
                    led.append(&mut chain);
                    chain = led;
                    start_index = Some(0);
                }
                // Cooldown skip: entries that recently died rate-limited
                // would only burn the rate-limit retry budget (2 attempts
                // plus backoff sleeps) before rolling over again. Advance
                // `start_index` past cooled entries — but only while a
                // live entry remains ahead; if every entry is cooled the
                // walk starts at the model's own entry as usual (a fully
                // cooled chain must still serve requests, not block).
                if !has_override && !chain.is_empty() {
                    let base = start_index.unwrap_or(0);
                    let mut skip = base;
                    while skip < chain.len() && self.state.is_cooled_down(&chain[skip].0) {
                        skip += 1;
                    }
                    if skip < chain.len() && skip > base {
                        start_index = Some(skip);
                    }
                }
                let start_index = start_index.unwrap_or(0);
                self.tasks.spawn(request_task::run_chain_task(
                    request_id,
                    request_inner,
                    chain,
                    start_index,
                    retry_policy,
                    event_tx,
                    cancel_token,
                    completion_tx,
                    self.cooldown_tx_handle(),
                ));
            }
            SamplerCommand::Cancel { request_id } => {
                self.state.cancel(&request_id);
            }
            SamplerCommand::UpdateConfig { config } => {
                self.state.update_config(*config);
            }
            SamplerCommand::UpdateChain { chain } => {
                self.state.update_chain(*chain);
            }
            SamplerCommand::IsActive { request_id, reply } => {
                let _ = reply.send(self.state.active_requests.contains_key(&request_id));
            }
            SamplerCommand::ActiveCount { reply } => {
                let _ = reply.send(self.state.active_requests.len());
            }
            SamplerCommand::PollChain { reply } => {
                let _ = reply.send(self.state.failover_chain.clone());
            }
        }
    }
}
