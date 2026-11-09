//! 可用模型查询数据模型
//!
//! 包含 ListAvailableModels API 的响应类型定义。
//!
//! 上游接口：`GET https://q.{api_region}.amazonaws.com/ListAvailableModels?origin=AI_EDITOR`
//! 返回该凭据（按订阅等级）当前真实可用的模型列表。

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// ListAvailableModels API 响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAvailableModelsResponse {
    /// 可用模型列表
    #[serde(default)]
    pub models: Vec<UpstreamModel>,
}

/// 单个上游模型
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamModel {
    /// 模型 ID（如 "claude-opus-5.5"）
    pub model_id: String,

    /// 模型展示名（可能不存在）
    #[serde(default)]
    pub model_name: Option<String>,

    /// 模型描述（可能不存在）
    #[serde(default)]
    pub description: Option<String>,

    /// Token 限额信息（可能不存在）
    #[serde(default)]
    pub token_limits: Option<TokenLimits>,

    /// 上游为该模型声明的 `additionalModelRequestFields` JSON Schema。
    ///
    /// 外层 `Option` 区分「键不存在」与「键存在但值为 null」：
    /// - `None`：上游没提供信息（silent），可按已知模型族兜底
    /// - `Some(None)`：上游明确表示该模型不接受附加字段（denied）
    /// - `Some(Some(schema))`：按 schema 的 properties 使用
    ///
    /// 必须用自定义 deserializer：serde 对 `Option<Option<T>>` 会把 null 和
    /// 键缺失都折叠成 `None`，正好丢掉这里唯一需要的区别。
    #[serde(default, deserialize_with = "deserialize_present_schema")]
    pub additional_model_request_fields_schema: Option<Option<Value>>,
}

/// 键存在时一定返回 `Some(...)`；值为 null 返回 `Some(None)`。
/// 键缺失由 `#[serde(default)]` 填 `None`，不会进入这里。
fn deserialize_present_schema<'de, D>(
    deserializer: D,
) -> Result<Option<Option<Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(Option::<Value>::deserialize(deserializer)?))
}

/// 模型 Token 限额
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimits {
    /// 最大输入 Token 数
    #[serde(default)]
    pub max_input_tokens: Option<i64>,

    /// 最大输出 Token 数。部分上游响应不提供该字段。
    #[serde(default)]
    pub max_output_tokens: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_full_response() {
        let json = r#"{
            "models": [
                {
                    "modelId": "claude-sonnet-4.5",
                    "modelName": "Claude Sonnet 4.5",
                    "description": "balanced model",
                    "tokenLimits": {
                        "maxInputTokens": 200000,
                        "maxOutputTokens": 64000
                    }
                },
                {
                    "modelId": "claude-opus-4.6"
                }
            ]
        }"#;
        let resp: ListAvailableModelsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.models.len(), 2);

        let first = &resp.models[0];
        assert_eq!(first.model_id, "claude-sonnet-4.5");
        assert_eq!(first.model_name.as_deref(), Some("Claude Sonnet 4.5"));
        assert_eq!(
            first.token_limits.as_ref().unwrap().max_input_tokens,
            Some(200000)
        );
        assert_eq!(
            first.token_limits.as_ref().unwrap().max_output_tokens,
            Some(64000)
        );

        // 仅 modelId 的最小对象：其余字段缺省为 None
        let second = &resp.models[1];
        assert_eq!(second.model_id, "claude-opus-4.6");
        assert!(second.model_name.is_none());
        assert!(second.token_limits.is_none());
        assert!(
            second.additional_model_request_fields_schema.is_none(),
            "键缺失必须保持 None，供上层按模型族兜底"
        );
    }

    #[test]
    fn test_additional_model_request_fields_schema_distinguishes_null_from_absent() {
        let json = r#"{
            "models": [
                {
                    "modelId": "claude-opus-5.5",
                    "additionalModelRequestFieldsSchema": {
                        "properties": {
                            "output_config": {
                                "properties": { "effort": { "enum": ["low", "high"] } }
                            }
                        }
                    }
                },
                {
                    "modelId": "claude-sonnet-4.5",
                    "additionalModelRequestFieldsSchema": null
                },
                {
                    "modelId": "glm-5"
                }
            ]
        }"#;
        let resp: ListAvailableModelsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.models[0]
                .additional_model_request_fields_schema
                .as_ref()
                .and_then(|schema| schema.as_ref())
                .and_then(|schema| schema.pointer("/properties/output_config"))
                .is_some(),
            true
        );
        assert_eq!(
            resp.models[1].additional_model_request_fields_schema,
            Some(None),
            "显式 null 必须与键缺失区分开"
        );
        assert_eq!(resp.models[2].additional_model_request_fields_schema, None);
    }

    #[test]
    fn test_deserialize_empty_models() {
        let resp: ListAvailableModelsResponse =
            serde_json::from_str(r#"{"models":[]}"#).unwrap();
        assert!(resp.models.is_empty());
    }

    #[test]
    fn test_deserialize_missing_models_field() {
        // 缺少 models 字段时回退为空数组
        let resp: ListAvailableModelsResponse = serde_json::from_str(r#"{}"#).unwrap();
        assert!(resp.models.is_empty());
    }
}
