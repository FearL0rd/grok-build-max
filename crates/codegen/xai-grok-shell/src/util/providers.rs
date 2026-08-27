//! Provider presets and the ordered failover-chain builder.
//!
//! `PRESETS` is the menu catalog shown by `/providers`; `build_failover_chain`
//! turns `[failover].order` plus `[model.<name>]` entries into the sampler's
//! [`FailoverChain`], layering preset-identity facts (base URL, backend, auth
//! scheme, extra headers, dynamic request-id injection) over whatever the
//! config resolved.

use std::sync::Arc;

use xai_grok_sampler::{ApiBackend, AuthScheme, FailoverChain, SamplerConfig};

use crate::util::copilot_headers::CopilotHeaderInjector;

/// One well-known provider row from the `/providers` add menu.
#[derive(Debug, Clone)]
pub struct ProviderPreset {
    /// Menu label, e.g. `"Anthropic"`.
    pub label: &'static str,
    /// `[model.<short_key>]` config name, e.g. `"anthropic"`.
    pub short_key: &'static str,
    /// Default endpoint; empty for `custom` (user types it).
    pub base_url: &'static str,
    pub api_backend: ApiBackend,
    pub auth_scheme: AuthScheme,
    /// Providers that need no key (local servers).
    pub keyless: bool,
    /// Providers requiring a unique per-request id header (GitHub Copilot).
    pub needs_dynamic_id: bool,
    /// Static headers every request to this provider must carry.
    pub extra_headers: &'static [(&'static str, &'static str)],
    /// Placeholder model name suggested by the add flow.
    pub suggested_model: &'static str,
}

/// Built-in provider catalog (spec §5, plus GLM Coding's OpenAI-compatible
/// coding endpoint). `custom` is the escape hatch for any other
/// OpenAI-compatible server.
pub const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        label: "Anthropic",
        short_key: "anthropic",
        base_url: "https://api.anthropic.com/v1",
        api_backend: ApiBackend::Messages,
        auth_scheme: AuthScheme::XApiKey,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[("anthropic-version", "2023-06-01")],
        suggested_model: "claude-sonnet-4-5",
    },
    ProviderPreset {
        label: "OpenAI",
        short_key: "openai",
        base_url: "https://api.openai.com/v1",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[],
        suggested_model: "gpt-5",
    },
    ProviderPreset {
        label: "Gemini",
        short_key: "gemini",
        base_url: "https://generativelanguage.googleapis.com",
        api_backend: ApiBackend::Gemini,
        auth_scheme: AuthScheme::XApiKey,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[],
        suggested_model: "gemini-2.5-pro",
    },
    ProviderPreset {
        label: "NVIDIA",
        short_key: "nvidia",
        base_url: "https://integrate.api.nvidia.com/v1",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[],
        suggested_model: "meta/llama-3.3-70b-instruct",
    },
    ProviderPreset {
        label: "Ollama (local)",
        short_key: "ollama-local",
        base_url: "http://localhost:11434/v1",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: true,
        needs_dynamic_id: false,
        extra_headers: &[],
        suggested_model: "llama3.1",
    },
    ProviderPreset {
        label: "GitHub Copilot",
        short_key: "copilot",
        base_url: "https://api.githubcopilot.com",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: false,
        needs_dynamic_id: true,
        extra_headers: &[],
        suggested_model: "gpt-4o",
    },
    ProviderPreset {
        label: "OpenRouter",
        short_key: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[("HTTP-Referer", "https://github.com/")],
        suggested_model: "openai/gpt-4o",
    },
    ProviderPreset {
        label: "DeepSeek",
        short_key: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[],
        suggested_model: "deepseek-chat",
    },
    ProviderPreset {
        label: "Cerebras",
        short_key: "cerebras",
        base_url: "https://api.cerebras.ai/v1",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[],
        suggested_model: "llama3.1-8b",
    },
    ProviderPreset {
        label: "GLM Coding",
        short_key: "glm",
        base_url: "https://api.z.ai/api/coding/paas/v4/",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[],
        suggested_model: "glm-4.6",
    },
    ProviderPreset {
        label: "Custom (OpenAI-compatible)",
        short_key: "custom",
        base_url: "",
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: AuthScheme::Bearer,
        keyless: false,
        needs_dynamic_id: false,
        extra_headers: &[],
        suggested_model: "custom-model",
    },
];

/// Walk `[failover].order`, resolve each entry through the normal model
/// machinery, and layer preset identity on top. Entries without credentials
/// (and not keyless) are skipped with a warning so one bad key doesn't kill
/// the whole chain.
///
/// A user-set `base_url` in `[model.<name>]` wins over the preset default so
/// custom endpoints survive; every other preset fact (backend, auth scheme,
/// static headers, request-id injection, keyless) always wins because it is
/// provider identity, not user preference.
pub fn build_failover_chain(
    cfg: &crate::agent::config::Config,
    session_key: Option<&str>,
    client_version: &str,
) -> (FailoverChain, Vec<String>) {
    let mut chain: FailoverChain = Vec::new();
    let mut warnings = Vec::new();

    let models = crate::agent::config::resolve_model_list(cfg, None);
    for name in &cfg.failover.order {
        let Some(entry) = models.get(name) else {
            warnings.push(format!(
                "{name}: skipped, no [model.{name}] entry and no built-in match"
            ));
            continue;
        };
        let credentials = crate::agent::config::resolve_credentials(entry, session_key);
        let mut sc = crate::agent::config::sampling_config_for_model(
            entry,
            credentials,
            None,
            Some(client_version.to_string()),
            None,
            None,
        );

        let preset = PRESETS
            .iter()
            .find(|p| name.eq_ignore_ascii_case(p.short_key));
        if let Some(p) = preset {
            if cfg
                .config_models
                .get(name)
                .and_then(|o| o.base_url.as_deref())
                .is_none()
            {
                sc.base_url = p.base_url.to_string();
            }
            sc.api_backend = p.api_backend.clone();
            sc.auth_scheme = p.auth_scheme;
            sc.extra_headers.extend(
                p.extra_headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string())),
            );
            if p.needs_dynamic_id {
                sc.header_injector = Some(Arc::new(CopilotHeaderInjector));
            }
        }
        if sc.keyless
            || preset.is_some_and(|p| p.keyless)
            || cfg.config_models.get(name).is_some_and(|o| o.keyless)
        {
            sc.keyless = true;
            sc.api_key = None;
        }

        if sc.api_key.is_none() && !sc.keyless {
            warnings.push(format!("{name}: skipped, no api_key configured"));
            continue;
        }
        chain.push((name.clone(), sc));
    }
    (chain, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(toml_src: &str) -> crate::agent::config::Config {
        let raw: toml::Value = toml::from_str(toml_src).unwrap();
        crate::agent::config::Config::new_from_toml_cfg(&raw).unwrap()
    }

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
            ("GLM Coding", "https://api.z.ai/api/coding/paas/v4/"),
            ("Custom (OpenAI-compatible)", ""),
        ];
        for (label, url) in want {
            let p = PRESETS
                .iter()
                .find(|p| p.label == label)
                .unwrap_or_else(|| panic!("preset {label} missing"));
            assert_eq!(p.base_url, url);
        }
        assert!(
            PRESETS
                .iter()
                .find(|p| p.label == "Ollama (local)")
                .unwrap()
                .keyless
        );
        assert!(
            PRESETS
                .iter()
                .find(|p| p.label == "GitHub Copilot")
                .unwrap()
                .needs_dynamic_id
        );
        assert_eq!(
            PRESETS
                .iter()
                .find(|p| p.label == "Gemini")
                .unwrap()
                .api_backend,
            ApiBackend::Gemini
        );
        assert_eq!(
            PRESETS
                .iter()
                .find(|p| p.label == "Anthropic")
                .unwrap()
                .auth_scheme,
            AuthScheme::XApiKey
        );
    }

    #[test]
    fn chain_builder_skips_keyless_unmarked_entries_and_warns() {
        let cfg = cfg_from(concat!(
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
        ));

        let (chain, warnings) = build_failover_chain(&cfg, None, "test");
        assert_eq!(
            chain.len(),
            2,
            "'openai' has key, 'ollama-local' is keyless-marked"
        );
        assert_eq!(chain[0].0, "ollama-local");
        assert_eq!(chain[0].1.api_backend, ApiBackend::ChatCompletions);
        assert_eq!(chain[0].1.base_url, "http://localhost:11434/v1");
        assert!(
            chain[1]
                .1
                .api_key
                .as_deref()
                .unwrap()
                .starts_with("sk-test")
        );
        assert!(warnings.iter().any(|w| w.contains("ghost")));
    }

    #[test]
    fn chain_builder_layers_preset_identity_and_copilot_injector() {
        let cfg = cfg_from(concat!(
            "[failover]\n",
            "order = [\"copilot\", \"openrouter\"]\n\n",
            "[model.copilot]\n",
            "api_key = \"gh-copilot-token\"\n",
            "model = \"gpt-4o\"\n",
            "[model.openrouter]\n",
            "api_key = \"sk-or\"\n",
            "model = \"openai/gpt-4o\"\n",
        ));

        let (chain, warnings) = build_failover_chain(&cfg, None, "test");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(chain.len(), 2);

        let copilot = &chain[0].1;
        assert_eq!(copilot.base_url, "https://api.githubcopilot.com");
        assert!(copilot.header_injector.is_some(), "dynamic X-Request-Id");

        let openrouter = &chain[1].1;
        assert_eq!(openrouter.base_url, "https://openrouter.ai/api/v1");
        assert!(
            openrouter
                .extra_headers
                .iter()
                .any(|(k, v)| k == "HTTP-Referer" && v == "https://github.com/")
        );
    }

    #[test]
    fn chain_builder_respects_user_base_url_over_preset() {
        let cfg = cfg_from(concat!(
            "[failover]\n",
            "order = [\"openai\"]\n\n",
            "[model.openai]\n",
            "base_url = \"https://my-proxy.internal/v1\"\n",
            "api_key = \"sk-test\"\n",
            "model = \"gpt-5\"\n",
        ));

        let (chain, warnings) = build_failover_chain(&cfg, None, "test");
        assert!(warnings.is_empty());
        assert_eq!(chain[0].1.base_url, "https://my-proxy.internal/v1");
        assert_eq!(chain[0].1.api_backend, ApiBackend::ChatCompletions);
    }
}
