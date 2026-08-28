//! Hot-apply provider changes made through the pager `/providers` panel.
//!
//! `agent.cfg` is a startup snapshot; `/providers` mutations write straight
//! to `config.toml`, so applying them means re-reading the on-disk config
//! and reinstalling the chain on the session's sampler.

use crate::agent::mvp_agent::MvpAgent;
use agent_client_protocol as acp;

/// Result payload for `x.ai/providers/reload`.
#[derive(Debug, serde::Serialize)]
pub(crate) struct ProviderReloadSummary {
    /// Provider entries now installed on the session's failover chain.
    pub providers: usize,
    /// Skipped entries / config problems the user should see.
    pub warnings: Vec<String>,
}

impl MvpAgent {
    /// Rebuild the failover chain from the on-disk config and install it on
    /// the session's sampler.
    pub(crate) async fn reload_provider_chain(
        &self,
        session_id: &acp::SessionId,
    ) -> anyhow::Result<ProviderReloadSummary> {
        let raw = crate::util::config::load_effective_config_disk_only()?;
        let cfg = crate::agent::config::Config::new_from_toml_cfg(&raw)
            .map_err(|e| anyhow::anyhow!("invalid config.toml after edit: {e}"))?;
        let session_key = self
            .auth_manager
            .current_or_expired()
            .map(|a| a.key.clone());
        let client_version = self
            .client_version()
            .unwrap_or_else(|| "grok-build".to_owned());
        let (chain, warnings) = crate::util::providers::build_failover_chain(
            &cfg,
            session_key.as_deref(),
            &client_version,
        );
        let providers = chain.len();
        let handle = self
            .session_handle_waiting_for_load(session_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("session not found"))?;
        let (responds_to, rx) = tokio::sync::oneshot::channel();
        let _ = handle.cmd_tx.send(crate::session::SessionCommand::UpdateFailoverChain {
            chain,
            responds_to,
        });
        let _ = rx.await;
        Ok(ProviderReloadSummary { providers, warnings })
    }
}
