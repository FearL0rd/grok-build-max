//! `x.ai/providers/*` extension methods: hot-apply provider config changes
//! made through the pager `/providers` panel.

use super::{ExtResult, parse_session_id, to_ext_response};
use crate::agent::MvpAgent;
use agent_client_protocol as acp;

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/providers/reload" => {
            let Some(sid) = parse_session_id(args) else {
                return Err(acp::Error::invalid_params().data("missing sessionId"));
            };
            let result = agent.reload_provider_chain(&sid).await;
            super::to_ext_response(result)
        }
        _ => Err(acp::Error::method_not_found()),
    }
}
