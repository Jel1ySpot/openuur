use std::fmt;

/// Errors returned by openuur's reusable library code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("not logged in; run `openuur login` first")]
    NotLoggedIn,

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API returned HTTP {status}: {message}")]
    Api {
        status: u16,
        code: Option<i64>,
        message: String,
        body: String,
    },

    #[error("invalid protocol data: {0}")]
    Protocol(String),

    #[error("terminal session error: {0}")]
    Session(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("token expired or rejected: {0}")]
    Unauthorized(String),

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("ambiguous device name {name:?}: {matches}")]
    AmbiguousDevice { name: String, matches: String },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn api(status: u16, body: impl Into<String>) -> Self {
        let body = body.into();
        let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
        let code = parsed
            .as_ref()
            .and_then(|v| v.get("code").or_else(|| v.get("error_code")))
            .and_then(|v| v.as_i64());
        let message = parsed
            .as_ref()
            .and_then(|v| {
                v.get("message")
                    .or_else(|| v.get("msg"))
                    .and_then(|x| x.as_str())
                    .or_else(|| {
                        v.get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|x| x.as_str())
                    })
                    .or_else(|| v.get("error").and_then(|e| e.as_str()))
            })
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                if body.is_empty() {
                    format!("HTTP {status}")
                } else {
                    body.chars().take(240).collect()
                }
            });

        Self::Api {
            status,
            code,
            message,
            body,
        }
    }

    pub fn is_method_not_allowed(&self) -> bool {
        matches!(self, Self::Api { status, .. } if matches!(status, 404 | 405 | 501))
    }

    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status, .. } if matches!(status, 401 | 403))
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(value: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(value.to_string())
    }
}

impl From<anyhow::Error> for Error {
    fn from(value: anyhow::Error) -> Self {
        Self::Session(value.to_string())
    }
}

impl From<Error> for std::io::Error {
    fn from(value: Error) -> Self {
        std::io::Error::other(value.to_string())
    }
}

/// A compact error formatter for the CLI.
pub struct DisplayError<'a>(pub &'a Error);

impl fmt::Display for DisplayError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
