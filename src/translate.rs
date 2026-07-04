//! JSON body translations. Anthropic Messages is the pivot format:
//! Codex Responses requests become Anthropic requests, and OpenAI Chat
//! responses become Anthropic responses, so the four client/provider
//! combinations compose out of these four translators.

use serde_json::{json, Map, Value};

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Flatten Anthropic/Responses content (string or block array) to plain text.
pub fn flatten_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| {
                matches!(
                    b.get("type").and_then(Value::as_str),
                    None | Some("text") | Some("input_text") | Some("output_text")
                )
            })
            .map(|b| str_of(b, "text").to_string())
            .collect(),
        _ => String::new(),
    }
}

fn parse_args(args: &Value) -> Value {
    match args {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        Value::Object(_) => args.clone(),
        _ => json!({}),
    }
}

/// Anthropic image block -> OpenAI Chat `image_url` content part.
fn image_part_from_anthropic(b: &Value) -> Option<Value> {
    let src = b.get("source")?;
    let url = match src.get("type").and_then(Value::as_str) {
        Some("base64") => format!("data:{};base64,{}", str_of(src, "media_type"), str_of(src, "data")),
        Some("url") => str_of(src, "url").to_string(),
        _ => return None,
    };
    Some(json!({"type": "image_url", "image_url": {"url": url}}))
}

/// URL (possibly a data: URL) -> Anthropic image block.
fn anthropic_image_from_url(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((media_type, data)) = rest.split_once(";base64,") {
            return json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}});
        }
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

/// Extract OpenAI image parts from Anthropic content (top level or nested in
/// tool_result content).
fn extract_image_parts(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
                .filter_map(image_part_from_anthropic)
                .collect()
        })
        .unwrap_or_default()
}

fn copy_fields(from: &Value, to: &mut Map<String, Value>, fields: &[(&str, &str)]) {
    for (src, dst) in fields {
        if let Some(v) = from.get(*src) {
            if !v.is_null() {
                to.insert((*dst).to_string(), v.clone());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages request -> OpenAI Chat Completions request
// ---------------------------------------------------------------------------

pub fn anthropic_to_chat(body: &Value, upstream_model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(upstream_model));
    let mut messages: Vec<Value> = Vec::new();

    if let Some(sys) = body.get("system") {
        let text = flatten_text(sys);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    let empty = Vec::new();
    for msg in body.get("messages").and_then(Value::as_array).unwrap_or(&empty) {
        let role = str_of(msg, "role");
        match msg.get("content") {
            Some(Value::String(text)) => messages.push(json!({"role": role, "content": text})),
            Some(Value::Array(blocks)) => {
                let mut text = String::new();
                let mut thinking = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                let mut images: Vec<Value> = Vec::new();
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => text.push_str(str_of(b, "text")),
                        Some("thinking") => thinking.push_str(str_of(b, "thinking")),
                        Some("image") => images.extend(image_part_from_anthropic(b)),
                        Some("tool_use") => tool_calls.push(json!({
                            "id": str_of(b, "id"),
                            "type": "function",
                            "function": {
                                "name": str_of(b, "name"),
                                "arguments": b.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into())
                            }
                        })),
                        // Tool outputs must directly follow the assistant tool_calls
                        // message in Chat Completions, so emit them before any text.
                        Some("tool_result") => {
                            let content = b.get("content").unwrap_or(&Value::Null);
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": str_of(b, "tool_use_id"),
                                "content": flatten_text(content),
                            }));
                            // Chat Completions tool messages cannot carry images
                            // (Claude Code's Read tool returns screenshots this
                            // way); relay them in a trailing user message instead.
                            images.extend(extract_image_parts(content));
                        }
                        _ => {}
                    }
                }
                if role == "assistant" && (!tool_calls.is_empty() || !text.is_empty()) {
                    let mut m = Map::new();
                    m.insert("role".into(), json!("assistant"));
                    m.insert("content".into(), if text.is_empty() { Value::Null } else { json!(text) });
                    if !tool_calls.is_empty() {
                        m.insert("tool_calls".into(), json!(tool_calls));
                    }
                    // Reasoning models with preserved thinking (e.g. Kimi K2.7)
                    // require prior reasoning back verbatim in tool loops.
                    // `reasoning_content` is Moonshot's key, `reasoning` is the
                    // OpenRouter/vLLM spelling; providers pick the one they know.
                    if !thinking.is_empty() {
                        m.insert("reasoning_content".into(), json!(thinking));
                        m.insert("reasoning".into(), json!(thinking));
                    }
                    messages.push(Value::Object(m));
                } else if !images.is_empty() {
                    let mut parts: Vec<Value> = Vec::new();
                    if !text.is_empty() {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    parts.extend(images);
                    messages.push(json!({"role": "user", "content": parts}));
                } else if !text.is_empty() {
                    messages.push(json!({"role": role, "content": text}));
                }
            }
            _ => {}
        }
    }
    out.insert("messages".into(), json!(messages));

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter(|t| t.get("input_schema").is_some())
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": str_of(t, "name"),
                    "description": str_of(t, "description"),
                    "parameters": t["input_schema"],
                }})
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), json!(mapped));
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        let mapped = match tc.get("type").and_then(Value::as_str) {
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            Some("tool") => json!({"type": "function", "function": {"name": str_of(tc, "name")}}),
            _ => json!("auto"),
        };
        out.insert("tool_choice".into(), mapped);
    }
    copy_fields(
        body,
        &mut out,
        &[("max_tokens", "max_tokens"), ("temperature", "temperature"), ("top_p", "top_p"), ("stop_sequences", "stop")],
    );
    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        out.insert("stream".into(), json!(true));
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions response -> Anthropic Messages response
// ---------------------------------------------------------------------------

pub fn chat_to_anthropic(resp: &Value, alias: &str) -> Value {
    let choice = &resp["choices"][0];
    let msg = &choice["message"];
    let mut content: Vec<Value> = Vec::new();
    let reasoning = msg
        .get("reasoning_content")
        .or_else(|| msg.get("reasoning"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !reasoning.is_empty() {
        content.push(json!({"type": "thinking", "thinking": reasoning, "signature": ""}));
    }
    if let Some(t) = msg.get("content").and_then(Value::as_str) {
        if !t.is_empty() {
            content.push(json!({"type": "text", "text": t}));
        }
    }
    let empty = Vec::new();
    for tc in msg.get("tool_calls").and_then(Value::as_array).unwrap_or(&empty) {
        content.push(json!({
            "type": "tool_use",
            "id": if str_of(tc, "id").is_empty() { format!("toolu_{}", uuid::Uuid::new_v4().simple()) } else { str_of(tc, "id").into() },
            "name": str_of(&tc["function"], "name"),
            "input": parse_args(&tc["function"]["arguments"]),
        }));
    }
    let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        _ => "end_turn",
    };
    json!({
        "id": format!("msg_{}", if str_of(resp, "id").is_empty() { uuid::Uuid::new_v4().simple().to_string() } else { str_of(resp, "id").into() }),
        "type": "message",
        "role": "assistant",
        "model": alias,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            "output_tokens": resp["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        }
    })
}

// ---------------------------------------------------------------------------
// OpenAI Responses request -> Anthropic Messages request
// ---------------------------------------------------------------------------

pub fn responses_to_anthropic(body: &Value, upstream_model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(upstream_model));
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(instr) = body.get("instructions").and_then(Value::as_str) {
        if !instr.is_empty() {
            system_parts.push(instr.to_string());
        }
    }
    let mut messages: Vec<Value> = Vec::new();

    match body.get("input") {
        Some(Value::String(s)) => {
            messages.push(json!({"role": "user", "content": [{"type": "text", "text": s}]}))
        }
        Some(Value::Array(items)) => {
            for item in items {
                let ty = item.get("type").and_then(Value::as_str).unwrap_or("message");
                match ty {
                    "message" => {
                        let role = str_of(item, "role");
                        let content = item.get("content").unwrap_or(&Value::Null);
                        if role == "system" || role == "developer" {
                            let text = flatten_text(content);
                            if !text.is_empty() {
                                system_parts.push(text);
                            }
                            continue;
                        }
                        let text = flatten_text(content);
                        let mut blocks: Vec<Value> = Vec::new();
                        if !text.is_empty() {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                        if let Some(parts) = content.as_array() {
                            for p in parts {
                                if p.get("type").and_then(Value::as_str) == Some("input_image") {
                                    if let Some(url) = p.get("image_url").and_then(Value::as_str) {
                                        blocks.push(anthropic_image_from_url(url));
                                    }
                                }
                            }
                        }
                        if !blocks.is_empty() {
                            messages.push(json!({"role": role, "content": blocks}));
                        }
                    }
                    "function_call" => messages.push(json!({"role": "assistant", "content": [{
                        "type": "tool_use",
                        "id": str_of(item, "call_id"),
                        "name": str_of(item, "name"),
                        "input": parse_args(&item["arguments"]),
                    }]})),
                    "function_call_output" => messages.push(json!({"role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": str_of(item, "call_id"),
                        "content": flatten_text(item.get("output").unwrap_or(&Value::Null)),
                    }]})),
                    _ => {} // reasoning items and unknown types dropped
                }
            }
        }
        _ => {}
    }
    if !system_parts.is_empty() {
        out.insert("system".into(), json!(system_parts.join("\n\n")));
    }
    out.insert("messages".into(), json!(messages));

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("function"))
            .map(|t| {
                json!({
                    "name": str_of(t, "name"),
                    "description": str_of(t, "description"),
                    "input_schema": t.get("parameters").cloned().unwrap_or(json!({"type": "object"})),
                })
            })
            .collect();
        if !mapped.is_empty() {
            out.insert("tools".into(), json!(mapped));
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        let mapped = match (tc.as_str(), tc.get("type").and_then(Value::as_str)) {
            (Some("required"), _) => Some(json!({"type": "any"})),
            (Some("none"), _) => Some(json!({"type": "none"})),
            (Some("auto"), _) => Some(json!({"type": "auto"})),
            (_, Some("function")) => Some(json!({"type": "tool", "name": str_of(tc, "name")})),
            _ => None,
        };
        if let Some(m) = mapped {
            out.insert("tool_choice".into(), m);
        }
    }
    out.insert(
        "max_tokens".into(),
        body.get("max_output_tokens").cloned().filter(|v| !v.is_null()).unwrap_or(json!(8192)),
    );
    copy_fields(body, &mut out, &[("temperature", "temperature"), ("top_p", "top_p"), ("stream", "stream")]);
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Anthropic Messages response -> OpenAI Responses response
// ---------------------------------------------------------------------------

pub fn anthropic_to_responses(resp: &Value, alias: &str) -> Value {
    let mut output: Vec<Value> = Vec::new();
    let empty = Vec::new();
    for (i, b) in resp["content"].as_array().unwrap_or(&empty).iter().enumerate() {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => output.push(json!({
                "type": "message",
                "id": format!("msg_out_{i}"),
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": str_of(b, "text"), "annotations": []}],
            })),
            Some("tool_use") => output.push(json!({
                "type": "function_call",
                "id": format!("fc_{}", str_of(b, "id")),
                "call_id": str_of(b, "id"),
                "name": str_of(b, "name"),
                "arguments": b.get("input").map(|v| v.to_string()).unwrap_or_else(|| "{}".into()),
                "status": "completed",
            })),
            _ => {}
        }
    }
    let usage = &resp["usage"];
    let (inp, outp) = (usage["input_tokens"].as_u64().unwrap_or(0), usage["output_tokens"].as_u64().unwrap_or(0));
    json!({
        "id": format!("resp_{}", str_of(resp, "id")),
        "object": "response",
        "status": if resp.get("stop_reason").and_then(Value::as_str) == Some("max_tokens") { "incomplete" } else { "completed" },
        "model": alias,
        "output": output,
        "usage": {
            "input_tokens": inp,
            "output_tokens": outp,
            "total_tokens": inp + outp,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0},
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_request_to_chat_matches_spec_example() {
        let req = json!({
            "model": "claude-gpt-coder",
            "system": [{"type": "text", "text": "You are an agent..."}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Fix the tests"}]}],
            "tools": [{"name": "bash", "description": "Run shell commands",
                       "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}}],
            "stream": true,
            "max_tokens": 8192
        });
        let chat = anthropic_to_chat(&req, "gpt-5.1");
        assert_eq!(chat["model"], "gpt-5.1");
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["messages"][0], json!({"role": "system", "content": "You are an agent..."}));
        assert_eq!(chat["messages"][1], json!({"role": "user", "content": "Fix the tests"}));
        assert_eq!(chat["tools"][0]["function"]["name"], "bash");
        assert_eq!(chat["tools"][0]["function"]["parameters"]["properties"]["cmd"]["type"], "string");
    }

    #[test]
    fn anthropic_tool_loop_to_chat() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "run tests"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Running."},
                    {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"cmd": "npm test"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "ok"}]}
                ]}
            ]
        });
        let chat = anthropic_to_chat(&req, "gpt");
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["arguments"], "{\"cmd\":\"npm test\"}");
        assert_eq!(msgs[2], json!({"role": "tool", "tool_call_id": "toolu_1", "content": "ok"}));
    }

    #[test]
    fn images_translate_to_openai_image_url_parts() {
        // Pasted image in user content
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe this image"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}]
        });
        let chat = anthropic_to_chat(&req, "kimi");
        let content = chat["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0], json!({"type": "text", "text": "describe this image"}));
        assert_eq!(content[1], json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}));

        // Image returned by a tool (Claude Code's Read tool on a screenshot):
        // tool message keeps the text, image is relayed in a trailing user message.
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "s.png"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "read image"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "BBBB"}}
                    ]}
                ]}
            ]
        });
        let chat = anthropic_to_chat(&req, "kimi");
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["content"], "read image");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["image_url"]["url"], "data:image/png;base64,BBBB");
    }

    #[test]
    fn responses_input_image_becomes_anthropic_image_block() {
        let req = json!({
            "model": "m",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "what is this"},
                {"type": "input_image", "image_url": "data:image/jpeg;base64,CCCC"}
            ]}]
        });
        let a = responses_to_anthropic(&req, "claude");
        let blocks = a["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0], json!({"type": "text", "text": "what is this"}));
        assert_eq!(
            blocks[1],
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "CCCC"}})
        );
    }

    #[test]
    fn reasoning_content_round_trips_for_preserved_thinking_models() {
        // Upstream (Kimi-style) response with reasoning_content becomes a
        // thinking block for the client...
        let resp = json!({
            "id": "c1",
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": null,
                "reasoning_content": "I should run the tests first.",
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "bash", "arguments": "{\"cmd\":\"npm test\"}"}}]
            }}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let a = chat_to_anthropic(&resp, "claude-kimi-coder");
        assert_eq!(a["content"][0]["type"], "thinking");
        assert_eq!(a["content"][0]["thinking"], "I should run the tests first.");
        assert_eq!(a["content"][1]["type"], "tool_use");

        // ...and when the client sends that history back, the thinking block
        // is restored as reasoning_content on the assistant message (Kimi
        // errors in tool loops if it is stripped).
        let history = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "run tests"},
                {"role": "assistant", "content": a["content"]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "ok"}]}
            ]
        });
        let chat = anthropic_to_chat(&history, "moonshotai/kimi-k2.7-code");
        let assistant = &chat["messages"][1];
        assert_eq!(assistant["reasoning_content"], "I should run the tests first.");
        assert_eq!(assistant["reasoning"], "I should run the tests first.");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn chat_response_to_anthropic_tool_use() {
        let resp = json!({
            "id": "chatcmpl-1",
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": null,
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}}]
            }}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let a = chat_to_anthropic(&resp, "claude-gpt-coder");
        assert_eq!(a["stop_reason"], "tool_use");
        assert_eq!(a["content"][0], json!({"type": "tool_use", "id": "call_1", "name": "bash", "input": {"cmd": "ls"}}));
        assert_eq!(a["usage"], json!({"input_tokens": 10, "output_tokens": 5}));
    }

    #[test]
    fn responses_request_to_anthropic_matches_spec_example() {
        let req = json!({
            "model": "claude-sonnet",
            "input": [{"role": "user", "content": "Fix the failing test"}],
            "tools": [{"type": "function", "name": "bash", "description": "Run shell commands",
                       "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}],
            "stream": true
        });
        let a = responses_to_anthropic(&req, "claude-sonnet-4-6");
        assert_eq!(a["model"], "claude-sonnet-4-6");
        assert_eq!(a["max_tokens"], 8192);
        assert_eq!(a["stream"], true);
        assert_eq!(a["messages"][0], json!({"role": "user", "content": [{"type": "text", "text": "Fix the failing test"}]}));
        assert_eq!(a["tools"][0]["input_schema"]["properties"]["cmd"]["type"], "string");
    }

    #[test]
    fn responses_function_call_loop_to_anthropic() {
        let req = json!({
            "model": "m",
            "instructions": "You are Codex.",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "run tests"}]},
                {"type": "function_call", "call_id": "toolu_123", "name": "bash", "arguments": "{\"cmd\":\"npm test\"}"},
                {"type": "function_call_output", "call_id": "toolu_123", "output": "all passed"}
            ]
        });
        let a = responses_to_anthropic(&req, "claude");
        assert_eq!(a["system"], "You are Codex.");
        assert_eq!(a["messages"][1]["content"][0], json!({"type": "tool_use", "id": "toolu_123", "name": "bash", "input": {"cmd": "npm test"}}));
        assert_eq!(a["messages"][2]["content"][0], json!({"type": "tool_result", "tool_use_id": "toolu_123", "content": "all passed"}));
    }

    #[test]
    fn anthropic_response_to_responses_function_call() {
        let resp = json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [
                {"type": "text", "text": "Running tests."},
                {"type": "tool_use", "id": "toolu_123", "name": "bash", "input": {"cmd": "npm test"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "output_tokens": 8}
        });
        let r = anthropic_to_responses(&resp, "claude-sonnet");
        assert_eq!(r["status"], "completed");
        assert_eq!(r["output"][0]["content"][0]["text"], "Running tests.");
        let fc = &r["output"][1];
        assert_eq!(fc["type"], "function_call");
        assert_eq!(fc["call_id"], "toolu_123");
        assert_eq!(fc["name"], "bash");
        assert_eq!(fc["arguments"], "{\"cmd\":\"npm test\"}");
        assert_eq!(r["usage"]["total_tokens"], 28);
    }
}
