use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Which client-facing protocol shape to use when serializing errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientProtocol {
    AnthropicMessages,
    OpenAiResponses,
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("{message}")]
    Capability { message: String },
    #[error("unknown model alias '{0}'")]
    UnknownModel(String),
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("authentication failed")]
    Unauthorized,
    /// Upstream error to forward as-is (status + raw body).
    #[error("upstream error {status}")]
    Upstream {
        status: u16,
        body: bytes::Bytes,
        content_type: Option<String>,
    },
    #[error("all routes failed for '{alias}': {detail}")]
    AllRoutesFailed { alias: String, detail: String },
    #[error(transparent)]
    Network(#[from] reqwest::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl GatewayError {
    pub fn capability(msg: impl Into<String>) -> Self {
        Self::Capability {
            message: msg.into(),
        }
    }

    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            Self::Capability { message } => (
                StatusCode::BAD_REQUEST,
                "gateway_capability_error",
                message.clone(),
            ),
            Self::UnknownModel(_) => (StatusCode::NOT_FOUND, "not_found_error", self.to_string()),
            Self::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                self.to_string(),
            ),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                self.to_string(),
            ),
            Self::AllRoutesFailed { .. } => (
                StatusCode::BAD_GATEWAY,
                "gateway_routing_error",
                self.to_string(),
            ),
            Self::Network(_) => (
                StatusCode::BAD_GATEWAY,
                "gateway_upstream_error",
                self.to_string(),
            ),
            Self::Upstream { .. } | Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                self.to_string(),
            ),
        }
    }

    pub fn into_response_for(self, client: ClientProtocol) -> Response {
        // Forward upstream errors byte-for-byte: clients (notably Claude Code)
        // key retry behavior off the original status and wording.
        if let Self::Upstream {
            status,
            body,
            content_type,
        } = self
        {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = content_type.unwrap_or_else(|| "application/json".into());
            return (status, [(axum::http::header::CONTENT_TYPE, ct)], body).into_response();
        }
        let (status, kind, message) = self.parts();
        let body = match client {
            ClientProtocol::AnthropicMessages => json!({
                "type": "error",
                "error": { "type": kind, "message": message }
            }),
            ClientProtocol::OpenAiResponses => json!({
                "error": { "message": message, "type": kind, "code": if kind == "gateway_capability_error" { "capability_mismatch" } else { kind } }
            }),
        };
        (status, axum::Json(body)).into_response()
    }
}
