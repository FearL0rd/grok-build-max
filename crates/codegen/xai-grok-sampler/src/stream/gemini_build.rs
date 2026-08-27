//! ConversationRequest -> Gemini streamGenerateContent JSON body.
//!
//! Pure mapping, unit-testable, no I/O.

use std::collections::HashMap;

use serde_json::{Value, json};
use xai_grok_sampling_types::ContentPart;
use xai_grok_sampling_types::conversation::{ConversationItem, ConversationRequest};

pub(crate) fn build_gemini_request(req: &ConversationRequest) -> Value {
    let mut system_parts: Vec<Value> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    // ToolResult carries tool_call_id, but Gemini functionResponse needs the
    // tool NAME — remember id -> name from prior assistant tool_calls.
    let mut tool_name_by_id: HashMap<String, String> = HashMap::new();

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
                        // ponytail: images not mapped for Gemini yet; add
                        // inlineData parts when image support is needed.
                        ContentPart::Image { .. } => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            ConversationItem::Assistant(a) => {
                let mut parts: Vec<Value> = Vec::new();
                if !a.content.is_empty() {
                    parts.push(json!({ "text": a.content.as_ref() }));
                }
                for tc in &a.tool_calls {
                    tool_name_by_id.insert(tc.id.to_string(), tc.name.clone());
                    // `arguments` is a JSON-encoded string; Gemini wants a
                    // live object — parse, fall back to {} on junk.
                    let args: Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
                    parts.push(json!({
                        "functionCall": { "name": tc.name, "args": args }
                    }));
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
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
            ConversationItem::Reasoning(_) => {} // dropped: no Gemini equivalent
            ConversationItem::BackendToolCall(_) => {} // hosted-tool-only path
        }
    }
    merge_consecutive_same_role(&mut contents);

    let mut generation_config = json!({});
    if let Some(t) = req.temperature {
        generation_config["temperature"] = json!(t);
    }
    if let Some(p) = req.top_p {
        generation_config["topP"] = json!(p);
    }
    if let Some(m) = req.max_output_tokens {
        generation_config["maxOutputTokens"] = json!(m);
    }

    let mut body = json!({ "contents": contents });
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({ "parts": system_parts });
    }
    if generation_config.as_object().is_some_and(|o| !o.is_empty()) {
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

/// Gemini requires alternating roles; consecutive same-role contents
/// (e.g. assistant text + tool-result "user" turns) merge into one.
fn merge_consecutive_same_role(contents: &mut Vec<Value>) {
    contents.dedup_by(|b, a| {
        if a["role"] == b["role"] {
            let extra = b["parts"].as_array().cloned().unwrap_or_default();
            if let Some(parts) = a["parts"].as_array_mut() {
                parts.extend(extra);
                return true;
            }
        }
        false
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::ToolSpec;
    use xai_grok_sampling_types::conversation::{SystemItem, UserItem};

    fn sys(text: &str) -> ConversationItem {
        ConversationItem::System(SystemItem {
            content: std::sync::Arc::from(text),
        })
    }

    fn user(text: &str) -> ConversationItem {
        ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: std::sync::Arc::from(text),
            }],
            synthetic_reason: None,
            ..Default::default()
        })
    }

    #[test]
    fn system_user_and_sampling_params_map() {
        let req = ConversationRequest {
            items: vec![sys("be terse"), user("hi")],
            temperature: Some(0.5),
            top_p: Some(0.9),
            max_output_tokens: Some(256),
            ..Default::default()
        };
        let body = build_gemini_request(&req);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be terse");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(body["generationConfig"]["temperature"], 0.5);
        assert!(
            (body["generationConfig"]["topP"].as_f64().unwrap() - 0.9).abs() < 1e-6,
            "topP was {}",
            body["generationConfig"]["topP"]
        );
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 256);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tool_result_resolves_name_from_prior_tool_call() {
        let req = ConversationRequest {
            items: vec![
                user("weather?"),
                ConversationItem::Assistant(xai_grok_sampling_types::AssistantItem {
                    content: std::sync::Arc::from(""),
                    tool_calls: vec![xai_grok_sampling_types::ToolCall {
                        id: std::sync::Arc::from("call-1"),
                        name: "get_weather".into(),
                        arguments: std::sync::Arc::from("{\"city\":\"Lisbon\"}"),
                    }],
                    model_id: None,
                    model_fingerprint: None,
                    reasoning_effort: None,
                }),
                ConversationItem::ToolResult(xai_grok_sampling_types::ToolResultItem {
                    tool_call_id: "call-1".into(),
                    content: std::sync::Arc::from("sunny"),
                    images: Vec::new(),
                }),
            ],
            ..Default::default()
        };
        let body = build_gemini_request(&req);
        let model_turn = &body["contents"][1];
        assert_eq!(model_turn["role"], "model");
        assert_eq!(
            model_turn["parts"][0]["functionCall"]["name"],
            "get_weather"
        );
        assert_eq!(
            model_turn["parts"][0]["functionCall"]["args"]["city"],
            "Lisbon"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "get_weather"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
            "sunny"
        );
    }

    #[test]
    fn tools_map_to_function_declarations() {
        let mut req = user_request_items(vec![user("hi")]);
        req.tools = vec![ToolSpec {
            name: "get_weather".into(),
            description: Some("Get current weather".into()),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let body = build_gemini_request(&req);
        assert_eq!(
            body["tools"][0]["function_declarations"][0]["name"],
            "get_weather"
        );
        assert_eq!(
            body["tools"][0]["function_declarations"][0]["description"],
            "Get current weather"
        );
    }

    #[test]
    fn consecutive_user_turns_merge() {
        let req = user_request_items(vec![user("a"), user("b")]);
        let body = build_gemini_request(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1, "merged: {contents:?}");
        assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);
    }

    fn user_request_items(items: Vec<ConversationItem>) -> ConversationRequest {
        ConversationRequest {
            items,
            ..Default::default()
        }
    }
}
