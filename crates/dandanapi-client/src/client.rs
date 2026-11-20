use std::{
    fmt,
    sync::{
        Arc,
        LazyLock,
        OnceLock,
    },
};

use dandanapi::{
    ClientPrelude,
    Request,
    Route,
};

use crate::{
    DandanapiError,
    secret::RequestHeaderGenerator,
};

static HEADER_GENERATOR: OnceLock<RequestHeaderGenerator> = OnceLock::new();

#[derive(Clone, Default)]
pub struct DanDanClient {
    client: reqwest::Client,
    custom: Option<Arc<CustomEndpoint>>,
}

struct CustomEndpoint {
    base_url: reqwest::Url,
    signer: Option<RequestHeaderGenerator>,
}

impl fmt::Debug for DanDanClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DanDanClient")
            .field("client", &self.client)
            .field("base_url", &self.base_uri())
            .finish_non_exhaustive()
    }
}

impl DanDanClient {
    pub fn init(x_appid: String, secret: String) -> crate::Result<()> {
        let request_header_generator = RequestHeaderGenerator::new(x_appid, secret)?;
        HEADER_GENERATOR
            .set(request_header_generator)
            .map_err(|_| DandanapiError::SecretGenerationError("Already initialized".into()))
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            custom: None,
        }
    }

    /// Returns a client for `base_url` that shares this client's HTTP transport.
    /// The URL may include a path prefix. Pass `Some((app_id, secret))` to sign
    /// requests for this endpoint, or `None` for unsigned requests.
    pub fn with_base_url(
        &self, base_url: &str, credentials: Option<(&str, &str)>,
    ) -> crate::Result<Self> {
        let base_url =
            reqwest::Url::parse(base_url.trim()).map_err(|_| DandanapiError::InvalidBaseUrl)?;
        if !matches!(base_url.scheme(), "http" | "https")
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(DandanapiError::InvalidBaseUrl);
        }

        let signer = credentials
            .map(|(app_id, secret)| {
                let (app_id, secret) = (app_id.trim(), secret.trim());
                if app_id.is_empty() || secret.is_empty() {
                    return Err(DandanapiError::SecretGenerationError(
                        "Custom credentials require a non-empty app ID and secret".into(),
                    ));
                }
                RequestHeaderGenerator::new(app_id.into(), secret.into())
            })
            .transpose()?;

        Ok(Self {
            client: self.client.clone(),
            custom: Some(Arc::new(CustomEndpoint { base_url, signer })),
        })
    }

    pub fn instance() -> Self {
        static INSTANCE: LazyLock<DanDanClient> = LazyLock::new(DanDanClient::default);
        INSTANCE.clone()
    }

    pub fn route<T>(&self, kind: T) -> Route<Self, T>
    where
        T: Request,
    {
        ClientPrelude::route(self, kind)
    }
}

impl ClientPrelude for DanDanClient {
    const BASE_URI: &'static str = "https://api.dandanplay.net";

    fn base_uri(&self) -> &str {
        self.custom
            .as_ref()
            .map_or(Self::BASE_URI, |endpoint| endpoint.base_url.as_str())
    }

    fn headers(&self, path: &str) -> Option<reqwest::header::HeaderMap> {
        let signer = match &self.custom {
            Some(endpoint) => endpoint.signer.as_ref(),
            None => HEADER_GENERATOR.get(),
        };
        signer.map(|generator| generator.header(path))
    }

    fn client(&self) -> reqwest::Client {
        self.client.clone()
    }
}
