use super::{
    Request,
    Route,
};

pub trait ClientPrelude: Clone + Send + 'static {
    const BASE_URI: &'static str;

    /// Returns the base URI for this client, including any path prefix.
    fn base_uri(&self) -> &str {
        Self::BASE_URI
    }

    /// Returns headers for the request URL's path.
    fn headers(&self, path: &str) -> Option<reqwest::header::HeaderMap>;

    fn client(&self) -> reqwest::Client;

    fn route<T>(&self, kind: T) -> Route<Self, T>
    where
        T: Request,
    {
        Route::new(self, kind)
    }
}
