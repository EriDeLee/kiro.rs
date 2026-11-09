//! 上游模型能力表。
//!
//! 每个模型的推理字段路径、档位枚举、thinking 枚举都来自
//! `ListAvailableModels` 的 `additionalModelRequestFieldsSchema`。这里把它解析成
//! 一份进程级只读快照，供 converter 在构造 `additionalModelRequestFields` 时查询。
//!
//! 设计原则：
//! - **不写死模型名**。模型池是动态的，能力表也随每次模型缓存刷新整体替换。
//! - **区分「没说话」与「明确拒绝」**。键缺失是上游没提供信息，可按模型族兜底；
//!   显式 `null` 是上游明确表示该模型不接受附加字段，不得再猜。
//! - 同一个 model ID 出现在多个凭据里时合并取并集/更完整的一份；上游窗口取最大值，
//!   避免把 1M 窗口误算成更小值而导致 input_tokens 系统性偏小。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use parking_lot::RwLock;
use serde_json::Value;

use super::policy;
use crate::kiro::model::available_models::UpstreamModel;

/// 上游对 `additionalModelRequestFieldsSchema` 的三种回答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaState {
    /// 响应里没有这个键：上游没说，允许按已知模型族兜底。
    Silent,
    /// 键存在但值是 `null` 或非对象：上游明确表示没有可用的附加字段。
    Denied,
    /// 键存在且是对象：按其中声明的 properties 使用。
    Declared,
}

/// 推理档位能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortCapability {
    /// 字段路径，例如 `output_config` 或 `reasoning`。
    pub path: String,
    /// 该模型声明的全部合法档位。
    ///
    /// `None` 表示上游声明了 `effort` 属性但**没有 `enum`**（自由字符串）：
    /// 客户端给什么就原样下发什么，不能当成「该模型没有 effort」静默丢掉。
    pub levels: Option<Vec<String>>,
    /// 上游声明的默认档位（若在 levels 内）。
    pub default: Option<String>,
}

/// thinking 能力。只有声明了对应属性的模型才会下发这些字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingCapability {
    /// `thinking.type` 的合法值，例如 `adaptive` / `disabled`。
    pub types: Vec<String>,
    /// `thinking.display` 的合法值，例如 `summarized` / `omitted`。
    pub displays: Vec<String>,
}

/// 一个模型的附加请求字段能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapability {
    pub schema: SchemaState,
    pub effort: Option<EffortCapability>,
    pub thinking: Option<ThinkingCapability>,
    pub max_input_tokens: Option<i32>,
}

#[derive(Debug, Clone)]
struct CatalogEntry {
    /// 上游返回的原始大小写 ID；匹配时返回它，保证发给上游的 ID 与清单一致。
    canonical_id: String,
    capability: ModelCapability,
}

static REGISTRY: OnceLock<RwLock<BTreeMap<String, CatalogEntry>>> = OnceLock::new();

fn registry() -> &'static RwLock<BTreeMap<String, CatalogEntry>> {
    REGISTRY.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// 用一组上游模型条目整体替换能力表。
///
/// 调用方应在每次模型缓存变化后重建完整并集（而不是只追加），这样被删除的
/// 模型不会在能力表里留下幽灵条目。
pub fn replace_from_models(models: &[UpstreamModel]) {
    let mut merged: BTreeMap<String, CatalogEntry> = BTreeMap::new();

    for model in models {
        let key = model.model_id.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let capability = capability_from_upstream(model);
        match merged.get_mut(&key) {
            Some(existing) => merge_capability(&mut existing.capability, capability),
            None => {
                merged.insert(
                    key,
                    CatalogEntry {
                        canonical_id: model.model_id.clone(),
                        capability,
                    },
                );
            }
        }
    }

    *registry().write() = merged;
}

/// 把客户端模型名解析成能力表里的真实 ID。
///
/// 支持精确匹配、`-thinking` / `-latest` / 日期后缀与点横线版本变体。
/// 找不到返回 `None`，调用方应把原始 ID 透传给上游。
pub fn canonical_id(model: &str) -> Option<String> {
    let registry = registry().read();
    for candidate in policy::candidate_ids(model) {
        if let Some(entry) = registry.get(&candidate.to_ascii_lowercase()) {
            return Some(entry.canonical_id.clone());
        }
    }
    None
}

/// 查询模型的附加字段能力。
pub fn lookup(model: &str) -> Option<ModelCapability> {
    let registry = registry().read();
    for candidate in policy::candidate_ids(model) {
        if let Some(entry) = registry.get(&candidate.to_ascii_lowercase()) {
            return Some(entry.capability.clone());
        }
    }
    None
}

/// 查询模型声明的输入窗口（`tokenLimits.maxInputTokens`）。
pub fn context_window(model: &str) -> Option<i32> {
    lookup(model).and_then(|capability| capability.max_input_tokens)
}

/// 清空能力表（仅用于测试与冷启动重置）。
#[cfg(test)]
pub fn clear() {
    registry().write().clear();
}

fn capability_from_upstream(model: &UpstreamModel) -> ModelCapability {
    let max_input_tokens = model
        .token_limits
        .as_ref()
        .and_then(|limits| limits.max_input_tokens)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value > 0);

    let schema = match &model.additional_model_request_fields_schema {
        // 键缺失：上游没说。
        None => SchemaState::Silent,
        // 键存在但显式 null：上游明确拒绝。
        Some(None) => SchemaState::Denied,
        Some(Some(value)) => {
            if value.is_object() {
                let (effort, thinking) = parse_schema(value);
                return ModelCapability {
                    schema: SchemaState::Declared,
                    effort,
                    thinking,
                    max_input_tokens,
                };
            }
            // 其它形状（字符串/数组/数字）同样视为上游已给出回答，但没有可用字段。
            SchemaState::Denied
        }
    };

    ModelCapability {
        schema,
        effort: None,
        thinking: None,
        max_input_tokens,
    }
}

fn parse_schema(schema: &Value) -> (Option<EffortCapability>, Option<ThinkingCapability>) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return (None, None);
    };
    (parse_effort(properties), parse_thinking(properties))
}

fn parse_effort(properties: &serde_json::Map<String, Value>) -> Option<EffortCapability> {
    // 实测 Kiro 只有这两条路径。先按已知顺序找，找不到再扫任意带
    // `properties.effort.enum` 的属性，避免新路径被硬编码漏掉。
    for path in ["output_config", "reasoning"] {
        if let Some(capability) = effort_from_property(properties.get(path), path) {
            return Some(capability);
        }
    }

    for (path, section) in properties {
        if path == "output_config" || path == "reasoning" {
            continue;
        }
        if let Some(capability) = effort_from_property(Some(section), path) {
            return Some(capability);
        }
    }
    None
}

fn effort_from_property(section: Option<&Value>, path: &str) -> Option<EffortCapability> {
    let effort = section?.get("properties")?.get("effort")?;
    if !effort.is_object() {
        return None;
    }
    let levels = string_enum(effort);
    let default = effort
        .get("default")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if levels.is_empty() {
        // 属性存在、但没有 enum：上游把它声明成自由字符串。仍算一条有效的
        // effort 路径，客户端显式值原样下发；当成「没有 effort」会静默丢掉档位。
        return Some(EffortCapability {
            path: path.to_string(),
            levels: None,
            default,
        });
    }
    let default = default.filter(|value| levels.iter().any(|level| level == value));
    Some(EffortCapability {
        path: path.to_string(),
        levels: Some(levels),
        default,
    })
}

fn parse_thinking(properties: &serde_json::Map<String, Value>) -> Option<ThinkingCapability> {
    let thinking = properties.get("thinking")?;
    let thinking_properties = thinking.get("properties")?;
    let types = thinking_properties
        .get("type")
        .map(string_enum)
        .unwrap_or_default();
    let displays = thinking_properties
        .get("display")
        .map(string_enum)
        .unwrap_or_default();
    if types.is_empty() && displays.is_empty() {
        return None;
    }
    Some(ThinkingCapability { types, displays })
}

fn string_enum(value: &Value) -> Vec<String> {
    let mut levels = Vec::new();
    if let Some(items) = value.get("enum").and_then(Value::as_array) {
        for item in items {
            if let Some(level) = item.as_str()
                && !level.is_empty()
                && !levels.iter().any(|existing| existing == level)
            {
                levels.push(level.to_string());
            }
        }
    }
    levels
}

fn merge_capability(existing: &mut ModelCapability, incoming: ModelCapability) {
    // Declared > Denied > Silent：同一模型在不同账号下应答不同时，先保住
    // 「上游明确说过不接受」这条边界；只有所有账号都没提供信息时才允许前缀兜底。
    if schema_rank(incoming.schema) > schema_rank(existing.schema) {
        existing.schema = incoming.schema;
        existing.effort = incoming.effort;
        existing.thinking = incoming.thinking;
    } else if incoming.schema == SchemaState::Declared {
        let existing_rank = existing.effort.as_ref().map_or(0, effort_rank);
        let incoming_rank = incoming.effort.as_ref().map_or(0, effort_rank);
        if existing_rank < incoming_rank {
            existing.effort = incoming.effort;
        }
        if existing.thinking.is_none() {
            existing.thinking = incoming.thinking;
        }
    }

    existing.max_input_tokens = match (existing.max_input_tokens, incoming.max_input_tokens) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (None, right) => right,
        (left, None) => left,
    };
}

/// 合并多个账号的同名模型时，哪一份 effort 声明更值得保留。
///
/// 自由字符串（`levels = None`）不加任何限制，按 0 计：与枚举声明混在一起时
/// 保留枚举（更严格），避免把枚举外的值发给会 400 的账号。
fn effort_rank(effort: &EffortCapability) -> usize {
    effort.levels.as_ref().map_or(0, Vec::len)
}

fn schema_rank(state: SchemaState) -> u8 {
    match state {
        SchemaState::Declared => 3,
        SchemaState::Denied => 2,
        SchemaState::Silent => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(schema: Option<Option<Value>>, max_input: Option<i64>) -> UpstreamModel {
        UpstreamModel {
            model_id: "test-model".to_string(),
            model_name: None,
            description: None,
            token_limits: max_input.map(|value| crate::kiro::model::available_models::TokenLimits {
                max_input_tokens: Some(value),
                max_output_tokens: None,
            }),
            additional_model_request_fields_schema: schema,
        }
    }

    #[test]
    fn parses_output_config_effort() {
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "output_config": {
                    "type": "object",
                    "properties": {
                        "effort": {
                            "type": "string",
                            "enum": ["low", "medium", "high", "xhigh", "max"],
                            "default": "high"
                        }
                    }
                },
                "thinking": {
                    "type": "object",
                    "properties": {
                        "type": { "enum": ["adaptive", "disabled"] },
                        "display": { "enum": ["summarized", "omitted"] }
                    }
                }
            }
        });
        let capability = capability_from_upstream(&upstream(Some(Some(schema)), Some(1_000_000)));
        assert_eq!(capability.schema, SchemaState::Declared);
        let effort = capability.effort.unwrap();
        assert_eq!(effort.path, "output_config");
        assert_eq!(
            effort.levels.unwrap(),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(effort.default.as_deref(), Some("high"));
        let thinking = capability.thinking.unwrap();
        assert_eq!(thinking.types, ["adaptive", "disabled"]);
        assert_eq!(thinking.displays, ["summarized", "omitted"]);
        assert_eq!(capability.max_input_tokens, Some(1_000_000));
    }

    #[test]
    fn parses_reasoning_effort_and_none() {
        let schema = serde_json::json!({
            "properties": {
                "reasoning": {
                    "properties": {
                        "effort": {
                            "enum": ["none", "low", "medium", "high", "xhigh", "max"],
                            "default": "high"
                        }
                    }
                }
            }
        });
        let capability = capability_from_upstream(&upstream(Some(Some(schema)), None));
        let effort = capability.effort.unwrap();
        assert_eq!(effort.path, "reasoning");
        assert!(effort.levels.unwrap().contains(&"none".to_string()));
        assert_eq!(capability.thinking, None);
    }

    #[test]
    fn explicit_null_is_a_denial_not_a_gap() {
        let capability = capability_from_upstream(&upstream(Some(None), None));
        assert_eq!(capability.schema, SchemaState::Denied);
        assert!(capability.effort.is_none() && capability.thinking.is_none());
    }

    #[test]
    fn absent_schema_is_silent() {
        let capability = capability_from_upstream(&upstream(None, None));
        assert_eq!(capability.schema, SchemaState::Silent);
        assert!(capability.effort.is_none() && capability.thinking.is_none());
    }

    #[test]
    fn declared_without_effort_still_keeps_thinking() {
        let schema = serde_json::json!({
            "properties": {
                "thinking": {
                    "properties": {
                        "type": { "enum": ["adaptive"] }
                    }
                }
            }
        });
        let capability = capability_from_upstream(&upstream(Some(Some(schema)), None));
        assert_eq!(capability.schema, SchemaState::Declared);
        assert!(capability.effort.is_none());
        assert_eq!(capability.thinking.unwrap().types, ["adaptive"]);
    }

    #[test]
    fn generic_effort_path_is_discovered() {
        let schema = serde_json::json!({
            "properties": {
                "future_config": {
                    "properties": {
                        "effort": { "enum": ["low", "high"] }
                    }
                }
            }
        });
        let capability = capability_from_upstream(&upstream(Some(Some(schema)), None));
        let effort = capability.effort.unwrap();
        assert_eq!(effort.path, "future_config");
        assert_eq!(effort.levels.unwrap(), ["low", "high"]);
    }

    /// 属性存在但没有 `enum`：上游声明的是自由字符串，不能当作「没有 effort」。
    #[test]
    fn effort_without_enum_is_free_form() {
        let schema = serde_json::json!({
            "properties": {
                "output_config": {
                    "properties": {
                        "effort": { "type": "string", "default": "high" }
                    }
                }
            }
        });
        let capability = capability_from_upstream(&upstream(Some(Some(schema)), None));
        let effort = capability.effort.unwrap();
        assert_eq!(effort.path, "output_config");
        assert_eq!(effort.levels, None);
        assert_eq!(effort.default.as_deref(), Some("high"));
    }

    #[test]
    fn merging_prefers_declared_and_larger_window() {
        let mut existing = ModelCapability {
            schema: SchemaState::Denied,
            effort: None,
            thinking: None,
            max_input_tokens: Some(100_000),
        };
        let incoming = ModelCapability {
            schema: SchemaState::Declared,
            effort: Some(EffortCapability {
                path: "reasoning".to_string(),
                levels: Some(vec!["low".to_string(), "high".to_string()]),
                default: None,
            }),
            thinking: None,
            max_input_tokens: Some(200_000),
        };
        merge_capability(&mut existing, incoming);
        assert_eq!(existing.schema, SchemaState::Declared);
        assert_eq!(existing.effort.unwrap().path, "reasoning");
        assert_eq!(existing.max_input_tokens, Some(200_000));
    }
}
