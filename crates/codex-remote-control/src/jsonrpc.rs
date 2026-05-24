use alleycat_codex_proto::jsonrpc::{JsonRpcError, RequestId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Request {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub method: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub(crate) fn new(id: i64, method: &'static str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id: RequestId::Integer(id),
            method,
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Notification {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub(crate) fn new(method: &'static str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Response {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl Response {
    pub(crate) fn result(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub(crate) fn error(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InboundRequest {
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InboundNotification {
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InboundResponse {
    pub id: RequestId,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone)]
pub(crate) enum InboundMessage {
    Request(InboundRequest),
    Notification(InboundNotification),
    Response(InboundResponse),
}

impl InboundMessage {
    pub(crate) fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let has_id = value.get("id").is_some();
        let has_method = value.get("method").is_some();
        match (has_id, has_method) {
            (true, true) => Ok(Self::Request(serde_json::from_value(value)?)),
            (true, false) => Ok(Self::Response(serde_json::from_value(value)?)),
            (false, true) => Ok(Self::Notification(serde_json::from_value(value)?)),
            (false, false) => Err(serde::de::Error::custom(
                "json-rpc frame missing both id and method",
            )),
        }
    }
}

pub(crate) fn redacted_json(value: &Value) -> String {
    fn redact(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        let lower = key.to_ascii_lowercase();
                        if lower.contains("token")
                            || lower.contains("secret")
                            || lower.contains("authorization")
                        {
                            (key.clone(), Value::String("<redacted>".to_string()))
                        } else {
                            (key.clone(), redact(value))
                        }
                    })
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.iter().map(redact).collect()),
            other => other.clone(),
        }
    }

    serde_json::to_string(&redact(value)).unwrap_or_else(|_| "<unserializable>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_token_like_fields_recursively() {
        let rendered = redacted_json(&json!({
            "accessToken": "secret-token",
            "nested": {"authorization": "Bearer secret", "ok": true},
            "items": [{"secretKey": "abc"}]
        }));

        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("Bearer secret"));
        assert!(!rendered.contains("abc"));
    }
}
