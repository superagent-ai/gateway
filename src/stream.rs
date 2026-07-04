//! Streaming translation. Upstream SSE (Anthropic or OpenAI Chat) is parsed
//! into a small internal event vocabulary, then rendered back out as either
//! Anthropic Messages SSE or OpenAI Responses SSE. Events are relayed as they
//! arrive; nothing is buffered except what the Responses protocol itself
//! requires for its final `response.completed` payload.

use serde_json::{json, Value};
use std::collections::HashMap;

/// Internal, protocol-neutral stream events (modeled on Anthropic's shape).
#[derive(Debug, Clone, PartialEq)]
pub enum Ev {
    Start {
        id: String,
        input_tokens: u64,
    },
    ThinkingStart {
        index: usize,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ArgsDelta {
        index: usize,
        json: String,
    },
    BlockStop {
        index: usize,
    },
    Finish {
        stop_reason: String,
        output_tokens: u64,
    },
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPhase {
    NotStarted,
    MessageStarted,
    TextStarted,
    ToolUseStarted,
    ToolUseCompleted,
    Done,
}

impl StreamPhase {
    pub fn advance(&mut self, ev: &Ev) {
        *self = match ev {
            Ev::Start { .. } => Self::MessageStarted,
            Ev::TextStart { .. }
            | Ev::TextDelta { .. }
            | Ev::ThinkingStart { .. }
            | Ev::ThinkingDelta { .. } => Self::TextStarted,
            Ev::ToolStart { .. } | Ev::ArgsDelta { .. } => Self::ToolUseStarted,
            Ev::BlockStop { .. } if *self == Self::ToolUseStarted => Self::ToolUseCompleted,
            Ev::Done => Self::Done,
            _ => *self,
        };
    }
}

// ---------------------------------------------------------------------------
// SSE wire parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf
            .push_str(&String::from_utf8_lossy(chunk).replace("\r\n", "\n"));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..pos + 2).collect();
            let (mut event, mut data) = (None, Vec::new());
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
                }
            }
            if !data.is_empty() {
                out.push(SseEvent {
                    event,
                    data: data.join("\n"),
                });
            }
        }
        out
    }
}

pub trait EventParser: Send {
    fn on_sse(&mut self, e: &SseEvent) -> Vec<Ev>;
}

pub trait EventRenderer: Send {
    fn render(&mut self, ev: &Ev) -> Option<String>;
}

pub fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------
// Anthropic SSE -> Ev
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct AnthropicParser;

impl EventParser for AnthropicParser {
    fn on_sse(&mut self, e: &SseEvent) -> Vec<Ev> {
        let Ok(v) = serde_json::from_str::<Value>(&e.data) else {
            return vec![];
        };
        let index = v["index"].as_u64().unwrap_or(0) as usize;
        match v["type"].as_str().unwrap_or("") {
            "message_start" => vec![Ev::Start {
                id: s(&v["message"], "id"),
                input_tokens: v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0),
            }],
            "content_block_start" => match v["content_block"]["type"].as_str() {
                Some("tool_use") => vec![Ev::ToolStart {
                    index,
                    id: s(&v["content_block"], "id"),
                    name: s(&v["content_block"], "name"),
                }],
                Some("text") => vec![Ev::TextStart { index }],
                Some("thinking") => vec![Ev::ThinkingStart { index }],
                _ => vec![],
            },
            "content_block_delta" => match v["delta"]["type"].as_str() {
                Some("text_delta") => vec![Ev::TextDelta {
                    index,
                    text: s(&v["delta"], "text"),
                }],
                Some("input_json_delta") => {
                    vec![Ev::ArgsDelta {
                        index,
                        json: s(&v["delta"], "partial_json"),
                    }]
                }
                Some("thinking_delta") => {
                    vec![Ev::ThinkingDelta {
                        index,
                        text: s(&v["delta"], "thinking"),
                    }]
                }
                _ => vec![],
            },
            "content_block_stop" => vec![Ev::BlockStop { index }],
            "message_delta" => vec![Ev::Finish {
                stop_reason: s(&v["delta"], "stop_reason"),
                output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            }],
            "message_stop" => vec![Ev::Done],
            _ => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions SSE -> Ev
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Thinking,
    Text,
    Tool,
}

#[derive(Default)]
pub struct ChatParser {
    started: bool,
    open: Option<(usize, BlockKind)>,
    next_index: usize,
    tool_index_map: HashMap<u64, usize>,
    stop_reason: Option<String>,
    output_tokens: u64,
}

impl ChatParser {
    fn close_open(&mut self, out: &mut Vec<Ev>) {
        if let Some((idx, _)) = self.open.take() {
            out.push(Ev::BlockStop { index: idx });
        }
    }

    fn open_block(&mut self, kind: BlockKind, out: &mut Vec<Ev>) -> usize {
        self.close_open(out);
        let idx = self.next_index;
        self.next_index += 1;
        self.open = Some((idx, kind));
        idx
    }
}

impl EventParser for ChatParser {
    fn on_sse(&mut self, e: &SseEvent) -> Vec<Ev> {
        let mut out = Vec::new();
        if e.data.trim() == "[DONE]" {
            self.close_open(&mut out);
            out.push(Ev::Finish {
                stop_reason: self.stop_reason.take().unwrap_or_else(|| "end_turn".into()),
                output_tokens: self.output_tokens,
            });
            out.push(Ev::Done);
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(&e.data) else {
            return out;
        };
        if !self.started {
            self.started = true;
            out.push(Ev::Start {
                id: s(&v, "id"),
                input_tokens: 0,
            });
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.output_tokens = u["completion_tokens"]
                .as_u64()
                .unwrap_or(self.output_tokens);
        }
        let choice = &v["choices"][0];
        if let Some(fr) = choice["finish_reason"].as_str() {
            self.stop_reason = Some(
                match fr {
                    "length" => "max_tokens",
                    "tool_calls" => "tool_use",
                    _ => "end_turn",
                }
                .into(),
            );
        }
        let delta = &choice["delta"];
        // Reasoning deltas: `reasoning_content` (Moonshot) or `reasoning` (OpenRouter/vLLM).
        let reasoning = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
            .filter(|t| !t.is_empty());
        if let Some(text) = reasoning {
            match self.open {
                Some((idx, BlockKind::Thinking)) => out.push(Ev::ThinkingDelta {
                    index: idx,
                    text: text.into(),
                }),
                _ => {
                    let idx = self.open_block(BlockKind::Thinking, &mut out);
                    out.push(Ev::ThinkingStart { index: idx });
                    out.push(Ev::ThinkingDelta {
                        index: idx,
                        text: text.into(),
                    });
                }
            }
        }
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            match self.open {
                Some((idx, BlockKind::Text)) => out.push(Ev::TextDelta {
                    index: idx,
                    text: text.into(),
                }),
                _ => {
                    let idx = self.open_block(BlockKind::Text, &mut out);
                    out.push(Ev::TextStart { index: idx });
                    out.push(Ev::TextDelta {
                        index: idx,
                        text: text.into(),
                    });
                }
            }
        }
        let empty = Vec::new();
        for tc in delta["tool_calls"].as_array().unwrap_or(&empty) {
            let oai_idx = tc["index"].as_u64().unwrap_or(0);
            let idx = match self.tool_index_map.get(&oai_idx) {
                Some(&idx) => idx,
                None => {
                    let idx = self.open_block(BlockKind::Tool, &mut out);
                    self.tool_index_map.insert(oai_idx, idx);
                    let id = Some(s(tc, "id"))
                        .filter(|i| !i.is_empty())
                        .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4().simple()));
                    out.push(Ev::ToolStart {
                        index: idx,
                        id,
                        name: s(&tc["function"], "name"),
                    });
                    idx
                }
            };
            if let Some(args) = tc["function"]["arguments"]
                .as_str()
                .filter(|a| !a.is_empty())
            {
                out.push(Ev::ArgsDelta {
                    index: idx,
                    json: args.into(),
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Ev -> Anthropic Messages SSE
// ---------------------------------------------------------------------------

pub struct AnthropicRenderer {
    pub alias: String,
    /// Indices of open thinking blocks (they need a signature_delta before stop).
    pub thinking_blocks: std::collections::HashSet<usize>,
}

impl AnthropicRenderer {
    pub fn new(alias: String) -> Self {
        Self {
            alias,
            thinking_blocks: Default::default(),
        }
    }
}

impl EventRenderer for AnthropicRenderer {
    fn render(&mut self, ev: &Ev) -> Option<String> {
        let out = match ev {
            Ev::ThinkingStart { index } => {
                self.thinking_blocks.insert(*index);
                frame(
                    "content_block_start",
                    &json!({"type": "content_block_start", "index": index,
                            "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                )
            }
            Ev::ThinkingDelta { index, text } => frame(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "thinking_delta", "thinking": text}}),
            ),
            Ev::BlockStop { index } if self.thinking_blocks.remove(index) => format!(
                "{}{}",
                frame(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": index,
                            "delta": {"type": "signature_delta", "signature": ""}}),
                ),
                frame(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": index})
                ),
            ),
            Ev::Start { id, input_tokens } => frame(
                "message_start",
                &json!({"type": "message_start", "message": {
                    "id": if id.is_empty() { format!("msg_{}", uuid::Uuid::new_v4().simple()) } else { format!("msg_{id}") },
                    "type": "message", "role": "assistant", "model": self.alias, "content": [],
                    "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": input_tokens, "output_tokens": 0}
                }}),
            ),
            Ev::TextStart { index } => frame(
                "content_block_start",
                &json!({"type": "content_block_start", "index": index,
                        "content_block": {"type": "text", "text": ""}}),
            ),
            Ev::TextDelta { index, text } => frame(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "text_delta", "text": text}}),
            ),
            Ev::ToolStart { index, id, name } => frame(
                "content_block_start",
                &json!({"type": "content_block_start", "index": index,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
            ),
            Ev::ArgsDelta { index, json } => frame(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": json}}),
            ),
            Ev::BlockStop { index } => frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            ),
            Ev::Finish {
                stop_reason,
                output_tokens,
            } => frame(
                "message_delta",
                &json!({"type": "message_delta",
                        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                        "usage": {"output_tokens": output_tokens}}),
            ),
            Ev::Done => frame("message_stop", &json!({"type": "message_stop"})),
        };
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Ev -> OpenAI Responses SSE
// ---------------------------------------------------------------------------

struct ItemState {
    item_id: String,
    output_index: usize,
    is_tool: bool,
    name: String,
    call_id: String,
    acc: String,
}

pub struct ResponsesRenderer {
    pub alias: String,
    seq: u64,
    response_id: String,
    input_tokens: u64,
    items: HashMap<usize, ItemState>,
    completed_items: Vec<Value>,
    stop_reason: String,
    output_tokens: u64,
}

impl ResponsesRenderer {
    pub fn new(alias: String) -> Self {
        Self {
            alias,
            seq: 0,
            response_id: String::new(),
            input_tokens: 0,
            items: HashMap::new(),
            completed_items: Vec::new(),
            stop_reason: "end_turn".into(),
            output_tokens: 0,
        }
    }

    fn emit(&mut self, event: &str, mut data: Value) -> String {
        data["sequence_number"] = json!(self.seq);
        self.seq += 1;
        frame(event, &data)
    }

    fn response_envelope(&self, status: &str, with_usage: bool) -> Value {
        let mut r = json!({
            "id": self.response_id,
            "object": "response",
            "status": status,
            "model": self.alias,
            "output": self.completed_items,
        });
        if with_usage {
            r["usage"] = json!({
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "total_tokens": self.input_tokens + self.output_tokens,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0},
            });
        }
        r
    }
}

impl EventRenderer for ResponsesRenderer {
    fn render(&mut self, ev: &Ev) -> Option<String> {
        match ev {
            Ev::Start { id, input_tokens } => {
                self.response_id = format!(
                    "resp_{}",
                    if id.is_empty() {
                        uuid::Uuid::new_v4().simple().to_string()
                    } else {
                        id.clone()
                    }
                );
                self.input_tokens = *input_tokens;
                let env = self.response_envelope("in_progress", false);
                Some(self.emit(
                    "response.created",
                    json!({"type": "response.created", "response": env}),
                ))
            }
            Ev::TextStart { index } => {
                let item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
                let output_index = self.completed_items.len() + self.items.len();
                let item = json!({"id": item_id, "type": "message", "role": "assistant",
                                  "status": "in_progress", "content": []});
                let mut out = self.emit(
                    "response.output_item.added",
                    json!({"type": "response.output_item.added", "output_index": output_index, "item": item}),
                );
                out.push_str(&self.emit(
                    "response.content_part.added",
                    json!({"type": "response.content_part.added", "item_id": item_id,
                           "output_index": output_index, "content_index": 0,
                           "part": {"type": "output_text", "text": "", "annotations": []}}),
                ));
                self.items.insert(
                    *index,
                    ItemState {
                        item_id,
                        output_index,
                        is_tool: false,
                        name: String::new(),
                        call_id: String::new(),
                        acc: String::new(),
                    },
                );
                Some(out)
            }
            Ev::TextDelta { index, text } => {
                let it = self.items.get_mut(index)?;
                it.acc.push_str(text);
                let (item_id, output_index) = (it.item_id.clone(), it.output_index);
                Some(self.emit(
                    "response.output_text.delta",
                    json!({"type": "response.output_text.delta", "item_id": item_id,
                           "output_index": output_index, "content_index": 0, "delta": text}),
                ))
            }
            Ev::ToolStart { index, id, name } => {
                let item_id = format!("fc_{}", uuid::Uuid::new_v4().simple());
                let output_index = self.completed_items.len() + self.items.len();
                let item = json!({"id": item_id, "type": "function_call", "call_id": id,
                                  "name": name, "arguments": "", "status": "in_progress"});
                let out = self.emit(
                    "response.output_item.added",
                    json!({"type": "response.output_item.added", "output_index": output_index, "item": item}),
                );
                self.items.insert(
                    *index,
                    ItemState {
                        item_id,
                        output_index,
                        is_tool: true,
                        name: name.clone(),
                        call_id: id.clone(),
                        acc: String::new(),
                    },
                );
                Some(out)
            }
            Ev::ArgsDelta { index, json: delta } => {
                let it = self.items.get_mut(index)?;
                it.acc.push_str(delta);
                let (item_id, output_index) = (it.item_id.clone(), it.output_index);
                Some(self.emit(
                    "response.function_call_arguments.delta",
                    json!({"type": "response.function_call_arguments.delta", "item_id": item_id,
                           "output_index": output_index, "delta": delta}),
                ))
            }
            Ev::BlockStop { index } => {
                let it = self.items.remove(index)?;
                let mut out = String::new();
                let done_item = if it.is_tool {
                    out.push_str(&self.emit(
                        "response.function_call_arguments.done",
                        json!({"type": "response.function_call_arguments.done", "item_id": it.item_id,
                               "output_index": it.output_index, "arguments": it.acc}),
                    ));
                    json!({"id": it.item_id, "type": "function_call", "call_id": it.call_id,
                           "name": it.name, "arguments": if it.acc.is_empty() { "{}".into() } else { it.acc.clone() },
                           "status": "completed"})
                } else {
                    out.push_str(&self.emit(
                        "response.output_text.done",
                        json!({"type": "response.output_text.done", "item_id": it.item_id,
                               "output_index": it.output_index, "content_index": 0, "text": it.acc}),
                    ));
                    out.push_str(&self.emit(
                        "response.content_part.done",
                        json!({"type": "response.content_part.done", "item_id": it.item_id,
                               "output_index": it.output_index, "content_index": 0,
                               "part": {"type": "output_text", "text": it.acc, "annotations": []}}),
                    ));
                    json!({"id": it.item_id, "type": "message", "role": "assistant", "status": "completed",
                           "content": [{"type": "output_text", "text": it.acc, "annotations": []}]})
                };
                out.push_str(&self.emit(
                    "response.output_item.done",
                    json!({"type": "response.output_item.done", "output_index": it.output_index, "item": done_item}),
                ));
                self.completed_items.push(done_item);
                Some(out)
            }
            Ev::Finish {
                stop_reason,
                output_tokens,
            } => {
                self.stop_reason = stop_reason.clone();
                self.output_tokens = *output_tokens;
                None
            }
            Ev::Done => {
                let status = if self.stop_reason == "max_tokens" {
                    "incomplete"
                } else {
                    "completed"
                };
                let env = self.response_envelope(status, true);
                Some(self.emit(
                    "response.completed",
                    json!({"type": "response.completed", "response": env}),
                ))
            }
            // Reasoning is not surfaced to Codex in the MVP subset.
            Ev::ThinkingStart { .. } | Ev::ThinkingDelta { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(parser: &mut dyn EventParser, renderer: &mut dyn EventRenderer, sse: &str) -> String {
        let mut sp = SseParser::default();
        let mut out = String::new();
        for e in sp.push(sse.as_bytes()) {
            for ev in parser.on_sse(&e) {
                if let Some(f) = renderer.render(&ev) {
                    out.push_str(&f);
                }
            }
        }
        out
    }

    const CHAT_TOOL_SSE: &str = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Let me check.\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );

    fn parse_frames(out: &str) -> Vec<(String, Value)> {
        let mut sp = SseParser::default();
        sp.push(out.as_bytes())
            .into_iter()
            .map(|e| {
                (
                    e.event.unwrap_or_default(),
                    serde_json::from_str(&e.data).unwrap(),
                )
            })
            .collect()
    }

    fn find<'a>(frames: &'a [(String, Value)], event: &str) -> &'a Value {
        &frames.iter().find(|(e, _)| e == event).unwrap().1
    }

    #[test]
    fn chat_stream_to_anthropic_sse() {
        let out = run(
            &mut ChatParser::default(),
            &mut AnthropicRenderer::new("claude-gpt-coder".into()),
            CHAT_TOOL_SSE,
        );
        let frames = parse_frames(&out);
        assert_eq!(frames[0].0, "message_start");
        assert_eq!(frames[0].1["message"]["model"], "claude-gpt-coder");
        let text_delta = find(&frames, "content_block_delta");
        assert_eq!(
            text_delta["delta"],
            json!({"type": "text_delta", "text": "Let me check."})
        );
        let tool_start = frames
            .iter()
            .find(|(e, v)| e == "content_block_start" && v["content_block"]["type"] == "tool_use")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(
            tool_start["content_block"],
            json!({"type": "tool_use", "id": "call_9", "name": "bash", "input": {}})
        );
        let args = frames
            .iter()
            .find(|(_, v)| v["delta"]["type"] == "input_json_delta")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(args["delta"]["partial_json"], "{\"cmd\":\"ls\"}");
        let msg_delta = find(&frames, "message_delta");
        assert_eq!(msg_delta["delta"]["stop_reason"], "tool_use");
        assert_eq!(msg_delta["usage"]["output_tokens"], 7);
        assert_eq!(frames.last().unwrap().0, "message_stop");
        // text block closed before tool block opened
        let events: Vec<&str> = frames.iter().map(|(e, _)| e.as_str()).collect();
        let close = events
            .iter()
            .position(|e| *e == "content_block_stop")
            .unwrap();
        let tool_open = frames
            .iter()
            .position(|(e, v)| {
                e == "content_block_start" && v["content_block"]["type"] == "tool_use"
            })
            .unwrap();
        assert!(close < tool_open);
    }

    const ANTHROPIC_TOOL_SSE: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_a\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":20}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Running tests.\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_123\",\"name\":\"bash\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\\\"npm test\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":8}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    #[test]
    fn anthropic_stream_to_responses_sse() {
        let out = run(
            &mut AnthropicParser,
            &mut ResponsesRenderer::new("claude-sonnet".into()),
            ANTHROPIC_TOOL_SSE,
        );
        assert!(out.starts_with("event: response.created\n"));
        assert!(out.contains(r#""delta":"Running tests.""#));
        assert!(out.contains("response.function_call_arguments.delta"));
        assert!(out.contains(r#""call_id":"toolu_123""#));
        assert!(out.contains("event: response.completed\n"));
        // final response carries both output items and usage
        let completed = out
            .split("event: response.completed\ndata: ")
            .nth(1)
            .unwrap();
        let v: Value = serde_json::from_str(completed.trim()).unwrap();
        assert_eq!(
            v["response"]["output"][0]["content"][0]["text"],
            "Running tests."
        );
        assert_eq!(
            v["response"]["output"][1]["arguments"],
            "{\"cmd\":\"npm test\"}"
        );
        assert_eq!(v["response"]["usage"]["total_tokens"], 28);
        assert_eq!(v["response"]["model"], "claude-sonnet");
    }

    const CHAT_REASONING_SSE: &str = concat!(
        "data: {\"id\":\"c2\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning\":\"thinking hard\"}}]}\n\n",
        "data: {\"id\":\"c2\",\"choices\":[{\"delta\":{\"reasoning_content\":\" about tests\"}}]}\n\n",
        "data: {\"id\":\"c2\",\"choices\":[{\"delta\":{\"content\":\"Done.\"}}]}\n\n",
        "data: {\"id\":\"c2\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn reasoning_deltas_become_anthropic_thinking_blocks() {
        let out = run(
            &mut ChatParser::default(),
            &mut AnthropicRenderer::new("claude-kimi-coder".into()),
            CHAT_REASONING_SSE,
        );
        let frames = parse_frames(&out);
        let think_start = find(&frames, "content_block_start");
        assert_eq!(think_start["content_block"]["type"], "thinking");
        let deltas: Vec<&Value> = frames
            .iter()
            .filter(|(e, _)| e == "content_block_delta")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            deltas[0]["delta"],
            json!({"type": "thinking_delta", "thinking": "thinking hard"})
        );
        assert_eq!(
            deltas[1]["delta"],
            json!({"type": "thinking_delta", "thinking": " about tests"})
        );
        // thinking block closed with a signature_delta, then text block follows
        assert!(deltas
            .iter()
            .any(|d| d["delta"]["type"] == "signature_delta"));
        assert!(deltas
            .iter()
            .any(|d| d["delta"] == json!({"type": "text_delta", "text": "Done."})));
        assert_eq!(frames.last().unwrap().0, "message_stop");
    }

    #[test]
    fn reasoning_dropped_on_codex_path() {
        let out = run(
            &mut ChatParser::default(),
            &mut ResponsesRenderer::new("claude-kimi-coder".into()),
            CHAT_REASONING_SSE,
        );
        assert!(!out.contains("thinking"));
        assert!(out.contains(r#""delta":"Done.""#));
        assert!(out.contains("event: response.completed"));
    }

    #[test]
    fn phase_tracks_fallback_boundary() {
        let mut phase = StreamPhase::NotStarted;
        phase.advance(&Ev::Start {
            id: "x".into(),
            input_tokens: 0,
        });
        assert_eq!(phase, StreamPhase::MessageStarted);
        phase.advance(&Ev::ToolStart {
            index: 0,
            id: "t".into(),
            name: "bash".into(),
        });
        assert_eq!(phase, StreamPhase::ToolUseStarted);
        phase.advance(&Ev::BlockStop { index: 0 });
        assert_eq!(phase, StreamPhase::ToolUseCompleted);
        phase.advance(&Ev::Done);
        assert_eq!(phase, StreamPhase::Done);
    }

    #[test]
    fn sse_parser_handles_split_chunks() {
        let mut p = SseParser::default();
        assert!(p.push(b"event: message_start\ndata: {\"a\":").is_empty());
        let evs = p.push(b"1}\n\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("message_start"));
        assert_eq!(evs[1].data, "[DONE]");
    }
}
