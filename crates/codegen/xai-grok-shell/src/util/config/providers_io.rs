//! `config.toml` mutation helpers for the `/providers` flow.
//!
//! Every mutation round-trips through `toml_edit` so user comments and
//! formatting survive, and syncs `[failover].order` in the SAME document
//! edit — one atomic write per operation. Each writer comes in two flavors:
//! `_at(path)` (pure, unit-tested) and an async wrapper that resolves the
//! real config path under [`super::lock_config_writes`].

/// Field bag for one `[model.<name>]` entry write.
#[derive(Debug, Clone, Default)]
pub struct ModelFields {
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_backend: Option<String>,
    pub model: String,
    pub temperature: Option<f64>,
    pub max_completion_tokens: Option<u32>,
    /// Providers that need no key (Ollama-style); written as `keyless = true`.
    pub keyless: bool,
    pub extra_headers: Vec<(String, String)>,
}

fn explicit_table() -> toml_edit::Item {
    toml_edit::Item::Table(toml_edit::Table::new())
}

fn model_table<'d>(
    doc: &'d mut toml_edit::DocumentMut,
    cfg_name: &str,
) -> &'d mut toml_edit::Table {
    let models = doc["model"].or_insert(explicit_table());
    models
        .as_table_mut()
        .expect("[model] must be a table")
        .entry(cfg_name)
        .or_insert(explicit_table())
        .as_table_mut()
        .expect("[model.<name>] must be a table")
}

fn order_append(doc: &mut toml_edit::DocumentMut, name: &str) {
    let failover = doc["failover"].or_insert(explicit_table());
    let order = failover["order"].or_insert(toml_edit::Item::Value(toml_edit::Array::new().into()));
    let Some(arr) = order.as_array_mut() else {
        return;
    };
    if !arr.iter().any(|v| v.as_str() == Some(name)) {
        arr.push(name);
    }
}

fn order_remove(doc: &mut toml_edit::DocumentMut, name: &str) {
    // Read via `Table::get` — indexing `doc["failover"]` mutably would
    // auto-insert an empty `failover = {}` table on files that lack it.
    let order = doc
        .get("failover")
        .and_then(|f| f.get("order"))
        .and_then(|o| o.as_array());
    let Some(arr) = order else { return };
    let before = arr.len();
    let keep: Vec<String> = arr
        .iter()
        .filter(|v| v.as_str() != Some(name))
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if keep.len() == before {
        return;
    }
    let mut new_arr = toml_edit::Array::new();
    for entry in keep {
        new_arr.push(entry);
    }
    doc["failover"]["order"] = toml_edit::value(new_arr);
}

/// Insert or replace `[model.<cfg_name>]` and append `cfg_name` to
/// `[failover].order` (no duplicate) in a single atomic write.
pub fn upsert_model_entry_at(
    path: &std::path::Path,
    cfg_name: &str,
    fields: &ModelFields,
) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = raw.parse::<toml_edit::DocumentMut>()?;
    let tbl = model_table(&mut doc, cfg_name);

    tbl.insert("base_url", toml_edit::value(fields.base_url.as_str()));
    tbl.insert("model", toml_edit::value(fields.model.as_str()));
    if let Some(k) = &fields.api_key {
        tbl.insert("api_key", toml_edit::value(k.as_str()));
    } else {
        tbl.remove("api_key");
    }
    if let Some(b) = &fields.api_backend {
        tbl.insert("api_backend", toml_edit::value(b.as_str()));
    }
    match fields.temperature {
        Some(t) => {
            tbl.insert("temperature", toml_edit::value(t));
        }
        None => {
            tbl.remove("temperature");
        }
    }
    match fields.max_completion_tokens {
        Some(m) => {
            tbl.insert("max_completion_tokens", toml_edit::value(i64::from(m)));
        }
        None => {
            tbl.remove("max_completion_tokens");
        }
    }
    if fields.keyless {
        tbl.insert("keyless", toml_edit::value(true));
    } else {
        tbl.remove("keyless");
    }
    if fields.extra_headers.is_empty() {
        tbl.remove("extra_headers");
    } else {
        let headers = tbl
            .entry("extra_headers")
            .or_insert(explicit_table())
            .as_table_mut()
            .expect("[model.<name>.extra_headers] must be a table");
        for (k, v) in &fields.extra_headers {
            headers.insert(k, toml_edit::value(v.as_str()));
        }
    }
    order_append(&mut doc, cfg_name);

    super::atomic_write_string(path, &doc.to_string())
        .map_err(|e| anyhow::anyhow!("write config.toml failed: {e}"))
}

/// Remove `[model.<cfg_name>]` and its `[failover].order` entry in one write.
/// Missing file or entry is a no-op success.
pub fn remove_model_entry_at(path: &std::path::Path, cfg_name: &str) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    if raw.is_empty() {
        return Ok(());
    }
    let mut doc = raw.parse::<toml_edit::DocumentMut>()?;
    order_remove(&mut doc, cfg_name);
    if let Some(models) = doc["model"].as_table_mut() {
        models.remove(cfg_name);
    }

    super::atomic_write_string(path, &doc.to_string())
        .map_err(|e| anyhow::anyhow!("write config.toml failed: {e}"))
}

/// Replace `[failover].order` with `order` verbatim in one write.
pub fn reorder_failover_at(path: &std::path::Path, order: Vec<String>) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = raw.parse::<toml_edit::DocumentMut>()?;
    let mut arr = toml_edit::Array::new();
    for name in &order {
        arr.push(name.as_str());
    }
    doc["failover"]["order"] = toml_edit::value(arr);

    super::atomic_write_string(path, &doc.to_string())
        .map_err(|e| anyhow::anyhow!("write config.toml failed: {e}"))
}

fn config_path() -> std::path::PathBuf {
    crate::util::grok_home::grok_home().join("config.toml")
}

/// Insert/replace a model entry in the user's real `config.toml`.
pub async fn upsert_model_entry(cfg_name: &str, fields: ModelFields) -> anyhow::Result<()> {
    let path = config_path();
    let _guard = super::lock_config_writes().await;
    upsert_model_entry_at(&path, cfg_name, &fields)
}

/// Remove a model entry from the user's real `config.toml`.
pub async fn remove_model_entry(cfg_name: &str) -> anyhow::Result<()> {
    let path = config_path();
    let _guard = super::lock_config_writes().await;
    remove_model_entry_at(&path, cfg_name)
}

/// Rewrite `[failover].order` in the user's real `config.toml`.
pub async fn reorder_failover(order: Vec<String>) -> anyhow::Result<()> {
    let path = config_path();
    let _guard = super::lock_config_writes().await;
    reorder_failover_at(&path, order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_path() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("grok-providers-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn sample_fields(model: &str, key: &str) -> ModelFields {
        ModelFields {
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some(key.into()),
            api_backend: Some("chat_completions".into()),
            model: model.into(),
            temperature: None,
            max_completion_tokens: None,
            keyless: false,
            extra_headers: vec![],
        }
    }

    #[test]
    fn upsert_preserves_comments_and_appends_order() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            "# my comments\n[model.grok]\nname = \"grok\"\n\n# keep me\n",
        )
        .unwrap();

        upsert_model_entry_at(&path, "openai", &sample_fields("gpt-5", "sk-x")).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains("# my comments"),
            "leading comment must survive"
        );
        assert!(out.contains("# keep me"), "trailing comment must survive");
        assert!(out.contains("[model.openai]"));
        assert!(out.contains("api_key = \"sk-x\""));
        assert!(
            out.contains("\"openai\""),
            "name appended to failover.order"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn upsert_existing_entry_does_not_duplicate_order() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            "[failover]\norder = [\"openai\"]\n\n[model.openai]\nmodel = \"old\"\n",
        )
        .unwrap();

        upsert_model_entry_at(&path, "openai", &sample_fields("gpt-5", "sk-y")).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            out.matches("\"openai\"").count(),
            1,
            "no duplicate in order"
        );
        assert!(out.contains("gpt-5"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn keyless_and_extra_headers_round_trip() {
        let path = temp_config_path();
        let mut fields = sample_fields("llama3.1", "");
        fields.api_key = None;
        fields.keyless = true;
        fields.extra_headers = vec![("HTTP-Referer".into(), "https://github.com/".into())];

        upsert_model_entry_at(&path, "ollama-local", &fields).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("keyless = true"));
        assert!(
            !out.contains("api_key"),
            "no api_key line for keyless entry"
        );
        assert!(out.contains("HTTP-Referer"));
        // Flip back to keyed: keyless flag and headers must be removed.
        let keyed = sample_fields("llama3.1", "sk-z");
        upsert_model_entry_at(&path, "ollama-local", &keyed).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(!out.contains("keyless"));
        assert!(!out.contains("HTTP-Referer"));
        assert!(out.contains("api_key = \"sk-z\""));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn remove_and_reorder_work() {
        let path = temp_config_path();
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

        reorder_failover_at(&path, vec!["b".into(), "c".into()]).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("order = [\"b\", \"c\"]"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn remove_missing_entry_is_noop_success() {
        let path = temp_config_path();
        std::fs::write(&path, "# only comments\n").unwrap();
        remove_model_entry_at(&path, "ghost").unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert_eq!(out, "# only comments\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
