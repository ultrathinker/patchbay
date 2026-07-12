//! Minimal JSON-RPC 2.0 types for the hand-rolled MCP gateway (MASTER_PLAN D1).
//!
//! MCP is a strict subset of JSON-RPC 2.0. We only need the request/response
//! envelopes, the error object, the standard error codes, and two constructors.
//! A request with `id == None` is a *notification* (no JSON-RPC response is
//! expected; the HTTP layer still returns 202).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---- standard JSON-RPC 2.0 error codes ----------------------------------

/// Invalid JSON was received (parse error).
pub const PARSE_ERROR: i64 = -32700;
/// The JSON sent is not a valid Request object.
pub const INVALID_REQUEST: i64 = -32600;
/// The method does not exist / is not available.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Invalid method parameters.
pub const INVALID_PARAMS: i64 = -32602;
/// Reserved for implementation-defined server errors.
pub const SERVER_ERROR: i64 = -32000;

fn default_version() -> String {
    "2.0".to_string()
}

/// A JSON-RPC 2.0 request. `id == None` marks a notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default = "default_version")]
    pub jsonrpc: String,
    /// `None` => notification (no response expected at the JSON-RPC level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response. Exactly one of `result` / `error` is `Some`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Build a success response: `{ jsonrpc:"2.0", id, result }`.
pub fn success(id: Option<Value>, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    }
}

/// Build an error response: `{ jsonrpc:"2.0", id, error:{ code, message, data } }`.
pub fn error(
    id: Option<Value>,
    code: i64,
    message: impl Into<String>,
    data: Option<Value>,
) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_with_id_round_trips() {
        let raw = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
        let req: JsonRpcRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.id, Some(json!(7)));
        assert_eq!(req.method, "tools/list");
        assert!(req.params.is_none());
    }

    #[test]
    fn notification_has_no_id() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req: JsonRpcRequest = serde_json::from_str(raw).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn success_serializes_to_canonical_envelope() {
        let resp = success(Some(json!(1)), json!({ "ok": true }));
        let v: Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["ok"], true);
        assert!(v.get("error").is_none() || v["error"].is_null());
    }

    #[test]
    fn error_envelope_has_code_and_message() {
        let resp = error(Some(json!(2)), METHOD_NOT_FOUND, "nope", None);
        let v: Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(v["error"]["message"], "nope");
        assert!(v.get("result").is_none() || v["result"].is_null());
    }
}
