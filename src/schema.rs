//! User-facing config format. Four sections — `server`, `providers`,
//! `models`, `clients` — compiled into the internal routing config
//! (`crate::config::Config`), so the engine and its semantics stay unchanged.
//!
//! ```yaml
//! server:
//!   token: local-dev-token
//! providers:                        # only for what presets can't know
//!   azure: { resource: my-foundry, api_key_env: AZURE_AI_API_KEY }
//! models:
//!   gpt-55: azure/gpt-5.5
//!   kimi-27: openrouter/moonshotai/kimi-k2.7-code
//! clients:
//!   claude_code:
//!     main: [gpt-55, kimi-27]       # list = pre-stream fallback chain
//!     subagent: kimi-27             # claude-opus-* requests
//!     background: kimi-27           # claude-haiku-* requests
//!   codex:
//!     main: kimi-27
//! ```

use crate::config::{
    AuthConfig, CompatLevel, Compatibility, Config, FallbackConfig, FeatureCap, ModelConfig,
    ModelMapEntry, ProviderKind, RouteCapabilities, RouteConfig, ServerConfig, TelemetryConfig,
    ToolCap,
};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
    /// Optional: name models for reuse, display names, or overrides. Roles
    /// may also reference `provider/model-id` strings inline.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
    pub clients: ClientsSection,
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct ServerSection {
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub tokens: Vec<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ProviderEntry {
    /// "anthropic" or "openai" wire protocol; presets imply it.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Azure AI Foundry resource name.
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ModelEntry {
    /// `name: provider/model-id`
    Short(String),
    /// `name: [provider/a, provider/b]` — a named fallback chain
    Chain(Vec<String>),
    Long(ModelLong),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
pub struct ModelLong {
    /// `provider/model-id`, or a list of them for a fallback chain
    pub model: OneOrMany,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub images: Option<bool>,
    #[serde(default)]
    pub tools: Option<ToolCap>,
    #[serde(default)]
    pub thinking: Option<FeatureCap>,
    #[serde(default)]
    pub cache_control: Option<FeatureCap>,
    #[serde(default)]
    pub drop_params: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Which clients may select this model directly (default: all configured).
    #[serde(default)]
    pub expose: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct ClientsSection {
    #[serde(default)]
    pub claude_code: Option<ClientRoles>,
    #[serde(default)]
    pub codex: Option<ClientRoles>,
}

#[derive(Debug, Deserialize)]
pub struct ClientRoles {
    pub main: RoleValue,
    #[serde(default)]
    pub subagent: Option<RoleValue>,
    #[serde(default)]
    pub background: Option<RoleValue>,
    /// Where unrecognized model ids go: a role name, a model name, or
    /// "reject" to return 404. Defaults to `main`.
    #[serde(default)]
    pub unknown: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RoleValue {
    One(String),
    Chain(Vec<String>),
}

impl RoleValue {
    fn names(&self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s.clone()],
            Self::Chain(v) => v.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Provider presets & model quirk table
// ---------------------------------------------------------------------------

/// (wire kind, base_url, default api key env var)
fn provider_preset(name: &str) -> Option<(ProviderKind, Option<&'static str>, &'static str)> {
    use ProviderKind::{Anthropic, OpenaiCompatible};
    match name {
        "openrouter" => Some((
            OpenaiCompatible,
            Some("https://openrouter.ai/api/v1"),
            "OPENROUTER_API_KEY",
        )),
        "openai" => Some((
            OpenaiCompatible,
            Some("https://api.openai.com/v1"),
            "OPENAI_API_KEY",
        )),
        "anthropic" => Some((
            Anthropic,
            Some("https://api.anthropic.com"),
            "ANTHROPIC_API_KEY",
        )),
        "moonshot" => Some((
            OpenaiCompatible,
            Some("https://api.moonshot.ai/v1"),
            "MOONSHOT_API_KEY",
        )),
        "fireworks" => Some((
            OpenaiCompatible,
            Some("https://api.fireworks.ai/inference/v1"),
            "FIREWORKS_API_KEY",
        )),
        "together" => Some((
            OpenaiCompatible,
            Some("https://api.together.xyz/v1"),
            "TOGETHER_API_KEY",
        )),
        "groq" => Some((
            OpenaiCompatible,
            Some("https://api.groq.com/openai/v1"),
            "GROQ_API_KEY",
        )),
        "deepinfra" => Some((
            OpenaiCompatible,
            Some("https://api.deepinfra.com/v1/openai"),
            "DEEPINFRA_API_KEY",
        )),
        "deepseek" => Some((
            OpenaiCompatible,
            Some("https://api.deepseek.com/v1"),
            "DEEPSEEK_API_KEY",
        )),
        "mistral" => Some((
            OpenaiCompatible,
            Some("https://api.mistral.ai/v1"),
            "MISTRAL_API_KEY",
        )),
        "xai" => Some((OpenaiCompatible, Some("https://api.x.ai/v1"), "XAI_API_KEY")),
        "cerebras" => Some((
            OpenaiCompatible,
            Some("https://api.cerebras.ai/v1"),
            "CEREBRAS_API_KEY",
        )),
        // Local runtime; no key required (env is checked but may be unset).
        "ollama" => Some((
            OpenaiCompatible,
            Some("http://localhost:11434/v1"),
            "OLLAMA_API_KEY",
        )),
        // Azure AI Foundry's OpenAI-compatible endpoint; needs `resource`.
        "azure" => Some((OpenaiCompatible, None, "AZURE_AI_API_KEY")),
        _ => None,
    }
}

#[derive(Clone)]
struct ResolvedProvider {
    kind: ProviderKind,
    base_url: String,
    api_key: Option<String>,
    api_key_env: Option<String>,
}

fn resolve_provider(name: &str, entry: Option<&ProviderEntry>) -> Result<ResolvedProvider, String> {
    let preset = provider_preset(name);
    let e = entry.cloned().unwrap_or_default();
    let kind = match e.kind.as_deref() {
        Some("anthropic") => ProviderKind::Anthropic,
        Some("openai") => ProviderKind::OpenaiCompatible,
        Some(other) => {
            return Err(format!(
                "provider '{name}': unknown type '{other}' (use anthropic|openai)"
            ))
        }
        None => preset
            .map(|(k, _, _)| k)
            .unwrap_or(ProviderKind::OpenaiCompatible),
    };
    let base_url = e
        .base_url
        .or_else(|| {
            if name == "azure" || preset.is_none() {
                e.resource
                    .as_ref()
                    .map(|r| format!("https://{r}.services.ai.azure.com/openai/v1"))
            } else {
                None
            }
        })
        .or_else(|| preset.and_then(|(_, b, _)| b.map(String::from)))
        .ok_or_else(|| {
            format!("provider '{name}' is not a known preset; declare it under `providers:` with a base_url")
        })?;
    let api_key_env = e.api_key_env.or_else(|| {
        if e.api_key.is_none() {
            preset.map(|(_, _, env)| env.to_string())
        } else {
            None
        }
    });
    Ok(ResolvedProvider {
        kind,
        base_url,
        api_key: e.api_key,
        api_key_env,
    })
}

struct Quirks {
    tools: ToolCap,
    images: bool,
    thinking: FeatureCap,
    cache_control: FeatureCap,
    drop_params: Vec<String>,
    timeout_ms: u64,
}

/// Built-in per-model-family defaults so known models need zero capability config.
fn quirks(model_id: &str) -> Quirks {
    let id = model_id.to_lowercase();
    if id.contains("kimi") {
        // Forced thinking, fixed sampling (errors on client temperature/top_p),
        // preserved reasoning, native multimodal.
        Quirks {
            tools: ToolCap::Openai,
            images: true,
            thinking: FeatureCap::Native,
            cache_control: FeatureCap::None,
            drop_params: vec!["temperature".into(), "top_p".into()],
            timeout_ms: 300_000,
        }
    } else if id.contains("claude") {
        Quirks {
            tools: ToolCap::Native,
            images: true,
            thinking: FeatureCap::Native,
            cache_control: FeatureCap::Native,
            drop_params: vec![],
            timeout_ms: 120_000,
        }
    } else if id.contains("gpt") || id.starts_with('o') {
        Quirks {
            tools: ToolCap::Openai,
            images: true,
            thinking: FeatureCap::None,
            cache_control: FeatureCap::None,
            drop_params: vec![],
            timeout_ms: 120_000,
        }
    } else {
        Quirks {
            tools: ToolCap::Openai,
            images: false,
            thinking: FeatureCap::None,
            cache_control: FeatureCap::None,
            drop_params: vec![],
            timeout_ms: 120_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

struct ResolvedModel {
    display_name: Option<String>,
    expose: Option<Vec<String>>,
    route_template: RouteConfig,
}

struct ChainDef {
    elems: Vec<String>,
    display_name: Option<String>,
    expose: Option<Vec<String>>,
}

const ROLE_NAMES: [&str; 3] = ["main", "subagent", "background"];

/// "openrouter/moonshotai/kimi-k2.7-code" -> "Kimi K2.7 Code"
fn pretty_model_name(id: &str) -> String {
    let last = id.rsplit('/').next().unwrap_or(id);
    last.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            if w.eq_ignore_ascii_case("gpt") {
                "GPT".to_string()
            } else {
                let mut chars = w.chars();
                match chars.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl FileConfig {
    fn resolve_single(
        &self,
        spec: &str,
        long: Option<&ModelLong>,
    ) -> Result<ResolvedModel, String> {
        let (provider_name, model_id) = spec
            .split_once('/')
            .ok_or_else(|| format!("expected 'provider/model-id', got '{spec}'"))?;
        let provider = resolve_provider(provider_name, self.providers.get(provider_name))?;
        let q = quirks(model_id);
        Ok(ResolvedModel {
            display_name: long.and_then(|l| l.display_name.clone()),
            expose: long.and_then(|l| l.expose.clone()),
            route_template: RouteConfig {
                id: String::new(), // set per alias below
                provider: provider.kind,
                model: model_id.to_string(),
                base_url: provider.base_url,
                api_key: provider.api_key,
                api_key_env: provider.api_key_env,
                timeout_ms: long.and_then(|l| l.timeout_ms).unwrap_or(q.timeout_ms),
                drop_params: long
                    .and_then(|l| l.drop_params.clone())
                    .unwrap_or(q.drop_params),
                capabilities: RouteCapabilities {
                    text: true,
                    tools: long.and_then(|l| l.tools).unwrap_or(q.tools),
                    images: long.and_then(|l| l.images).unwrap_or(q.images),
                    streaming: None,
                    thinking: long.and_then(|l| l.thinking).unwrap_or(q.thinking),
                    cache_control: long
                        .and_then(|l| l.cache_control)
                        .unwrap_or(q.cache_control),
                },
            },
        })
    }

    pub fn compile(&self) -> Result<Config, String> {
        // 1. Resolve named single models; collect named chains.
        let mut resolved: BTreeMap<String, ResolvedModel> = BTreeMap::new();
        let mut chains: BTreeMap<String, ChainDef> = BTreeMap::new();
        for (name, entry) in &self.models {
            if ROLE_NAMES.contains(&name.as_str()) || name == "unknown" {
                return Err(format!("model name '{name}' collides with a role name"));
            }
            match entry {
                ModelEntry::Short(s) => {
                    let m = self
                        .resolve_single(s, None)
                        .map_err(|e| format!("model '{name}': {e}"))?;
                    resolved.insert(name.clone(), m);
                }
                ModelEntry::Long(l) => match &l.model {
                    OneOrMany::One(s) => {
                        let m = self
                            .resolve_single(s, Some(l))
                            .map_err(|e| format!("model '{name}': {e}"))?;
                        resolved.insert(name.clone(), m);
                    }
                    OneOrMany::Many(specs) => {
                        // Long form with a chain: overrides apply to every
                        // element; display_name/expose belong to the chain.
                        if specs.is_empty() {
                            return Err(format!("model '{name}': chain is empty"));
                        }
                        let mut elems = Vec::new();
                        for (i, spec) in specs.iter().enumerate() {
                            let mut m = self
                                .resolve_single(spec, Some(l))
                                .map_err(|e| format!("model '{name}': {e}"))?;
                            m.display_name = None;
                            m.expose = Some(vec![]); // anonymous element
                            let key = format!("{name}[{i}]");
                            resolved.insert(key.clone(), m);
                            elems.push(key);
                        }
                        chains.insert(
                            name.clone(),
                            ChainDef {
                                elems,
                                display_name: l.display_name.clone(),
                                expose: l.expose.clone(),
                            },
                        );
                    }
                },
                ModelEntry::Chain(elems) => {
                    if elems.is_empty() {
                        return Err(format!("model '{name}': chain is empty"));
                    }
                    chains.insert(
                        name.clone(),
                        ChainDef {
                            elems: elems.clone(),
                            display_name: None,
                            expose: None,
                        },
                    );
                }
            }
        }

        // 2. Inline `provider/model-id` references (in chains or role values)
        //    become anonymous models: routable, but never listed or directly
        //    selectable.
        let mut inline_refs: Vec<String> = chains.values().flat_map(|c| c.elems.clone()).collect();
        for roles in [
            self.clients.claude_code.as_ref(),
            self.clients.codex.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let values = [
                Some(&roles.main),
                roles.subagent.as_ref(),
                roles.background.as_ref(),
            ];
            inline_refs.extend(values.into_iter().flatten().flat_map(RoleValue::names));
        }
        for name in inline_refs {
            if !resolved.contains_key(&name) && !chains.contains_key(&name) && name.contains('/') {
                let mut m = self.resolve_single(&name, None)?;
                m.expose = Some(vec![]);
                resolved.insert(name, m);
            }
        }

        // 3. Validate chain elements (must be named single models or inline).
        for (name, def) in &chains {
            for e in &def.elems {
                if !resolved.contains_key(e) {
                    return Err(format!(
                        "model '{name}': chain element '{e}' is not a model"
                    ));
                }
            }
        }

        let mut models: BTreeMap<String, ModelConfig> = BTreeMap::new();
        let mut model_map: Vec<ModelMapEntry> = Vec::new();
        let insert = |models: &mut BTreeMap<String, ModelConfig>, alias: String, m: ModelConfig| {
            if models.insert(alias.clone(), m).is_some() {
                return Err(format!(
                    "alias '{alias}' is defined twice (role/model name collision)"
                ));
            }
            Ok(())
        };

        let clients: [(&str, Option<&ClientRoles>); 2] = [
            ("claude_code", self.clients.claude_code.as_ref()),
            ("codex", self.clients.codex.as_ref()),
        ];
        if clients.iter().all(|(_, c)| c.is_none()) {
            return Err("clients: configure at least one of claude_code, codex".into());
        }

        // Named models & chains: one bare alias per name, usable by every
        // exposed client — the name in the config IS the model id everywhere.
        // (Claude Code's model *discovery* drops non-Claude-looking ids, but
        // explicitly configured names are sent verbatim, so bare names work.)
        let level_for = |routes: &[RouteConfig]| -> CompatLevel {
            if routes.iter().all(|r| r.provider == ProviderKind::Anthropic) {
                CompatLevel::Full
            } else {
                CompatLevel::Tools
            }
        };
        type NamedModelEntry = (String, Vec<String>, Option<String>, Option<Vec<String>>);
        let named: Vec<NamedModelEntry> = resolved
            .iter()
            .filter(|(_, rm)| rm.expose.as_deref() != Some(&[])) // skip anonymous inline models
            .map(|(n, rm)| {
                (
                    n.clone(),
                    vec![n.clone()],
                    rm.display_name.clone(),
                    rm.expose.clone(),
                )
            })
            .chain(chains.iter().map(|(n, def)| {
                (
                    n.clone(),
                    def.elems.clone(),
                    def.display_name.clone(),
                    def.expose.clone(),
                )
            }))
            .collect();
        for (name, elems, display_name, expose) in named {
            let client_on = |c: &str, cfg_present: bool| {
                cfg_present && expose.as_ref().is_none_or(|e| e.iter().any(|x| x == c))
            };
            let for_claude = client_on("claude_code", self.clients.claude_code.is_some());
            let for_codex = client_on("codex", self.clients.codex.is_some());
            if !for_claude && !for_codex {
                continue;
            }
            let build = |alias: &str| -> ModelConfig {
                let routes: Vec<RouteConfig> = elems
                    .iter()
                    .map(|e| {
                        let mut r = resolved[e].route_template.clone();
                        r.id = format!("{alias}--{e}");
                        r
                    })
                    .collect();
                let display = display_name.clone().unwrap_or_else(|| {
                    elems
                        .iter()
                        .map(|e| {
                            resolved[e].display_name.clone().unwrap_or_else(|| {
                                pretty_model_name(&resolved[e].route_template.model)
                            })
                        })
                        .collect::<Vec<_>>()
                        .join(" → ")
                });
                // Claude Code is protocol-native only on Anthropic upstreams;
                // Codex's Responses front door is translated either way.
                let claude_level = level_for(&routes);
                ModelConfig {
                    display_name: Some(display),
                    compatibility: Compatibility {
                        claude_code: if for_claude {
                            claude_level
                        } else {
                            CompatLevel::Blocked
                        },
                        codex: if for_codex {
                            CompatLevel::Full
                        } else {
                            CompatLevel::Blocked
                        },
                    },
                    routes,
                    fallback: None,
                    hidden: false,
                }
            };
            insert(&mut models, name.clone(), build(&name))?;
            // Discovery twin: Claude Code's /model picker only lists gateway
            // models with Claude-looking ids, so each named model exposed to
            // it gets a "claude-<name>" copy. Only `models:` entries get
            // twins — roles stay internal. The bare name remains canonical.
            if for_claude {
                let mut twin = build(&format!("claude-{name}"));
                twin.compatibility.codex = CompatLevel::Blocked;
                insert(&mut models, format!("claude-{name}"), twin)?;
            }
        }

        for (client, roles) in clients {
            let Some(roles) = roles else { continue };
            let alias_of = |name: &str| -> String {
                if client == "claude_code" {
                    format!("claude-{name}")
                } else {
                    name.to_string()
                }
            };
            let compat_for = |routes: &[RouteConfig]| -> Compatibility {
                // Anthropic upstreams are protocol-native for Claude Code;
                // anything translated is "tools" level.
                let full = routes.iter().all(|r| r.provider == ProviderKind::Anthropic);
                let level = if full {
                    CompatLevel::Full
                } else {
                    CompatLevel::Tools
                };
                match client {
                    "claude_code" => Compatibility {
                        claude_code: level,
                        codex: CompatLevel::Blocked,
                    },
                    _ => Compatibility {
                        claude_code: CompatLevel::Blocked,
                        codex: CompatLevel::Full,
                    },
                }
            };

            // Role aliases are internal routing targets only — hidden from
            // /v1/models. Users select the names they defined in `models:`.
            let role_values: [(&str, Option<RoleValue>); 3] = [
                ("main", Some(roles.main.clone())),
                ("subagent", roles.subagent.clone()),
                ("background", roles.background.clone()),
            ];
            for (role, value) in &role_values {
                let Some(value) = value else { continue };
                let chain = resolve_chain(value, roles, &resolved, &chains, 0)
                    .map_err(|e| format!("clients.{client}.{role}: {e}"))?;
                let alias = alias_of(role);
                let routes: Vec<RouteConfig> = chain
                    .iter()
                    .map(|model_name| {
                        let mut r = resolved[model_name].route_template.clone();
                        r.id = format!("{alias}--{model_name}");
                        r
                    })
                    .collect();
                let display = chain
                    .iter()
                    .map(|name| {
                        resolved[name].display_name.clone().unwrap_or_else(|| {
                            pretty_model_name(&resolved[name].route_template.model)
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(" → ");
                let m = ModelConfig {
                    display_name: Some(display),
                    compatibility: compat_for(&routes),
                    routes,
                    fallback: None,
                    hidden: true,
                };
                insert(&mut models, alias, m)?;
            }

            // Unknown-id routing. Claude Code pins concrete ids: opus ->
            // subagents, haiku -> background tasks; everything else claude-*
            // (incl. sonnet defaults) -> the unknown target (default: main).
            let unknown_target = match roles.unknown.as_deref() {
                Some("reject") => None,
                Some(name) => Some(name.to_string()),
                None => Some("main".to_string()),
            };
            if let Some(target) = unknown_target {
                // role names resolve to this client's role alias; model names
                // are bare
                let target_alias = if ROLE_NAMES.contains(&target.as_str())
                    && models.contains_key(&alias_of(&target))
                {
                    alias_of(&target)
                } else if models.contains_key(&target) {
                    target
                } else {
                    return Err(format!(
                        "clients.{client}.unknown: '{target}' is not a role or model of this client"
                    ));
                };
                if client == "claude_code" {
                    if roles.subagent.is_some() {
                        model_map.push(ModelMapEntry {
                            pattern: "claude-opus-*".into(),
                            model: alias_of("subagent"),
                        });
                    }
                    if roles.background.is_some() {
                        model_map.push(ModelMapEntry {
                            pattern: "claude-haiku-*".into(),
                            model: alias_of("background"),
                        });
                    }
                    model_map.push(ModelMapEntry {
                        pattern: "claude-*".into(),
                        model: target_alias,
                    });
                } else {
                    model_map.push(ModelMapEntry {
                        pattern: "*".into(),
                        model: target_alias,
                    });
                }
            }
        }

        let mut tokens = self.server.tokens.clone();
        if let Some(t) = &self.server.token {
            tokens.push(t.clone());
        }

        let cfg = Config {
            server: ServerConfig {
                bind: self
                    .server
                    .bind
                    .clone()
                    .unwrap_or_else(|| "127.0.0.1:4000".into()),
            },
            auth: AuthConfig {
                mode: Some("bearer".into()),
                tokens,
            },
            fallback: self.fallback.clone(),
            model_map,
            default_model: None,
            models,
            telemetry: self.telemetry.clone(),
        };
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Resolve a role value into a flat chain of single-model names. Entries may
/// be named models, named chains, inline `provider/model-id` strings, or
/// other roles of the same client (e.g. `subagent: main`).
fn resolve_chain(
    value: &RoleValue,
    roles: &ClientRoles,
    models: &BTreeMap<String, ResolvedModel>,
    chains: &BTreeMap<String, ChainDef>,
    depth: u8,
) -> Result<Vec<String>, String> {
    if depth > 4 {
        return Err("role references are nested too deeply (cycle?)".into());
    }
    let mut out = Vec::new();
    for name in value.names() {
        if models.contains_key(&name) {
            out.push(name);
            continue;
        }
        if let Some(def) = chains.get(&name) {
            out.extend(def.elems.clone());
            continue;
        }
        let referenced = match name.as_str() {
            "main" => Some(&roles.main),
            "subagent" => roles.subagent.as_ref(),
            "background" => roles.background.as_ref(),
            _ => None,
        };
        match referenced {
            Some(rv) => out.extend(resolve_chain(rv, roles, models, chains, depth + 1)?),
            None => return Err(format!("'{name}' is not a model or role")),
        }
    }
    if out.is_empty() {
        return Err("empty role chain".into());
    }
    Ok(out)
}

/// Expand `${VAR}` references from the environment so secrets never need to
/// live in the config file. Missing variables are a startup error. YAML
/// comments are left untouched so documented examples don't need to exist.
fn expand_env(input: &str) -> Result<String, String> {
    fn comment_start(line: &str) -> usize {
        let (mut in_single, mut in_double) = (false, false);
        for (i, c) in line.char_indices() {
            match c {
                '\'' if !in_double => in_single = !in_single,
                '"' if !in_single => in_double = !in_double,
                '#' if !in_single && !in_double => return i,
                _ => {}
            }
        }
        line.len()
    }
    let mut out = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        let split = comment_start(line);
        let (code, comment) = line.split_at(split);
        let mut rest = code;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find('}')
                .ok_or("config contains an unclosed ${...} reference")?;
            let name = &after[..end];
            let val = std::env::var(name).map_err(|_| {
                format!("config references ${{{name}}} but that environment variable is not set")
            })?;
            out.push_str(&val);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out.push_str(comment);
    }
    Ok(out)
}

pub fn load(yaml: &str) -> Result<Config, String> {
    let yaml = expand_env(yaml)?;
    let file: FileConfig =
        serde_yaml::from_str(&yaml).map_err(|e| format!("config parse error: {e}"))?;
    file.compile()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
server:
  token: local-dev-token
providers:
  azure: { resource: my-foundry }
models:
  gpt-55: azure/gpt-5.5
  kimi-26: azure/kimi-k2.6
  kimi-27:
    model: openrouter/moonshotai/kimi-k2.7-code
    display_name: "Kimi K2.7 Code"
  local-only:
    model: openrouter/qwen3-coder
    expose: [codex]
clients:
  claude_code:
    main: [gpt-55, kimi-27]
    subagent: kimi-26
    background: kimi-26
  codex:
    main: kimi-27
    unknown: reject
"#;

    #[test]
    fn compiles_role_based_config() {
        let cfg = load(EXAMPLE).unwrap();
        assert_eq!(cfg.auth.tokens, vec!["local-dev-token"]);

        // role alias with fallback chain
        let main = &cfg.models["claude-main"];
        assert_eq!(main.routes.len(), 2);
        assert_eq!(main.routes[0].model, "gpt-5.5");
        assert_eq!(
            main.routes[0].base_url,
            "https://my-foundry.services.ai.azure.com/openai/v1"
        );
        assert_eq!(
            main.routes[0].api_key_env.as_deref(),
            Some("AZURE_AI_API_KEY")
        );
        assert_eq!(main.routes[1].model, "moonshotai/kimi-k2.7-code");
        assert_eq!(main.routes[1].base_url, "https://openrouter.ai/api/v1");
        assert_eq!(main.compatibility.claude_code, CompatLevel::Tools);
        assert_eq!(main.compatibility.codex, CompatLevel::Blocked);

        // quirk table: kimi gets fixed-sampling drops + vision + thinking
        let kimi_route = &cfg.models["kimi-27"].routes[0];
        assert_eq!(kimi_route.drop_params, vec!["temperature", "top_p"]);
        assert!(kimi_route.capabilities.images);
        assert_eq!(kimi_route.capabilities.thinking, FeatureCap::Native);
        assert_eq!(kimi_route.timeout_ms, 300_000);
        // gpt: images but no drops
        let gpt_route = &cfg.models["gpt-55"].routes[0];
        assert!(gpt_route.capabilities.images);
        assert!(gpt_route.drop_params.is_empty());

        // one bare alias per name, shared by both clients
        assert_eq!(cfg.models["kimi-27"].compatibility.codex, CompatLevel::Full);
        assert_eq!(
            cfg.models["kimi-27"].compatibility.claude_code,
            CompatLevel::Tools
        );
        // role aliases exist per client but stay internal
        assert_eq!(
            cfg.models["main"].compatibility.claude_code,
            CompatLevel::Blocked
        );

        // expose filtering: local-only blocked for claude_code
        assert_eq!(
            cfg.models["local-only"].compatibility.claude_code,
            CompatLevel::Blocked
        );
        assert_eq!(
            cfg.models["local-only"].compatibility.codex,
            CompatLevel::Full
        );

        // all role aliases are internal (hidden); only named models are listed
        assert!(cfg.models["claude-main"].hidden);
        assert!(cfg.models["claude-subagent"].hidden);
        assert!(cfg.models["claude-background"].hidden);
        assert!(!cfg.models["kimi-27"].hidden);
        assert_eq!(
            cfg.models["claude-main"].display_name.as_deref(),
            Some("GPT 5.5 → Kimi K2.7 Code")
        );

        // unknown-id routing: opus -> subagent, haiku -> background, claude-* -> main;
        // codex has unknown: reject so no "*" pattern
        let pats: Vec<(&str, &str)> = cfg
            .model_map
            .iter()
            .map(|e| (e.pattern.as_str(), e.model.as_str()))
            .collect();
        assert_eq!(
            pats,
            vec![
                ("claude-opus-*", "claude-subagent"),
                ("claude-haiku-*", "claude-background"),
                ("claude-*", "claude-main"),
            ]
        );
    }

    #[test]
    fn named_chains_are_selectable_models() {
        let yaml = r#"
server: { token: t }
models:
  kimi: [openrouter/moonshotai/kimi-k2.7-code, openrouter/moonshotai/kimi-k2.6]
clients:
  claude_code:
    main: kimi
    subagent: kimi
  codex:
    main: kimi
"#;
        let cfg = load(yaml).unwrap();
        // the bare name is the canonical identity for BOTH clients
        let kimi = &cfg.models["kimi"];
        assert!(!kimi.hidden);
        assert_eq!(kimi.routes.len(), 2);
        assert_eq!(
            kimi.display_name.as_deref(),
            Some("Kimi K2.7 Code → Kimi K2.6")
        );
        assert_eq!(kimi.compatibility.claude_code, CompatLevel::Tools);
        assert_eq!(kimi.compatibility.codex, CompatLevel::Full);
        // discovery twin exists for Claude Code's picker, blocked for codex
        let twin = &cfg.models["claude-kimi"];
        assert_eq!(twin.routes.len(), 2);
        assert_eq!(twin.compatibility.codex, CompatLevel::Blocked);
        // roles exist but are internal
        assert!(cfg.models["claude-main"].hidden);
        assert_eq!(cfg.models["claude-main"].routes.len(), 2);
        // inline chain elements are not exposed as their own aliases
        assert!(!cfg.models.keys().any(|k| k.contains('/')));
    }

    #[test]
    fn long_form_model_accepts_a_chain() {
        let yaml = r#"
server: { token: t }
providers:
  vllm-local: { base_url: "http://localhost:8000/v1", api_key: dummy }
models:
  qwen:
    model: [vllm-local/qwen3-coder, openrouter/qwen/qwen3-coder]
    display_name: "Qwen3 Coder (local + cloud fallback)"
    images: false
    timeout_ms: 60000
clients:
  claude_code: { main: qwen }
"#;
        let cfg = load(yaml).unwrap();
        let qwen = &cfg.models["qwen"];
        assert!(!qwen.hidden);
        assert_eq!(
            qwen.display_name.as_deref(),
            Some("Qwen3 Coder (local + cloud fallback)")
        );
        assert_eq!(qwen.routes.len(), 2);
        assert_eq!(qwen.routes[0].base_url, "http://localhost:8000/v1");
        assert_eq!(qwen.routes[1].base_url, "https://openrouter.ai/api/v1");
        // overrides apply to every element of the chain
        for r in &qwen.routes {
            assert!(!r.capabilities.images);
            assert_eq!(r.timeout_ms, 60_000);
        }
        // chain elements are anonymous, not selectable models
        assert!(!cfg.models.contains_key("qwen[0]"));
        // main role follows the chain
        assert_eq!(cfg.models["claude-main"].routes.len(), 2);
    }

    #[test]
    fn chain_element_errors_are_clear() {
        let yaml = r#"
server: { token: t }
models:
  stack: [openrouter/x, missing-name]
clients: { codex: { main: stack } }
"#;
        assert!(load(yaml)
            .unwrap_err()
            .contains("'missing-name' is not a model"));
    }

    #[test]
    fn inline_model_strings_in_roles() {
        let yaml = r#"
server: { token: t }
clients:
  claude_code:
    main: [openrouter/moonshotai/kimi-k2.7-code, openrouter/moonshotai/kimi-k2.6]
    subagent: main
  codex:
    main: groq/llama-4-maverick
"#;
        let cfg = load(yaml).unwrap();
        let main = &cfg.models["claude-main"];
        assert_eq!(main.routes.len(), 2);
        assert_eq!(main.routes[0].model, "moonshotai/kimi-k2.7-code");
        // quirk table still applies to inline models
        assert_eq!(main.routes[0].drop_params, vec!["temperature", "top_p"]);
        assert_eq!(
            cfg.models["main"].routes[0].base_url,
            "https://api.groq.com/openai/v1"
        );
        // inline models are not directly selectable aliases
        assert!(!cfg.models.keys().any(|k| k.contains('/')));
    }

    #[test]
    fn env_interpolation() {
        std::env::set_var("GW_TEST_TOKEN", "secret-from-env");
        let yaml = r#"
server: { token: "${GW_TEST_TOKEN}" }
models: { kimi: openrouter/moonshotai/kimi-k2.7-code }
clients: { claude_code: { main: kimi } }
"#;
        let cfg = load(yaml).unwrap();
        assert_eq!(cfg.auth.tokens, vec!["secret-from-env"]);

        let missing = r#"
server: { token: "${GW_TEST_MISSING_VAR}" }
models: { kimi: openrouter/x }
clients: { claude_code: { main: kimi } }
"#;
        assert!(load(missing).unwrap_err().contains("GW_TEST_MISSING_VAR"));

        // commented-out ${...} examples must not be expanded
        let commented = r#"
server:
  token: t   # or: token: "${GW_TEST_MISSING_VAR}"
# token: "${GW_ALSO_MISSING}"
models: { kimi: openrouter/x }
clients: { claude_code: { main: kimi } }
"#;
        assert!(load(commented).is_ok());
    }

    #[test]
    fn provider_presets_resolve() {
        for (name, base, env) in [
            (
                "fireworks",
                "https://api.fireworks.ai/inference/v1",
                "FIREWORKS_API_KEY",
            ),
            (
                "together",
                "https://api.together.xyz/v1",
                "TOGETHER_API_KEY",
            ),
            ("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
            ("ollama", "http://localhost:11434/v1", "OLLAMA_API_KEY"),
        ] {
            let p = resolve_provider(name, None).unwrap();
            assert_eq!(p.base_url, base, "{name}");
            assert_eq!(p.api_key_env.as_deref(), Some(env), "{name}");
            assert_eq!(p.kind, ProviderKind::OpenaiCompatible);
        }
    }

    #[test]
    fn role_can_reference_another_role() {
        let yaml = r#"
server: { token: t }
models:
  kimi: openrouter/moonshotai/kimi-k2.7-code
clients:
  claude_code:
    main: kimi
    subagent: main
"#;
        let cfg = load(yaml).unwrap();
        assert_eq!(
            cfg.models["claude-subagent"].routes[0].model,
            "moonshotai/kimi-k2.7-code"
        );
    }

    #[test]
    fn helpful_errors() {
        let bad_provider = "models: { m: nowhere/x }\nclients: { codex: { main: m } }";
        assert!(load(bad_provider)
            .unwrap_err()
            .contains("not a known preset"));

        let bad_ref = r#"
models: { kimi: openrouter/k }
clients: { codex: { main: kimmy } }
"#;
        assert!(load(bad_ref)
            .unwrap_err()
            .contains("'kimmy' is not a model or role"));

        let no_clients = "models: { m: openrouter/x }\nclients: {}";
        assert!(load(no_clients).unwrap_err().contains("at least one"));

        let collision = r#"
models: { main: openrouter/x }
clients: { codex: { main: main } }
"#;
        assert!(load(collision)
            .unwrap_err()
            .contains("collides with a role name"));
    }

    #[test]
    fn minimal_two_line_config() {
        let yaml = r#"
server: { token: t }
models: { kimi: openrouter/moonshotai/kimi-k2.7-code }
clients: { claude_code: { main: kimi } }
"#;
        let cfg = load(yaml).unwrap();
        assert!(cfg.models.contains_key("claude-main"));
        assert_eq!(cfg.model_map.last().unwrap().pattern, "claude-*");
    }
}
