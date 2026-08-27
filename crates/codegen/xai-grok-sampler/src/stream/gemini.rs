//! Layer-2 stream transform for the native Gemini streamGenerateContent API.
//!
//! Consumes raw [`GeminiStreamEvent`]s (parsed SSE JSON payloads) and produces
//! [`SamplingEvent`]s. Pure: no I/O, no shell coupling.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};

use xai_grok_sampling_types::{
    AssistantItem, ConversationItem, ConversationResponse, ResponseModelMetadata, SamplingError,
    StopReason, TokenUsage, ToolCall,
};

use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

/// One parsed `streamGenerateContent` SSE data payload, plus the transport
/// error channel carried alongside it.
pub enum GeminiStreamEvent {
    /// A full JSON response chunk (`{"candidates": [...], ...}`).
    Chunk(serde_json::Value),
}

/// Transform a raw Gemini chunk stream into a stream of [`SamplingEvent`]s.
///
/// Same contract as [`crate::stream::stream_chat_completions`]: exactly one
/// terminal event per request; `idle_timeout` covers both transport stalls
/// and keepalive-only streams.
pub fn stream_gemini<'a>(
    raw_stream: BoxStream<'a, Result<GeminiStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    async_stream::stream! {
        let stream_start = Instant::now();
        let mut chunk_timestamps: Vec<Instant> = Vec::new();

        yield SamplingEvent::StreamStarted {
            request_id: request_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        if let Some(metadata) = model_metadata {
            yield SamplingEvent::ModelMetadata {
                request_id: request_id.clone(),
                metadata,
            };
        }

        let mut first_token_emitted = false;
        let mut model: Option<String> = None;
        let mut usage: Option<TokenUsage> = None;
        let mut finish_reason: Option<StopReason> = None;
        let mut content_acc = String::new();
        // Gemini functionCall parts arrive complete (no argument streaming),
        // so accumulation is just an ordered list, not a delta merge.
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        let mut chunk_index: u64 = 0;
        let mut message_chunk_count: u64 = 0;
        let mut last_content_chunk_at = Instant::now();
        let mut saw_candidate = false;

        let mut stream = raw_stream;
        loop {
            let next = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(next)) => next,
                Ok(None) => break, // stream ended normally
                Err(_elapsed) => {
                    let err = SamplingError::IdleTimeout {
                        elapsed_secs: idle_timeout.as_secs(),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };
            let chunk = match next {
                Ok(GeminiStreamEvent::Chunk(chunk)) => chunk,
                Err(err) => {
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            // Gemini reports failures inside a 200 OK SSE stream as a
            // top-level `error` object — surface it as an Api error.
            if let Some(err) = chunk.get("error").filter(|e| e.is_object()) {
                let status = err
                    .get("code")
                    .and_then(|c| c.as_u64())
                    .and_then(|c| reqwest::StatusCode::from_u16(c as u16).ok())
                    .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown Gemini stream error")
                    .to_string();
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&SamplingError::Api {
                        status,
                        message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                        error_code: None,
                    }),
                };
                return;
            }

            if let Some(mv) = chunk.get("modelVersion").and_then(|m| m.as_str()) && model.is_none() {
                model = Some(mv.to_string());
            }

            if let Some(u) = parse_usage(&chunk) {
                usage = Some(u); // cumulative for the response, last-write-wins
            }

            let mut chunk_has_content = false;

            for candidate in chunk
                .get("candidates")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(fr) = candidate.get("finishReason").and_then(|f| f.as_str()) {
                    finish_reason = Some(match fr {
                        "MAX_TOKENS" => StopReason::Length,
                        "SAFETY" => StopReason::ContentFilter,
                        _ => StopReason::Stop, // STOP and anything unknown
                    });
                    chunk_has_content = true;
                }

                for part in candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                    .into_iter()
                    .flatten()
                {
                    saw_candidate = true;
                    if let Some(text) = part.get("text").and_then(|t| t.as_str())
                        && !text.is_empty()
                    {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_has_content = true;
                        chunk_timestamps.push(Instant::now());
                        chunk_index += 1;
                        message_chunk_count += 1;
                        content_acc.push_str(text);
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Text,
                            text: text.to_string(),
                            chunk_index,
                        };
                    }

                    if let Some(fc) = part.get("functionCall").filter(|fc| fc.is_object()) {
                        chunk_has_content = true;
                        let name = fc
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string();
                        // Gemini has no call id; synthesize a stable one.
                        let id = format!("gemini-call-{}", tool_calls.len());
                        let args = fc.get("args").cloned().unwrap_or(serde_json::json!({}));
                        tool_calls.push(ToolCall {
                            id: std::sync::Arc::<str>::from(id),
                            name: name.clone(),
                            arguments: std::sync::Arc::<str>::from(args.to_string()),
                        });
                    }
                }
            }

            if chunk_has_content {
                last_content_chunk_at = Instant::now();
            } else if last_content_chunk_at.elapsed() > idle_timeout {
                let err = SamplingError::IdleTimeout {
                    elapsed_secs: idle_timeout.as_secs(),
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }
        }

        // ── Build the final response ─────────────────────────────────
        if !tool_calls.is_empty() {
            finish_reason = Some(StopReason::ToolCalls);
        }

        let mut items: Vec<ConversationItem> = Vec::new();
        if saw_candidate || !tool_calls.is_empty() {
            items.push(ConversationItem::Assistant(AssistantItem {
                content: std::sync::Arc::<str>::from(content_acc),
                tool_calls,
                model_id: model,
                model_fingerprint: None,
                reasoning_effort: None,
            }));
        } else {
            items.push(ConversationItem::assistant(""));
        }

        let stream_end = Instant::now();
        let metrics =
            InferenceLatencyStats::from_timestamps(stream_start, &chunk_timestamps, stream_end);

        let response = ConversationResponse {
            items,
            stop_reason: finish_reason,
            usage,
            cost_usd_ticks: None, // Gemini does not report cost
            message_chunks_emitted: message_chunk_count,
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        };

        yield SamplingEvent::Completed {
            request_id: request_id.clone(),
            response: Box::new(response),
            metrics,
        };
    }
}

fn parse_usage(chunk: &serde_json::Value) -> Option<TokenUsage> {
    let u = chunk.get("usageMetadata")?;
    Some(TokenUsage {
        prompt_tokens: u
            .get("promptTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or_default() as u32,
        completion_tokens: u
            .get("candidatesTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or_default() as u32,
        total_tokens: u
            .get("totalTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or_default() as u32,
        reasoning_tokens: u
            .get("thoughtsTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or_default() as u32,
        cached_prompt_tokens: u
            .get("cachedContentTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or_default() as u32,
        cache_creation_prompt_tokens: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::pin::pin;

    fn rid() -> RequestId {
        RequestId::from("gemini-req")
    }

    fn chunk(json: serde_json::Value) -> Result<GeminiStreamEvent, SamplingError> {
        Ok(GeminiStreamEvent::Chunk(json))
    }

    async fn collect(s: impl Stream<Item = SamplingEvent>) -> Vec<SamplingEvent> {
        let mut out = Vec::new();
        let mut s = pin!(s);
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn text_chunks_stream_then_complete() {
        let raw = stream::iter(vec![
            chunk(serde_json::json!({"candidates":[{"content":{"parts":[{"text":"he"}]}}]})),
            chunk(serde_json::json!({"candidates":[{"content":{"parts":[{"text":"llo"}]}}],
                "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}})),
            chunk(serde_json::json!({"candidates":[{"finishReason":"STOP"}]})),
        ])
        .boxed();
        let events = collect(stream_gemini(raw, None, rid(), Duration::from_secs(60))).await;

        assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
        assert!(matches!(events[1], SamplingEvent::FirstToken { .. }));
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.assistant().unwrap().content.as_ref(), "hello");
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
                let u = response.usage.as_ref().unwrap();
                assert_eq!(
                    (u.prompt_tokens, u.completion_tokens, u.total_tokens),
                    (5, 2, 7)
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_object_fails_the_stream() {
        let raw = stream::iter(vec![chunk(serde_json::json!({
            "error": {"code": 429, "message": "quota exceeded", "status": "RESOURCE_EXHAUSTED"}
        }))])
        .boxed();
        let events = collect(stream_gemini(raw, None, rid(), Duration::from_secs(60))).await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert!(error.message.contains("quota exceeded"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );
    }

    #[tokio::test]
    async fn function_call_accumulates_and_forces_tool_calls_stop() {
        let raw = stream::iter(vec![chunk(serde_json::json!({
            "candidates":[{"content":{"parts":[
                {"functionCall":{"name":"get_weather","args":{"city":"Lisbon"}}},
                {"text":"checking"}
            ]}}]
        }))])
        .boxed();
        let events = collect(stream_gemini(raw, None, rid(), Duration::from_secs(60))).await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let a = response.assistant().unwrap();
                assert_eq!(a.content.as_ref(), "checking");
                assert_eq!(a.tool_calls.len(), 1);
                assert_eq!(a.tool_calls[0].name, "get_weather");
                assert!(a.tool_calls[0].arguments.as_ref().contains("Lisbon"));
                assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_tokens_finish_reason_maps_to_length() {
        let raw = stream::iter(vec![chunk(serde_json::json!({
            "candidates":[{"finishReason":"MAX_TOKENS"}]
        }))])
        .boxed();
        let events = collect(stream_gemini(raw, None, rid(), Duration::from_secs(60))).await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Length));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
