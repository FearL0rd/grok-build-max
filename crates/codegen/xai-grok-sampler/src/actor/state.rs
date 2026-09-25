//! All fields are touched only from the actor task, which processes one command at a time, so no mutex or atomic synchronization is needed.

use std::collections::HashMap;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::{RetryPolicy, SamplerConfig};
use crate::types::RequestId;

/// `cancel_token` is owned by the actor (cloned into the spawned per-request task).
/// The completion oneshot is moved into the per-request task at spawn time and is therefore not stored here.
pub(crate) struct ActiveRequest {
    pub(crate) cancel_token: CancellationToken,
}

/// Upper bound for a provider cooldown, even when the server's
/// `retry_after` is longer (e.g. weekly-limit errors). Limits how long a
/// stale skip can suppress a healthy-again provider.
pub(crate) const MAX_COOLDOWN_SECS: u64 = 300;

pub(crate) struct ActorState {
    pub(crate) active_requests: HashMap<RequestId, ActiveRequest>,
    pub(crate) config: SamplerConfig,
    pub(crate) retry_policy: RetryPolicy,
    /// Ordered failover chain. Empty = single-provider behavior using
    /// `config`.
    pub(crate) failover_chain: Vec<(String, SamplerConfig)>,
    /// Providers that recently died rate-limited, with the instant their
    /// cooldown ends. Keyed by chain entry name. Consulted by `Submit` to
    /// skip entries that would only burn the rate-limit retry budget
    /// before rolling over again.
    cooldowns: HashMap<String, std::time::Instant>,
}

impl ActorState {
    pub(crate) fn new(config: SamplerConfig, retry_policy: RetryPolicy) -> Self {
        Self {
            active_requests: HashMap::new(),
            config,
            retry_policy,
            failover_chain: Vec::new(),
            cooldowns: HashMap::new(),
        }
    }

    /// Record that `provider` failed rate-limited; skip it for
    /// `retry_after_secs` (capped at [`MAX_COOLDOWN_SECS`]). `None` uses
    /// the cap: an unsignalled rate limit is at least as long as the
    /// transport backoff the walk would otherwise spend.
    pub(crate) fn note_rate_limited(
        &mut self,
        provider: &str,
        retry_after_secs: Option<u64>,
    ) {
        let secs = retry_after_secs.unwrap_or(MAX_COOLDOWN_SECS).min(MAX_COOLDOWN_SECS);
        self.cooldowns
            .insert(provider.to_string(), std::time::Instant::now() + Duration::from_secs(secs));
    }

    /// True while `provider` is in cooldown. Expired entries are dropped
    /// lazily on read.
    pub(crate) fn is_cooled_down(&mut self, provider: &str) -> bool {
        match self.cooldowns.get(provider) {
            Some(&until) if until > std::time::Instant::now() => true,
            Some(_) => {
                self.cooldowns.remove(provider);
                false
            }
            None => false,
        }
    }

    /// Returns the previous entry if the same `request_id` was already in flight (callers should cancel the previous token before overwriting).
    pub(crate) fn register(
        &mut self,
        request_id: RequestId,
        active: ActiveRequest,
    ) -> Option<ActiveRequest> {
        self.active_requests.insert(request_id, active)
    }

    /// Remove a request from the active set without cancelling its token.
    /// The actor calls this when a per-request task exits normally.
    pub(crate) fn remove(&mut self, request_id: &RequestId) -> Option<ActiveRequest> {
        self.active_requests.remove(request_id)
    }

    /// Cancel and remove an in-flight request.
    pub(crate) fn cancel(&mut self, request_id: &RequestId) -> bool {
        if let Some(active) = self.active_requests.remove(request_id) {
            active.cancel_token.cancel();
            true
        } else {
            false
        }
    }

    /// Replace the default config.
    /// The next request submitted without an override will use this.
    pub(crate) fn update_config(&mut self, config: SamplerConfig) {
        self.config = config;
    }

    /// Replace the failover chain.
    pub(crate) fn update_chain(&mut self, chain: Vec<(String, SamplerConfig)>) {
        self.failover_chain = chain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SamplerConfig {
        SamplerConfig {
            base_url: "https://example.test".into(),
            model: "test-model".into(),
            context_window: 8192,
            ..Default::default()
        }
    }

    #[test]
    fn cancel_unknown_request_returns_false() {
        let mut state = ActorState::new(cfg(), RetryPolicy::default());
        assert!(!state.cancel(&RequestId::from("unknown")));
    }

    #[test]
    fn register_then_cancel_removes() {
        let mut state = ActorState::new(cfg(), RetryPolicy::default());
        let id = RequestId::from("req-1");
        state.register(
            id.clone(),
            ActiveRequest {
                cancel_token: CancellationToken::new(),
            },
        );
        assert_eq!(state.active_requests.len(), 1);
        assert!(state.cancel(&id));
        assert_eq!(state.active_requests.len(), 0);
    }

    #[test]
    fn register_returns_previous_when_same_id() {
        let mut state = ActorState::new(cfg(), RetryPolicy::default());
        let id = RequestId::from("req-1");
        let first = ActiveRequest {
            cancel_token: CancellationToken::new(),
        };
        let second = ActiveRequest {
            cancel_token: CancellationToken::new(),
        };
        assert!(state.register(id.clone(), first).is_none());
        assert!(state.register(id.clone(), second).is_some());
    }

    #[test]
    fn note_rate_limited_cools_provider_down() {
        let mut state = ActorState::new(cfg(), RetryPolicy::default());
        assert!(!state.is_cooled_down("grok"));
        state.note_rate_limited("grok", Some(60));
        assert!(state.is_cooled_down("grok"));
        // A different provider is unaffected.
        assert!(!state.is_cooled_down("backup"));
    }

    #[test]
    fn note_rate_limited_caps_long_retry_after() {
        let mut state = ActorState::new(cfg(), RetryPolicy::default());
        // Weekly-limit-shaped retry_after must not pin the skip for a week.
        state.note_rate_limited("grok", Some(604_800));
        assert!(
            state.cooldowns["grok"].duration_since(std::time::Instant::now()).as_secs()
                <= MAX_COOLDOWN_SECS
        );
    }

    #[test]
    fn note_rate_limited_defaults_to_cap_when_unsignalled() {
        let mut state = ActorState::new(cfg(), RetryPolicy::default());
        state.note_rate_limited("grok", None);
        assert!(
            state.cooldowns["grok"].duration_since(std::time::Instant::now()).as_secs()
                <= MAX_COOLDOWN_SECS
        );
    }
}
