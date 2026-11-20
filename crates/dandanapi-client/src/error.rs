#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("http: {0}")]
    HttpError(reqwest::Error),
    #[error("serde: {0}")]
    SerdeError(serde_json::Error),
    #[error("base URL must be an absolute HTTP(S) URL without credentials, a query, or a fragment")]
    InvalidBaseUrl,
    #[error("invalid header value: {0}")]
    InvalidHeaderValue(#[from] reqwest::header::InvalidHeaderValue),
    #[error("secret: {0}")]
    SecretGenerationError(String),
}
