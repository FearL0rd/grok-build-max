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
        if let Ok(val) = HeaderValue::from_str(&id) {
            headers.insert(HeaderName::from_static("x-request-id"), val);
        }
    }
}

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
