//! Integration tests for the native Gemini backend (streamGenerateContent).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use xai_grok_sampling_types::conversation::{
    AssistantItem, ConversationItem, ConversationRequest, SystemItem, UserItem,
};
use xai_grok_sampling_types::{ApiBackend, ContentPart, ToolSpec};

use xai_grok_sampler::SamplingEvent;
use xai_grok_sampler::actor::SamplerActor;
use xai_grok_sampler::config::{RetryPolicy, SamplerConfig};
use xai_grok_sampler::types::RequestId;

// ---------------------------------------------------------------------------
// Harness (local copy of test_actor.rs helpers; Gemini base has no /v1 prefix)
// ---------------------------------------------------------------------------

struct MockServer {
    addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl MockServer {
    async fn spawn(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        Self { addr, shutdown_tx }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

fn gemini_config(base_url: String) -> SamplerConfig {
    SamplerConfig {
        api_key: Some("goog-key".into()),
        keyless: false,
        base_url,
        model: "gemini-pro".into(),
        max_completion_tokens: Some(1024),
        temperature: None,
        top_p: None,
        api_backend: ApiBackend::Gemini,
        auth_scheme: Default::default(),
        extra_headers: Default::default(),
        extra_response_includes: Vec::new(),
        query_params: Default::default(),
        env_http_headers: Default::default(),
        context_window: 128_000,
        force_http1: false,
        max_retries: Some(0),
        stream_tool_calls: false,
        idle_timeout_secs: Some(30),
        reasoning_effort: None,
        origin_client: None,
        client_identifier: None,
        deployment_id: None,
        user_id: None,
        client_version: None,
        attribution_callback: None,
        bearer_resolver: None,
        supports_backend_search: false,
        compactions_remaining: None,
        compaction_at_tokens: None,
        doom_loop_recovery: None,
        header_injector: None,
    }
}

fn user_request_with_system(system: &str, user: &str) -> ConversationRequest {
    ConversationRequest {
        items: vec![
            ConversationItem::System(SystemItem {
                content: Arc::from(system),
            }),
            ConversationItem::User(UserItem {
                content: vec![ContentPart::Text {
                    text: Arc::from(user),
                }],
                synthetic_reason: None,
                ..Default::default()
            }),
        ],
        ..Default::default()
    }
}

fn user_request(text: &str) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::from(text),
            }],
            synthetic_reason: None,
            ..Default::default()
        })],
        ..Default::default()
    }
}

fn sse_response(body: &str) -> axum::response::Response {
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

async fn submit_and_collect(
    cfg: SamplerConfig,
    req: ConversationRequest,
) -> xai_grok_sampling_types::ConversationResponse {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);
    let (resp, _stats) = handle
        .submit_and_collect(RequestId::from("gemini-test"), req)
        .await
        .expect("collect");
    resp
}

type SharedCaptures = Arc<Mutex<Vec<serde_json::Value>>>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_maps_system_user_assistant_and_streams_text() {
    let captured: SharedCaptures = Arc::default();
    let c = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1beta/models/gemini-pro:streamGenerateContent",
        post(
            move |headers: axum::http::HeaderMap, body: String| async move {
                let json: serde_json::Value = serde_json::from_str(&body).unwrap();
                c.lock().unwrap().push(json.clone());
                assert_eq!(
                    headers.get("x-goog-api-key").unwrap(),
                    "goog-key",
                    "Gemini auth is the x-goog-api-key header"
                );
                assert!(
                    headers.get("authorization").is_none(),
                    "generic bearer auth must not ride on Gemini requests"
                );

                // Wire-shape assertions on the request body.
                assert_eq!(json["systemInstruction"]["parts"][0]["text"], "be terse");
                assert_eq!(json["contents"][0]["role"], "user");
                assert_eq!(json["contents"][0]["parts"][0]["text"], "say hi");
                assert!(json.get("tools").is_none(), "no tools declared");

                let sse = concat!(
                    r#"data: {"candidates":[{"content":{"parts":[{"text":"he"}]}}]}"#,
                    "\n\n",
                    r#"data: {"candidates":[{"content":{"parts":[{"text":"llo"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}"#,
                    "\n\n",
                    r#"data: {"candidates":[{"finishReason":"STOP","content":{"parts":[],"role":"model"}}]}"#,
                    "\n\n",
                );
                sse_response(sse)
            },
        ),
    );
    let server = MockServer::spawn(app).await;

    let resp = submit_and_collect(
        gemini_config(server.base_url()),
        user_request_with_system("be terse", "say hi"),
    )
    .await;
    server.shutdown();

    let item = resp
        .items
        .iter()
        .find_map(|i| match i {
            ConversationItem::Assistant(a) => Some(a),
            _ => None,
        })
        .expect("assistant item");
    assert_eq!(item.content.as_ref(), "hello");
    let usage = resp.usage.expect("usage from usageMetadata");
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.completion_tokens, 2);
    assert_eq!(usage.total_tokens, 7);
    // The request reached the server exactly once, in Gemini shape.
    assert_eq!(captured.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_tool_call_round_trip() {
    let captured: SharedCaptures = Arc::default();
    let c = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1beta/models/gemini-pro:streamGenerateContent",
        post(move |body: String| async move {
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            c.lock().unwrap().push(json.clone());
            assert_eq!(
                json["tools"][0]["function_declarations"][0]["name"], "get_weather",
                "ToolSpec maps to Gemini function_declarations"
            );

            let sse = concat!(
                r#"data: {"candidates":[{"content":{"parts":["#,
                r#"{"functionCall":{"name":"get_weather","args":{"city":"Lisbon"}}}"#,
                r#",{"text":"checking"}],"role":"model"}}]}"#,
                "\n\n",
                r#"data: {"candidates":[{"finishReason":"STOP","content":{"parts":[],"role":"model"}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"totalTokenCount":13}}"#,
                "\n\n",
            );
            sse_response(sse)
        }),
    );
    let server = MockServer::spawn(app).await;

    let mut req = user_request_with_system("you are a weather bot", "weather in Lisbon?");
    req.tools = vec![ToolSpec {
        name: "get_weather".into(),
        description: Some("Get current weather".into()),
        parameters: serde_json::json!({"type":"object","properties":{"city":{"type":"string"}}}),
    }];
    let resp = submit_and_collect(gemini_config(server.base_url()), req).await;
    server.shutdown();

    let a = resp
        .items
        .iter()
        .find(|i| {
            matches!(
                i,
                ConversationItem::Assistant(AssistantItem { tool_calls, .. }) if !tool_calls.is_empty()
            )
        })
        .and_then(|i| match i {
            ConversationItem::Assistant(a) => Some(a),
            _ => None,
        })
        .expect("assistant tool call item");
    let tc = &a.tool_calls[0];
    assert_eq!(tc.name, "get_weather");
    assert!(tc.arguments.as_ref().contains("Lisbon"));
    assert_eq!(a.content.as_ref(), "checking");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_sse_error_event_fails_attempt() {
    let sse = "data: {\"error\":{\"code\":429,\"message\":\"quota exceeded\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n";
    let app = Router::new().route(
        "/v1beta/models/gemini-pro:streamGenerateContent",
        post(move || async move { sse_response(sse) }),
    );
    let server = MockServer::spawn(app).await;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        gemini_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );
    handle.submit(RequestId::from("gemini-err"), user_request("hi"));

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .ok()
            .flatten();
        match ev.expect("terminal event") {
            SamplingEvent::Failed { error, .. } => {
                assert!(
                    error.message.contains("quota exceeded"),
                    "message was {}",
                    error.message
                );
                break;
            }
            SamplingEvent::ProviderFailed { .. } => {
                panic!("single-entry chain must surface the underlying Api error")
            }
            _ => continue,
        }
    }
    server.shutdown();
}
