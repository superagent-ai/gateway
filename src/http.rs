use crate::classify::{self, Requirements};
use crate::config::{CompatLevel, Config, ProviderKind, RouteConfig};
use crate::error::{ClientProtocol, GatewayError};
use crate::stream::{
    AnthropicParser, AnthropicRenderer, ChatParser, EventParser, EventRenderer, ResponsesRenderer,
    SseParser, StreamPhase,
};
use crate::translate;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
    /// route id -> (owning model alias, route)
    pub routes: std::collections::BTreeMap<String, (String, RouteConfig)>,
}

pub fn build_router(cfg: Config) -> Router {
    let routes = cfg.route_index();
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("http client");
    let state = Arc::new(AppState { cfg, http, routes });
    Router::new()
        .route("/health", get(health))
        .route("/", head(|| async { StatusCode::OK }))
        .route("/v1/messages", post(messages))
        .route("/anthropic/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/anthropic/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(responses))
        .route("/openai/v1/responses", post(responses))
        .route("/v1/models", get(models_any))
        .route("/anthropic/v1/models", get(models_anthropic))
        .route("/openai/v1/models", get(models_openai))
        .layer(axum::middleware::from_fn(log_requests))
        .with_state(state)
}

async fn log_requests(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let resp = next.run(req).await;
    tracing::info!(%method, path, status = resp.status().as_u16(), "http");
    resp
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}))
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

fn check_auth(cfg: &Config, headers: &HeaderMap) -> Result<(), GatewayError> {
    if cfg.auth.tokens.is_empty() {
        return Ok(());
    }
    let matches = |t: &str| cfg.auth.tokens.iter().any(|x| x == t);
    // Claude Code may present the gateway token as either a Bearer token or
    // an x-api-key depending on which env var configured it.
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| matches(t.trim()));
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(matches);
    if bearer || api_key {
        Ok(())
    } else {
        Err(GatewayError::Unauthorized)
    }
}

// ---------------------------------------------------------------------------
// Model listing
// ---------------------------------------------------------------------------

fn models_payload(cfg: &Config, client: Option<ClientProtocol>) -> Value {
    let visible = |m: &crate::config::ModelConfig| match client {
        Some(c) => classify::compat_for(&m.compatibility, c) != CompatLevel::Blocked,
        None => {
            m.compatibility.claude_code != CompatLevel::Blocked
                || m.compatibility.codex != CompatLevel::Blocked
        }
    };
    let data: Vec<Value> = cfg
        .models
        .iter()
        .filter(|(_, m)| !m.hidden && visible(m))
        .map(|(alias, m)| {
            // Hybrid shape parseable as both an Anthropic and an OpenAI model object.
            json!({
                "type": "model",
                "object": "model",
                "id": alias,
                "display_name": m.display_name.clone().unwrap_or_else(|| alias.clone()),
                "created": 0,
                "owned_by": "gateway",
            })
        })
        .collect();
    json!({"object": "list", "data": data, "has_more": false})
}

macro_rules! models_handler {
    ($name:ident, $client:expr) => {
        async fn $name(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
            if let Err(e) = check_auth(&st.cfg, &headers) {
                return e.into_response_for($client.unwrap_or(ClientProtocol::AnthropicMessages));
            }
            Json(models_payload(&st.cfg, $client)).into_response()
        }
    };
}
models_handler!(models_any, None::<ClientProtocol>);
models_handler!(models_anthropic, Some(ClientProtocol::AnthropicMessages));
models_handler!(models_openai, Some(ClientProtocol::OpenAiResponses));

// ---------------------------------------------------------------------------
// Route selection
// ---------------------------------------------------------------------------

/// Ordered eligible routes for an alias. Routes borrowed from other models via
/// per-model fallback lists count as degraded candidates when their owning
/// model's compat level is degraded, and are gated by allow_degraded_fallback.
fn candidates(
    st: &AppState,
    alias: &str,
    client: ClientProtocol,
    req: &Requirements,
) -> Result<Vec<(String, RouteConfig)>, GatewayError> {
    let model = st
        .cfg
        .models
        .get(alias)
        .ok_or_else(|| GatewayError::UnknownModel(alias.into()))?;
    let level = classify::compat_for(&model.compatibility, client);
    if level == CompatLevel::Blocked {
        return Err(GatewayError::capability(format!(
            "model '{alias}' is blocked for this client"
        )));
    }
    let allow_degraded = model
        .fallback
        .as_ref()
        .and_then(|f| f.allow_degraded_fallback)
        .unwrap_or(st.cfg.fallback.allow_degraded_fallback);

    let ordered: Vec<(String, RouteConfig)> =
        match model.fallback.as_ref().and_then(|f| f.routes.clone()) {
            Some(ids) => ids
                .iter()
                .filter_map(|id| st.routes.get(id).cloned())
                .collect(),
            None => model
                .routes
                .iter()
                .map(|r| (alias.to_string(), r.clone()))
                .collect(),
        };

    let mut reasons = Vec::new();
    let eligible: Vec<(String, RouteConfig)> = ordered
        .into_iter()
        .filter(|(owner, route)| {
            let owner_level = classify::compat_for(&st.cfg.models[owner].compatibility, client);
            // Degraded routes from *other* models need explicit opt-in;
            // the requested model's own routes were asked for directly.
            if owner != alias && owner_level.is_degraded() && !allow_degraded {
                reasons.push(format!("{}: degraded fallback not allowed", route.id));
                return false;
            }
            match classify::route_eligible(route, owner_level, req) {
                Ok(()) => true,
                Err(why) => {
                    reasons.push(format!("{}: {}", route.id, why));
                    false
                }
            }
        })
        .collect();

    if eligible.is_empty() {
        return Err(GatewayError::capability(format!(
            "No route for {alias} supports this request ({})",
            reasons.join("; ")
        )));
    }
    Ok(eligible)
}

// ---------------------------------------------------------------------------
// Upstream calls
// ---------------------------------------------------------------------------

fn build_upstream_body(client: ClientProtocol, route: &RouteConfig, body: &Value) -> Value {
    let mut out = translate_request_body(client, route, body);
    if let Some(map) = out.as_object_mut() {
        for p in &route.drop_params {
            map.remove(p);
        }
    }
    out
}

fn translate_request_body(client: ClientProtocol, route: &RouteConfig, body: &Value) -> Value {
    match (client, route.provider) {
        (ClientProtocol::AnthropicMessages, ProviderKind::Anthropic) => {
            let mut b = body.clone();
            b["model"] = json!(route.model);
            b
        }
        (ClientProtocol::AnthropicMessages, ProviderKind::OpenaiCompatible) => {
            translate::anthropic_to_chat(body, &route.model)
        }
        (ClientProtocol::OpenAiResponses, ProviderKind::Anthropic) => {
            translate::responses_to_anthropic(body, &route.model)
        }
        // Pivot through Anthropic Messages: Responses -> Messages -> Chat.
        (ClientProtocol::OpenAiResponses, ProviderKind::OpenaiCompatible) => {
            translate::anthropic_to_chat(
                &translate::responses_to_anthropic(body, &route.model),
                &route.model,
            )
        }
    }
}

fn upstream_request(
    st: &AppState,
    route: &RouteConfig,
    body: &Value,
    client_headers: &HeaderMap,
    stream: bool,
) -> reqwest::RequestBuilder {
    let key = route.resolve_api_key().unwrap_or_default();
    let mut rb = match route.provider {
        ProviderKind::Anthropic => {
            let mut rb = st
                .http
                .post(format!("{}/v1/messages", route.base()))
                .header("x-api-key", key);
            // Forward anthropic-* headers (anthropic-version, anthropic-beta, ...)
            // unchanged; default the version if the client did not send one.
            let mut has_version = false;
            for (name, value) in client_headers {
                if name.as_str().starts_with("anthropic-") {
                    has_version |= name.as_str() == "anthropic-version";
                    rb = rb.header(name, value);
                }
            }
            if !has_version {
                rb = rb.header("anthropic-version", "2023-06-01");
            }
            rb
        }
        ProviderKind::OpenaiCompatible => st
            .http
            .post(format!("{}/chat/completions", route.base()))
            .bearer_auth(key),
    };
    if !stream {
        rb = rb.timeout(Duration::from_millis(route.timeout_ms));
    }
    rb.json(body)
}

fn translate_response_body(
    client: ClientProtocol,
    provider: ProviderKind,
    alias: &str,
    upstream: &Value,
) -> Value {
    match (client, provider) {
        (ClientProtocol::AnthropicMessages, ProviderKind::Anthropic) => upstream.clone(),
        (ClientProtocol::AnthropicMessages, ProviderKind::OpenaiCompatible) => {
            translate::chat_to_anthropic(upstream, alias)
        }
        (ClientProtocol::OpenAiResponses, ProviderKind::Anthropic) => {
            translate::anthropic_to_responses(upstream, alias)
        }
        (ClientProtocol::OpenAiResponses, ProviderKind::OpenaiCompatible) => {
            translate::anthropic_to_responses(&translate::chat_to_anthropic(upstream, alias), alias)
        }
    }
}

fn sse_response(
    body_stream: impl futures_util::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>>
        + Send
        + 'static,
) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(body_stream),
    )
        .into_response()
}

/// Relay an upstream SSE body through a parser/renderer pair. Fallback is
/// impossible past this point by construction: this only runs after a 2xx,
/// and errors mid-stream terminate the client stream rather than retrying.
fn translated_stream(
    upstream: reqwest::Response,
    mut parser: Box<dyn EventParser>,
    mut renderer: Box<dyn EventRenderer>,
    request_id: String,
) -> Response {
    let mut bytes_stream = upstream.bytes_stream();
    let s = async_stream::stream! {
        let mut sse = SseParser::default();
        let mut phase = StreamPhase::NotStarted;
        while let Some(chunk) = bytes_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    for e in sse.push(&bytes) {
                        for ev in parser.on_sse(&e) {
                            phase.advance(&ev);
                            if let Some(frame) = renderer.render(&ev) {
                                yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from(frame));
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(request_id, phase = ?phase, error = %err, "upstream stream failed after start; surfacing to client");
                    let frame = crate::stream::frame("error", &json!({
                        "type": "error",
                        "error": {"type": "gateway_upstream_error", "message": format!("upstream stream failed: {err}")}
                    }));
                    yield Ok(bytes::Bytes::from(frame));
                    return;
                }
            }
        }
        if phase != StreamPhase::Done {
            tracing::warn!(request_id, phase = ?phase, "upstream stream ended before completion");
        }
    };
    sse_response(s)
}

/// Byte-for-byte relay for Claude Code -> Anthropic upstream. A parser rides
/// along only to track the stream phase for logging.
fn passthrough_stream(upstream: reqwest::Response, request_id: String) -> Response {
    let mut bytes_stream = upstream.bytes_stream();
    let s = async_stream::stream! {
        let mut sse = SseParser::default();
        let mut parser = AnthropicParser;
        let mut phase = StreamPhase::NotStarted;
        while let Some(chunk) = bytes_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    for e in sse.push(&bytes) {
                        for ev in parser.on_sse(&e) {
                            phase.advance(&ev);
                        }
                    }
                    yield Ok::<_, std::convert::Infallible>(bytes);
                }
                Err(err) => {
                    tracing::warn!(request_id, phase = ?phase, error = %err, "anthropic passthrough stream failed");
                    return;
                }
            }
        }
    };
    sse_response(s)
}

/// Map the requested model to a configured alias. Unknown ids (e.g. concrete
/// claude-* models pinned by Claude Code subagents) fall back to
/// `default_model` when configured.
fn resolve_alias(cfg: &Config, body: &Value) -> Result<String, GatewayError> {
    let requested = body["model"]
        .as_str()
        .ok_or_else(|| GatewayError::BadRequest("missing 'model'".into()))?;
    if cfg.models.contains_key(requested) {
        return Ok(requested.to_string());
    }
    for e in &cfg.model_map {
        if crate::config::glob_match(&e.pattern, requested) {
            tracing::info!(
                requested,
                pattern = e.pattern,
                model = e.model,
                "model_map routed request"
            );
            return Ok(e.model.clone());
        }
    }
    if let Some(d) = &cfg.default_model {
        tracing::info!(
            requested,
            default = d,
            "unknown model id routed to default_model"
        );
        return Ok(d.clone());
    }
    Err(GatewayError::UnknownModel(requested.into()))
}

// ---------------------------------------------------------------------------
// Core execute: classify -> select routes -> attempt loop -> respond
// ---------------------------------------------------------------------------

async fn execute(
    st: &AppState,
    client: ClientProtocol,
    body: Value,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let alias = resolve_alias(&st.cfg, &body)?;
    let req = match client {
        ClientProtocol::AnthropicMessages => classify::classify_anthropic(&body),
        ClientProtocol::OpenAiResponses => classify::classify_responses(&body),
    };
    if client == ClientProtocol::OpenAiResponses
        && body
            .get("previous_response_id")
            .is_some_and(|v| !v.is_null())
    {
        return Err(GatewayError::capability(
            "previous_response_id is not supported; send full conversation input",
        ));
    }
    let routes = candidates(st, &alias, client, &req)?;

    // Attempt plan per spec: retry the first route once, then walk fallbacks,
    // capped at fallback.max_attempts total attempts.
    let mut plan: Vec<&(String, RouteConfig)> = vec![&routes[0]];
    plan.extend(routes.iter());
    plan.truncate(st.cfg.fallback.max_attempts.max(1) as usize);

    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    let client_name = match client {
        ClientProtocol::AnthropicMessages => "claude_code",
        ClientProtocol::OpenAiResponses => "codex",
    };
    let session_id = headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if st.cfg.telemetry.log_prompts {
        tracing::info!(request_id, client = client_name, body = %body, "request trace");
    }

    let total = plan.len();
    let mut last_err: Option<GatewayError> = None;
    for (attempt, (_owner, route)) in plan.into_iter().enumerate() {
        if attempt > 0 {
            let jitter = 100
                + (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as u64
                    % 250);
            tokio::time::sleep(Duration::from_millis(jitter)).await;
        }
        let upstream_body = build_upstream_body(client, route, &body);
        let started = Instant::now();
        let result = upstream_request(st, route, &upstream_body, &headers, req.requires_streaming)
            .send()
            .await;

        let log = |status: i64, ok: bool| {
            tracing::info!(
                request_id,
                client = client_name,
                model_alias = alias,
                route_id = route.id,
                attempt = attempt + 1,
                stream = req.requires_streaming,
                status,
                fallback_used = attempt > 0,
                duration_ms = started.elapsed().as_millis() as u64,
                session_id,
                ok,
                "attempt"
            );
        };

        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                log(0, false);
                last_err = Some(e.into());
                continue; // connection failure before headers: always fallback-safe
            }
        };
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let retryable = st.cfg.fallback.retryable_statuses.contains(&status);
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let bytes = resp.bytes().await.unwrap_or_default();
            log(status as i64, false);
            tracing::warn!(
                request_id,
                route_id = route.id,
                status,
                body = %String::from_utf8_lossy(&bytes[..bytes.len().min(2000)]),
                "upstream error"
            );
            let err = GatewayError::Upstream {
                status,
                body: bytes,
                content_type,
            };
            if retryable && attempt + 1 < total {
                last_err = Some(err);
                continue;
            }
            return Err(err); // forwarded verbatim (status + body untouched)
        }
        log(status as i64, true);

        if req.requires_streaming {
            return Ok(match (client, route.provider) {
                (ClientProtocol::AnthropicMessages, ProviderKind::Anthropic) => {
                    passthrough_stream(resp, request_id)
                }
                (ClientProtocol::AnthropicMessages, ProviderKind::OpenaiCompatible) => {
                    translated_stream(
                        resp,
                        Box::new(ChatParser::default()),
                        Box::new(AnthropicRenderer::new(alias.clone())),
                        request_id,
                    )
                }
                (ClientProtocol::OpenAiResponses, ProviderKind::Anthropic) => translated_stream(
                    resp,
                    Box::new(AnthropicParser),
                    Box::new(ResponsesRenderer::new(alias.clone())),
                    request_id,
                ),
                (ClientProtocol::OpenAiResponses, ProviderKind::OpenaiCompatible) => {
                    translated_stream(
                        resp,
                        Box::new(ChatParser::default()),
                        Box::new(ResponsesRenderer::new(alias.clone())),
                        request_id,
                    )
                }
            });
        }
        let upstream_json: Value = resp.json().await?;
        let translated = translate_response_body(client, route.provider, &alias, &upstream_json);
        return Ok(Json(translated).into_response());
    }
    Err(last_err.unwrap_or_else(|| GatewayError::AllRoutesFailed {
        alias: alias.clone(),
        detail: "no attempts were possible".into(),
    }))
}

// ---------------------------------------------------------------------------
// Endpoint handlers
// ---------------------------------------------------------------------------

async fn messages(State(st): State<Arc<AppState>>, headers: HeaderMap, body: String) -> Response {
    let client = ClientProtocol::AnthropicMessages;
    match handle(&st, client, headers, body).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(client = "claude_code", error = %e, "request rejected");
            e.into_response_for(client)
        }
    }
}

async fn responses(State(st): State<Arc<AppState>>, headers: HeaderMap, body: String) -> Response {
    let client = ClientProtocol::OpenAiResponses;
    match handle(&st, client, headers, body).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(client = "codex", error = %e, "request rejected");
            e.into_response_for(client)
        }
    }
}

async fn handle(
    st: &AppState,
    client: ClientProtocol,
    headers: HeaderMap,
    body: String,
) -> Result<Response, GatewayError> {
    check_auth(&st.cfg, &headers)?;
    let body: Value = serde_json::from_str(&body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid JSON: {e}")))?;
    execute(st, client, body, headers).await
}

async fn count_tokens(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let client = ClientProtocol::AnthropicMessages;
    let result: Result<Response, GatewayError> = async {
        check_auth(&st.cfg, &headers)?;
        let body: Value = serde_json::from_str(&body)
            .map_err(|e| GatewayError::BadRequest(format!("invalid JSON: {e}")))?;
        let alias = resolve_alias(&st.cfg, &body)?;
        let req = classify::classify_anthropic(&body);
        let routes = candidates(&st, &alias, client, &req)?;
        let (_, route) = &routes[0];
        match route.provider {
            ProviderKind::Anthropic => {
                let mut upstream_body = body.clone();
                upstream_body["model"] = json!(route.model);
                let resp = st
                    .http
                    .post(format!("{}/v1/messages/count_tokens", route.base()))
                    .header("x-api-key", route.resolve_api_key().unwrap_or_default())
                    .header("anthropic-version", "2023-06-01")
                    .timeout(Duration::from_millis(route.timeout_ms))
                    .json(&upstream_body)
                    .send()
                    .await?;
                let status = resp.status().as_u16();
                let content_type = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                let bytes = resp.bytes().await?;
                if !(200..300).contains(&status) {
                    return Err(GatewayError::Upstream {
                        status,
                        body: bytes,
                        content_type,
                    });
                }
                Ok((
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response())
            }
            ProviderKind::OpenaiCompatible => {
                // Rough estimate: ~4 chars per token over the serialized prompt.
                let text = format!(
                    "{}{}{}",
                    body["system"],
                    body["messages"],
                    body.get("tools").unwrap_or(&Value::Null)
                );
                Ok(
                    Json(json!({"input_tokens": (text.chars().count() / 4).max(1)}))
                        .into_response(),
                )
            }
        }
    }
    .await;
    result.unwrap_or_else(|e| e.into_response_for(client))
}
