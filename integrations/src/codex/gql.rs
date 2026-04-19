use serde::{Deserialize, Serialize};

/// Body of a GraphQL POST request
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GqlRequest {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "operationName")]
    pub operation_name: Option<String>,
}

/// Standard GraphQL response envelope, parameterised over the concrete `data`
/// shape each endpoint decodes into.
///
/// `errors` defaults to an empty vec so a successful payload (`{"data": ...}`
/// with no `errors` key at all) deserializes cleanly.
// Derives used so endpoint modules can log raw responses while debugging.
#[derive(Debug, Deserialize)]
pub(crate) struct GqlResponse<T> {
    pub data: Option<T>,
    #[serde(default)]
    pub errors: Vec<GqlError>,
}

/// One entry of a GraphQL `errors` array.
///
/// Only `message` is surfaced today; `path` and `extensions` vary per-provider
/// and are not worth the extra type surface. Unknown JSON keys are ignored by
/// `serde` by default, so adding them later is forward-compatible.
#[derive(Debug, Deserialize)]
pub(crate) struct GqlError {
    pub message: String,
}
