use crate::config::{CompatLevel, Compatibility, RouteConfig, ToolCap};
use crate::error::ClientProtocol;
use serde_json::Value;

/// Feature requirements detected on an incoming request, protocol-agnostic
/// where the routing decision is concerned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Requirements {
    pub has_tools: bool,
    pub has_tool_results: bool,
    pub has_images: bool,
    pub has_thinking: bool,
    pub has_cache_control: bool,
    pub requires_streaming: bool,
}

fn any_content_block(body: &Value, pred: &dyn Fn(&Value) -> bool) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|msgs| {
            msgs.iter().any(|m| {
                m.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| blocks.iter().any(pred))
            })
        })
}

fn contains_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| contains_key(v, key)),
        Value::Array(arr) => arr.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}

/// Deep search for `"type": <ty>` objects (images can be nested inside
/// tool_result content, not just top-level message blocks).
fn contains_block_type(v: &Value, ty: &str) -> bool {
    match v {
        Value::Object(map) => {
            map.get("type").and_then(Value::as_str) == Some(ty)
                || map.values().any(|v| contains_block_type(v, ty))
        }
        Value::Array(arr) => arr.iter().any(|v| contains_block_type(v, ty)),
        _ => false,
    }
}

pub fn classify_anthropic(body: &Value) -> Requirements {
    let block_type = |t: &'static str| move |b: &Value| b.get("type").and_then(Value::as_str) == Some(t);
    Requirements {
        has_tools: body.get("tools").and_then(Value::as_array).is_some_and(|t| !t.is_empty()),
        has_tool_results: any_content_block(body, &block_type("tool_result")),
        has_images: contains_block_type(body.get("messages").unwrap_or(&Value::Null), "image"),
        has_thinking: body.get("thinking").is_some_and(|t| !t.is_null()),
        has_cache_control: contains_key(body, "cache_control"),
        requires_streaming: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
    }
}

pub fn classify_responses(body: &Value) -> Requirements {
    let input_items = body.get("input").and_then(Value::as_array);
    let item_type = |t: &str| {
        input_items.is_some_and(|items| items.iter().any(|i| i.get("type").and_then(Value::as_str) == Some(t)))
    };
    let has_images = contains_block_type(body.get("input").unwrap_or(&Value::Null), "input_image");
    Requirements {
        has_tools: body.get("tools").and_then(Value::as_array).is_some_and(|t| !t.is_empty()),
        has_tool_results: item_type("function_call_output"),
        has_images,
        has_thinking: body.get("reasoning").is_some_and(|r| !r.is_null()),
        has_cache_control: false,
        requires_streaming: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
    }
}

pub fn compat_for(compat: &Compatibility, client: ClientProtocol) -> CompatLevel {
    match client {
        ClientProtocol::AnthropicMessages => compat.claude_code,
        ClientProtocol::OpenAiResponses => compat.codex,
    }
}

/// A route is eligible only if the client compat level is not blocked and the
/// route capabilities cover what the request actually uses.
pub fn route_eligible(route: &RouteConfig, level: CompatLevel, req: &Requirements) -> Result<(), String> {
    if level == CompatLevel::Blocked {
        return Err("route blocked for this client".into());
    }
    if (req.has_tools || req.has_tool_results) && route.capabilities.tools == ToolCap::None {
        return Err("request uses tools but route has no tool support".into());
    }
    if (req.has_tools || req.has_tool_results) && level == CompatLevel::TextOnly {
        return Err("request uses tools but route is text_only for this client".into());
    }
    if req.has_images && !route.capabilities.images {
        return Err("request contains images but route does not support image input".into());
    }
    if req.has_thinking
        && route.capabilities.thinking == crate::config::FeatureCap::None
        && level == CompatLevel::Full
    {
        // Full compat promises native features; anything else drops thinking silently.
        return Err("request uses thinking but route does not support it natively".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_anthropic_tool_request() {
        let body = json!({
            "model": "claude-sonnet",
            "stream": true,
            "tools": [{"name": "bash", "input_schema": {}}],
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}
            ],
            "system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}]
        });
        let r = classify_anthropic(&body);
        assert!(r.has_tools && r.has_tool_results && r.requires_streaming && r.has_cache_control);
        assert!(!r.has_images && !r.has_thinking);
    }

    #[test]
    fn classifies_responses_request() {
        let body = json!({
            "model": "claude-sonnet",
            "stream": true,
            "tools": [{"type": "function", "name": "bash", "parameters": {}}],
            "input": [
                {"type": "function_call_output", "call_id": "c1", "output": "done"}
            ]
        });
        let r = classify_responses(&body);
        assert!(r.has_tools && r.has_tool_results && r.requires_streaming);
    }

    #[test]
    fn detects_images_nested_in_tool_results_and_gates_routes() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "x"}}
                ]}
            ]}]
        });
        let r = classify_anthropic(&body);
        assert!(r.has_images);

        let no_images: RouteConfig = serde_yaml::from_str(
            "{ id: r, provider: openai_compatible, model: m, base_url: 'http://x', api_key: k, capabilities: { tools: openai } }",
        )
        .unwrap();
        let with_images: RouteConfig = serde_yaml::from_str(
            "{ id: r2, provider: openai_compatible, model: m, base_url: 'http://x', api_key: k, capabilities: { tools: openai, images: true } }",
        )
        .unwrap();
        let req = Requirements { has_images: true, ..Default::default() };
        assert!(route_eligible(&no_images, CompatLevel::Tools, &req).is_err());
        assert!(route_eligible(&with_images, CompatLevel::Tools, &req).is_ok());
    }

    #[test]
    fn tool_request_needs_tool_capable_route() {
        let route: RouteConfig = serde_yaml::from_str(
            "{ id: r, provider: openai_compatible, model: m, base_url: 'http://x', api_key: k }",
        )
        .unwrap();
        let req = Requirements { has_tools: true, ..Default::default() };
        assert!(route_eligible(&route, CompatLevel::Tools, &req).is_err());
        let req = Requirements::default();
        assert!(route_eligible(&route, CompatLevel::Tools, &req).is_ok());
        assert!(route_eligible(&route, CompatLevel::Blocked, &req).is_err());
    }
}
