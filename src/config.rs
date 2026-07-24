use serde::Deserialize;
use std::collections::BTreeMap;

fn d_true() -> bool {
    true
}
fn d_bind() -> String {
    "127.0.0.1:4000".into()
}
fn d_timeout() -> u64 {
    120_000
}
fn d_attempts() -> u32 {
    3
}
fn d_retryable() -> Vec<u16> {
    vec![408, 429, 500, 502, 503, 504]
}
fn d_blocked() -> CompatLevel {
    CompatLevel::Blocked
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub fallback: FallbackConfig,
    /// Pattern-based routing for model ids that aren't gateway aliases,
    /// checked in order before default_model. Claude Code subagents pin
    /// claude-opus-* ids and background tasks pin claude-haiku-* ids, so this
    /// is where those get pointed at gateway models.
    #[serde(default)]
    pub model_map: Vec<ModelMapEntry>,
    /// Alias to route unknown model ids to when nothing in model_map matches.
    #[serde(default)]
    pub default_model: Option<String>,
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "d_bind")]
    pub bind: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { bind: d_bind() }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AuthConfig {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub tokens: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FallbackConfig {
    #[serde(default = "d_attempts")]
    pub max_attempts: u32,
    #[serde(default = "d_retryable")]
    pub retryable_statuses: Vec<u16>,
    #[serde(default)]
    pub allow_degraded_fallback: bool,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            max_attempts: d_attempts(),
            retryable_statuses: d_retryable(),
            allow_degraded_fallback: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelMapEntry {
    /// Glob-style pattern with `*` wildcards, e.g. "claude-opus-*".
    pub pattern: String,
    /// Gateway model alias to route matching requests to.
    pub model: String,
}

/// Minimal glob: `*` matches any (possibly empty) substring.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let segs: Vec<&str> = pattern.split('*').collect();
    if segs.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, seg) in segs.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 {
            match rest.strip_prefix(seg) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == segs.len() - 1 {
            return rest.ends_with(seg);
        } else {
            match rest.find(seg) {
                Some(pos) => rest = &rest[pos + seg.len()..],
                None => return false,
            }
        }
    }
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub log_prompts: bool,
    #[serde(default = "d_true")]
    pub log_tool_calls: bool,
    #[serde(default)]
    pub redact_headers: Vec<String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_prompts: false,
            log_tool_calls: true,
            redact_headers: vec![
                "authorization".into(),
                "x-api-key".into(),
                "modal-key".into(),
                "modal-secret".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatLevel {
    Full,
    Tools,
    DegradedTools,
    TextOnly,
    ResponsesBridge,
    Blocked,
}

impl CompatLevel {
    pub fn is_degraded(self) -> bool {
        matches!(self, Self::DegradedTools | Self::ResponsesBridge)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Compatibility {
    #[serde(default = "d_blocked")]
    pub claude_code: CompatLevel,
    #[serde(default = "d_blocked")]
    pub codex: CompatLevel,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub display_name: Option<String>,
    pub compatibility: Compatibility,
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub fallback: Option<ModelFallback>,
    /// Routable but not listed in /v1/models (internal role targets).
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelFallback {
    #[serde(default)]
    pub allow_degraded_fallback: Option<bool>,
    #[serde(default)]
    pub routes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    OpenaiCompatible,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteConfig {
    pub id: String,
    pub provider: ProviderKind,
    pub model: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Extra headers sent verbatim on every upstream request (custom
    /// endpoints with non-standard auth, e.g. Modal-Key/Modal-Secret).
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    /// Top-level body params to strip before sending upstream (for providers
    /// that reject params they don't control, e.g. Kimi K2.7 fixes temperature).
    #[serde(default)]
    pub drop_params: Vec<String>,
    #[serde(default)]
    pub capabilities: RouteCapabilities,
}

impl RouteConfig {
    pub fn resolve_api_key(&self) -> Option<String> {
        self.api_key.clone().or_else(|| {
            self.api_key_env
                .as_ref()
                .and_then(|e| std::env::var(e).ok())
        })
    }

    pub fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolCap {
    Native,
    Openai,
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FeatureCap {
    Native,
    #[default]
    None,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteCapabilities {
    #[serde(default = "d_true")]
    pub text: bool,
    #[serde(default)]
    pub tools: ToolCap,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub streaming: Option<String>,
    #[serde(default)]
    pub thinking: FeatureCap,
    #[serde(default)]
    pub cache_control: FeatureCap,
}

impl Default for RouteCapabilities {
    fn default() -> Self {
        Self {
            text: true,
            tools: ToolCap::None,
            images: false,
            streaming: None,
            thinking: FeatureCap::None,
            cache_control: FeatureCap::None,
        }
    }
}

impl Config {
    /// Global route index: route id -> (owning model alias, route).
    pub fn route_index(&self) -> BTreeMap<String, (String, RouteConfig)> {
        let mut idx = BTreeMap::new();
        for (alias, m) in &self.models {
            for r in &m.routes {
                idx.insert(r.id.clone(), (alias.clone(), r.clone()));
            }
        }
        idx
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.models.is_empty() {
            return Err("config has no models".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for (alias, m) in &self.models {
            if m.routes.is_empty() {
                return Err(format!("model '{alias}' has no routes"));
            }
            for r in &m.routes {
                if !ids.insert(r.id.clone()) {
                    return Err(format!("duplicate route id '{}'", r.id));
                }
                if r.api_key.is_none() && r.api_key_env.is_none() {
                    return Err(format!("route '{}' needs api_key or api_key_env", r.id));
                }
            }
        }
        if let Some(d) = &self.default_model {
            if !self.models.contains_key(d) {
                return Err(format!("default_model '{d}' is not a configured model"));
            }
        }
        for e in &self.model_map {
            if !self.models.contains_key(&e.model) {
                return Err(format!(
                    "model_map '{}' targets unknown model '{}'",
                    e.pattern, e.model
                ));
            }
        }
        for (alias, m) in &self.models {
            if let Some(routes) = m.fallback.as_ref().and_then(|f| f.routes.as_ref()) {
                for id in routes {
                    if !ids.contains(id) {
                        return Err(format!(
                            "model '{alias}' fallback references unknown route '{id}'"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_example() {
        let yaml = r#"
server:
  bind: "127.0.0.1:4000"
auth:
  mode: bearer
  tokens: ["local-dev-token"]
fallback:
  max_attempts: 3
  retryable_statuses: [408, 429, 500, 502, 503, 504]
  allow_degraded_fallback: false
models:
  claude-sonnet:
    display_name: "Claude Sonnet via Gateway"
    compatibility: { claude_code: full, codex: full }
    routes:
      - id: anthropic-sonnet-primary
        provider: anthropic
        model: claude-sonnet-4-6
        base_url: "https://api.anthropic.com"
        api_key_env: ANTHROPIC_API_KEY
        capabilities: { text: true, tools: native, streaming: anthropic_sse, thinking: native, cache_control: native }
  claude-qwen-local:
    display_name: "Qwen Local Degraded"
    compatibility: { claude_code: degraded_tools, codex: responses_bridge }
    routes:
      - id: local-qwen
        provider: openai_compatible
        model: qwen3-coder
        base_url: "http://localhost:8000/v1"
        api_key: "dummy"
        capabilities: { text: true, tools: openai, streaming: openai_sse }
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.models["claude-sonnet"].compatibility.codex,
            CompatLevel::Full
        );
        assert!(cfg.models["claude-qwen-local"]
            .compatibility
            .codex
            .is_degraded());
        assert_eq!(cfg.route_index()["local-qwen"].0, "claude-qwen-local");
    }

    #[test]
    fn glob_match_patterns() {
        assert!(glob_match("claude-opus-*", "claude-opus-4-8"));
        assert!(glob_match("claude-haiku-*", "claude-haiku-4-5-20260101"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("claude-*-coder", "claude-kimi-coder"));
        assert!(!glob_match("claude-opus-*", "claude-sonnet-4-6"));
        assert!(!glob_match("claude-opus", "claude-opus-4-8"));
        assert!(glob_match("claude-opus", "claude-opus"));
    }

    #[test]
    fn model_map_must_target_known_models() {
        let yaml = r#"
model_map:
  - { pattern: "claude-opus-*", model: missing }
models:
  a:
    compatibility: { claude_code: full }
    routes:
      - { id: r1, provider: anthropic, model: m, base_url: "http://x", api_key: k }
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.validate().unwrap_err().contains("model_map"));
    }

    #[test]
    fn rejects_duplicate_route_ids() {
        let yaml = r#"
models:
  a:
    compatibility: { claude_code: full }
    routes:
      - { id: r1, provider: anthropic, model: m, base_url: "http://x", api_key: k }
  b:
    compatibility: { claude_code: full }
    routes:
      - { id: r1, provider: anthropic, model: m, base_url: "http://x", api_key: k }
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.validate().unwrap_err().contains("duplicate"));
    }
}
