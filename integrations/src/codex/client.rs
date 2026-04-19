use error_stack::{Report, ResultExt};
use serde::de::DeserializeOwned;

use crate::codex::gql::{GqlRequest, GqlResponse};
use crate::error::{IntegrationError, IntegrationResult};

/// Default production GraphQL endpoint published by Codex.
pub const DEFAULT_ENDPOINT: &str = "https://graph.codex.io/graphql";

/// HTTP client for the Codex GraphQL API.
pub struct CodexClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl CodexClient {
    /// Builds a client targeting [`DEFAULT_ENDPOINT`] with the given API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: DEFAULT_ENDPOINT.to_string(),
            api_key: api_key.into(),
        }
    }

    /// Overrides the GraphQL endpoint (useful for a mock server in tests or
    /// pointing at a private deployment).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// POSTs a GraphQL request and decodes its `data` payload into `T`.
    ///
    /// Errors are normalized into [`IntegrationError`] variants:
    ///
    /// * Transport failures (DNS, TLS, timeout) → [`IntegrationError::Network`].
    /// * Non-2xx HTTP responses → [`IntegrationError::Provider`].
    /// * Body that can't be parsed as JSON → [`IntegrationError::Parse`].
    /// * Valid JSON with a non-empty GraphQL `errors` array, or with
    ///   `data: null` and no errors → [`IntegrationError::Codex`] (the error
    ///   messages are attached for easy `tracing` inspection).
    pub(crate) async fn post_gql<T: DeserializeOwned>(
        &self,
        req: &GqlRequest,
    ) -> IntegrationResult<T> {
        let response = self
            .http
            .post(&self.endpoint)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(req)
            .send()
            .await
            .change_context(IntegrationError::Network)
            .attach(format!("Failed to call Codex {}", self.endpoint))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .change_context(IntegrationError::Network)
            .attach("Failed to read Codex GraphQL response body")?;

        if !status.is_success() {
            return Err(Report::new(IntegrationError::Provider)
                .attach(format!("Codex GraphQL returned HTTP {}: {}", status, body)));
        }

        let parsed: GqlResponse<T> = serde_json::from_str(&body)
            .change_context(IntegrationError::Parse)
            .attach(format!("Failed to parse Codex GraphQL response: {}", body))?;

        if !parsed.errors.is_empty() {
            // Join messages so a single `tracing::warn!("{err:?}")` gives the
            // full picture without having to walk the attachments.
            let joined = parsed
                .errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Report::new(IntegrationError::Codex)
                .attach(format!("Codex GraphQL errors: {}", joined)));
        }

        parsed.data.ok_or_else(|| {
            Report::new(IntegrationError::Codex)
                .attach("Codex GraphQL returned no data and no errors")
        })
    }
}
