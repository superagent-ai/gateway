//! End-to-end tests against mocked upstreams: a fake Anthropic Messages
//! server and a fake OpenAI Chat Completions server.

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct MockState {
    calls: AtomicUsize,
    fail_first_with: Mutex<Option<u16>>,
    last_body: Mutex<Option<Value>>,
    last_headers: Mutex<Option<HeaderMap>>,
    /// When set, streaming responses send these raw SSE bytes and then abort.
    broken_sse: Mutex<Option<String>>,
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn sse_body(frames: &str) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        frames.to_string(),
    )
        .into_response()
}

async fn mock_anthropic_handler(
    State(st): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    st.calls.fetch_add(1, Ordering::SeqCst);
    *st.last_body.lock().unwrap() = Some(body.clone());
    *st.last_headers.lock().unwrap() = Some(headers);
    if let Some(status) = st.fail_first_with.lock().unwrap().take() {
        return (
            axum::http::StatusCode::from_u16(status).unwrap(),
            Json(json!({"type": "error", "error": {"type": "overloaded_error", "message": "upstream overloaded"}})),
        )
            .into_response();
    }
    if body["stream"].as_bool().unwrap_or(false) {
        if let Some(partial) = st.broken_sse.lock().unwrap().clone() {
            // Flush headers + partial output before breaking, so the failure
            // happens after user-visible streaming has begun.
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(bytes::Bytes::from(partial));
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                yield Err(std::io::Error::other("connection reset"));
            };
            return (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response();
        }
        return sse_body(concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_a\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":20}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_123\",\"name\":\"bash\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\\\"npm test\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":8}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
    }
    Json(json!({
        "id": "msg_up1", "type": "message", "role": "assistant",
        "model": body["model"],
        "content": [{"type": "text", "text": "Hello from Anthropic mock"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 12, "output_tokens": 4}
    }))
    .into_response()
}

async fn mock_openai_handler(
    State(st): State<Arc<MockState>>,
    Json(body): Json<Value>,
) -> Response {
    st.calls.fetch_add(1, Ordering::SeqCst);
    *st.last_body.lock().unwrap() = Some(body.clone());
    if body["stream"].as_bool().unwrap_or(false) {
        return sse_body(concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        ));
    }
    Json(json!({
        "id": "chatcmpl-1",
        "choices": [{"finish_reason": "tool_calls", "message": {
            "content": null,
            "tool_calls": [{"id": "call_1", "type": "function",
                            "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}}]
        }}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    }))
    .into_response()
}

struct TestEnv {
    gateway: String,
    anthropic: Arc<MockState>,
    openai: Arc<MockState>,
    client: reqwest::Client,
}

async fn setup() -> TestEnv {
    let anthropic = Arc::new(MockState::default());
    let openai = Arc::new(MockState::default());
    let a_url = serve(
        Router::new()
            .route("/v1/messages", post(mock_anthropic_handler))
            .with_state(anthropic.clone()),
    )
    .await;
    let o_url = serve(
        Router::new()
            .route("/v1/chat/completions", post(mock_openai_handler))
            .with_state(openai.clone()),
    )
    .await;

    let yaml = format!(
        r#"
server:
  token: local-dev-token
fallback:
  max_attempts: 3
providers:
  mock-anthropic: {{ type: anthropic, base_url: "{a_url}", api_key: upstream-key }}
  mock-openai: {{ base_url: "{o_url}/v1", api_key: upstream-key }}
models:
  sonnet: mock-anthropic/claude-sonnet-4-6
  gpt-coder: mock-openai/gpt-5.1
  codex-only:
    model: mock-openai/gpt-5.1
    expose: [codex]
clients:
  claude_code:
    main: sonnet
    subagent: gpt-coder
    background: sonnet
  codex:
    main: sonnet
    unknown: reject
"#
    );
    let cfg = gateway::schema::load(&yaml).unwrap();
    let gateway = serve(gateway::http::build_router(cfg)).await;
    TestEnv {
        gateway,
        anthropic,
        openai,
        client: reqwest::Client::new(),
    }
}

fn auth(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    rb.header("authorization", "Bearer local-dev-token")
}

#[tokio::test]
async fn claude_code_text_to_anthropic_passthrough() {
    let env = setup().await;
    let resp = auth(
        env.client
            .post(format!("{}/anthropic/v1/messages", env.gateway)),
    )
    .header("anthropic-version", "2023-06-01")
    .header("anthropic-beta", "context-1m-2025-08-07")
    .json(&json!({
        "model": "sonnet", "max_tokens": 100,
        "messages": [{"role": "user", "content": "hello"}]
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "Hello from Anthropic mock");

    // model alias rewritten to upstream model; anthropic headers forwarded
    let sent = env.anthropic.last_body.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "claude-sonnet-4-6");
    let headers = env.anthropic.last_headers.lock().unwrap().clone().unwrap();
    assert_eq!(
        headers.get("anthropic-beta").unwrap(),
        "context-1m-2025-08-07"
    );
    assert_eq!(headers.get("x-api-key").unwrap(), "upstream-key");
}

#[tokio::test]
async fn claude_code_tools_to_openai_compatible() {
    let env = setup().await;
    let resp = auth(env.client.post(format!("{}/v1/messages", env.gateway)))
        .json(&json!({
            "model": "gpt-coder", "max_tokens": 100,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "run ls"}]}],
            "tools": [{"name": "bash", "description": "Run shell commands",
                       "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(body["content"][0]["type"], "tool_use");
    assert_eq!(body["content"][0]["name"], "bash");
    assert_eq!(body["content"][0]["input"], json!({"cmd": "ls"}));

    // upstream received OpenAI Chat shape
    let sent = env.openai.last_body.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "gpt-5.1");
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "bash");
}

#[tokio::test]
async fn codex_text_to_anthropic() {
    let env = setup().await;
    let resp = auth(
        env.client
            .post(format!("{}/openai/v1/responses", env.gateway)),
    )
    .json(&json!({
        "model": "sonnet",
        "input": [{"role": "user", "content": "Fix the failing test"}]
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(
        body["output"][0]["content"][0]["text"],
        "Hello from Anthropic mock"
    );
    assert_eq!(body["usage"]["total_tokens"], 16);

    // upstream received Anthropic Messages shape
    let sent = env.anthropic.last_body.lock().unwrap().clone().unwrap();
    assert_eq!(sent["model"], "claude-sonnet-4-6");
    assert_eq!(
        sent["messages"][0]["content"][0]["text"],
        "Fix the failing test"
    );
    assert_eq!(sent["max_tokens"], 8192);
}

#[tokio::test]
async fn codex_streaming_function_call_from_anthropic() {
    let env = setup().await;
    let resp = auth(
        env.client
            .post(format!("{}/openai/v1/responses", env.gateway)),
    )
    .json(&json!({
        "model": "main", "stream": true,
        "input": [{"role": "user", "content": "run tests"}],
        "tools": [{"type": "function", "name": "bash", "parameters": {"type": "object"}}]
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let text = resp.text().await.unwrap();
    assert!(text.contains("event: response.created"));
    assert!(text.contains("event: response.output_item.added"));
    assert!(text.contains("event: response.function_call_arguments.delta"));
    assert!(text.contains("event: response.completed"));
    let completed: Value = serde_json::from_str(
        text.split("event: response.completed\ndata: ")
            .nth(1)
            .unwrap()
            .trim(),
    )
    .unwrap();
    let fc = &completed["response"]["output"][0];
    assert_eq!(fc["type"], "function_call");
    assert_eq!(fc["call_id"], "toolu_123");
    assert_eq!(fc["arguments"], "{\"cmd\":\"npm test\"}");
}

#[tokio::test]
async fn retryable_status_before_stream_falls_back() {
    let env = setup().await;
    *env.anthropic.fail_first_with.lock().unwrap() = Some(429);
    let resp = auth(env.client.post(format!("{}/v1/messages", env.gateway)))
        .json(&json!({
            "model": "sonnet", "max_tokens": 100,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        env.anthropic.calls.load(Ordering::SeqCst),
        2,
        "expected retry after 429"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "Hello from Anthropic mock");
}

#[tokio::test]
async fn non_retryable_upstream_error_forwarded_verbatim() {
    let env = setup().await;
    *env.anthropic.fail_first_with.lock().unwrap() = Some(400);
    let resp = auth(env.client.post(format!("{}/v1/messages", env.gateway)))
        .json(&json!({
            "model": "sonnet", "max_tokens": 100,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(env.anthropic.calls.load(Ordering::SeqCst), 1);
    let body: Value = resp.json().await.unwrap();
    // exact upstream wording preserved
    assert_eq!(body["error"]["message"], "upstream overloaded");
    assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn stream_failure_after_output_does_not_fall_back() {
    let env = setup().await;
    *env.anthropic.broken_sse.lock().unwrap() = Some(concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_a\",\"usage\":{\"input_tokens\":5}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
    ).to_string());
    let resp = auth(
        env.client
            .post(format!("{}/openai/v1/responses", env.gateway)),
    )
    .json(&json!({
        "model": "sonnet", "stream": true,
        "input": [{"role": "user", "content": "hello"}]
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("\"delta\":\"partial\""),
        "partial output relayed"
    );
    assert!(text.contains("event: error"), "error surfaced to client");
    assert!(!text.contains("event: response.completed"));
    assert_eq!(
        env.anthropic.calls.load(Ordering::SeqCst),
        1,
        "no fallback after user-visible output"
    );
}

#[tokio::test]
async fn claude_streaming_via_openai_upstream() {
    let env = setup().await;
    let resp = auth(env.client.post(format!("{}/v1/messages", env.gateway)))
        .json(&json!({
            "model": "gpt-coder", "max_tokens": 100, "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("event: message_start"));
    assert!(text.contains("\"text\":\"hi\""));
    assert!(text.contains("event: message_stop"));
}

#[tokio::test]
async fn models_filtered_per_client_and_auth_enforced() {
    let env = setup().await;

    let unauthorized = env
        .client
        .post(format!("{}/v1/messages", env.gateway))
        .json(&json!({"model": "sonnet", "max_tokens": 1, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let err: Value = unauthorized.json().await.unwrap();
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "authentication_error");

    let get_ids = |url: String| {
        let client = env.client.clone();
        async move {
            let v: Value = auth(client.get(url))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };
    let claude_ids = get_ids(format!("{}/anthropic/v1/models", env.gateway)).await;
    assert!(claude_ids.contains(&"sonnet".to_string()));
    assert!(
        !claude_ids.contains(&"codex-only".to_string()),
        "blocked model hidden from Claude Code"
    );
    let codex_ids = get_ids(format!("{}/openai/v1/models", env.gateway)).await;
    assert!(codex_ids.contains(&"codex-only".to_string()));

    // x-api-key accepted as alternative auth header
    let ok = env
        .client
        .get(format!("{}/v1/models", env.gateway))
        .header("x-api-key", "local-dev-token")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
}

#[tokio::test]
async fn count_tokens_estimates_for_openai_route() {
    let env = setup().await;
    let resp = auth(
        env.client
            .post(format!("{}/v1/messages/count_tokens", env.gateway)),
    )
    .json(&json!({
        "model": "gpt-coder",
        "messages": [{"role": "user", "content": "some prompt text to count"}]
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["input_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn capability_mismatch_returns_protocol_shaped_error() {
    let env = setup().await;
    // codex-only is blocked for Claude Code
    let resp = auth(env.client.post(format!("{}/v1/messages", env.gateway)))
        .json(&json!({"model": "codex-only", "max_tokens": 1, "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "gateway_capability_error");

    // Claude Code background tasks pin claude-haiku-* ids -> background role
    // (sonnet -> anthropic mock)
    let resp = auth(env.client.post(format!("{}/v1/messages", env.gateway)))
        .json(
            &json!({"model": "claude-haiku-4-5-20260101", "max_tokens": 10,
                       "messages": [{"role": "user", "content": "hello"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "Hello from Anthropic mock");

    // Subagents pin claude-opus-* ids -> subagent role (gpt-coder -> openai mock)
    let openai_calls_before = env.openai.calls.load(Ordering::SeqCst);
    let resp = auth(env.client.post(format!("{}/v1/messages", env.gateway)))
        .json(&json!({"model": "claude-opus-4-8", "max_tokens": 10,
                       "messages": [{"role": "user", "content": "hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        env.openai.calls.load(Ordering::SeqCst),
        openai_calls_before + 1
    );

    // codex has `unknown: reject`, so unrecognized ids 404 instead of routing
    let resp = auth(
        env.client
            .post(format!("{}/openai/v1/responses", env.gateway)),
    )
    .json(&json!({"model": "nope", "input": "x"}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("nope"));
}

#[tokio::test]
async fn health_endpoints() {
    let env = setup().await;
    let v: Value = env
        .client
        .get(format!("{}/health", env.gateway))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["status"], "ok");
    let head = env
        .client
        .head(format!("{}/", env.gateway))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
}
