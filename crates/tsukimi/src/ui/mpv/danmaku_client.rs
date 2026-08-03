use std::{
    sync::{
        LazyLock,
        RwLock,
    },
    time::{
        Duration,
        Instant,
    },
};

use base64::prelude::*;
use dandanapi_client::*;
use mutsumi::Danmaku;
use reqwest::header::{
    HeaderMap,
    HeaderValue,
};
use serde::{
    Deserialize,
    de::DeserializeOwned,
};
use sha2::{
    Digest,
    Sha256,
};

use crate::{
    client::{
        DEFAULT_DANMAKU_APP_ID,
        DEFAULT_DANMAKU_SERVER_URL,
        normalize_danmaku_server_url,
        uses_builtin_danmaku_credentials,
        validate_danmaku_credentials,
    },
    ui::mpv::danmaku::DanmakuExt,
};

const KEY: Option<&str> = option_env!("DANDANAPI_SECRET_KEY");
const CIPHERTEXT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/data/dandanapi-secret.age"
));
const ANIME_DETAIL_SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const DANMAKU_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DANMAKU_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
enum DanmakuCredentials {
    BuiltIn,
    Anonymous,
    Custom { app_id: String, app_secret: String },
}

pub struct EpisodeSearchResult {
    pub episode_match: Option<(i64, String)>,
    pub anime_count: usize,
    pub episode_count: usize,
}

pub struct DanmakuLoadResult {
    pub danmaku: Vec<Danmaku>,
    pub raw_count: usize,
    pub fetch_elapsed: Duration,
    pub convert_elapsed: Duration,
}

// Several self-hosted, DanDanPlay-compatible servers return localized values such as `日番`
// in the `type` field. Decode only the search fields used by the UI so those labels and other
// nonessential schema extensions cannot invalidate an otherwise usable response.
#[derive(Debug, Deserialize)]
struct CompatibleSearchResponse<T> {
    #[serde(rename = "errorCode")]
    error_code: Option<i32>,
    success: Option<bool>,
    #[serde(rename = "errorMessage")]
    error_message: Option<String>,
    animes: Option<Vec<T>>,
}

impl<T> CompatibleSearchResponse<T> {
    fn into_animes(self) -> anyhow::Result<Vec<T>> {
        if self.success == Some(false) || self.error_code.is_some_and(|code| code != 0) {
            let message = self
                .error_message
                .filter(|message| !message.trim().is_empty())
                .unwrap_or_else(|| "Danmaku API search failed".to_string());
            if let Some(code) = self.error_code {
                anyhow::bail!("{message} (error code {code})");
            }
            anyhow::bail!(message);
        }

        Ok(self.animes.unwrap_or_default())
    }
}

#[derive(Debug, Deserialize)]
struct CompatibleSearchAnimeDetails {
    #[serde(rename = "animeId")]
    anime_id: Option<i64>,
    #[serde(rename = "bangumiId")]
    bangumi_id: Option<String>,
    #[serde(rename = "animeTitle")]
    anime_title: Option<String>,
    #[serde(rename = "type")]
    type_label: Option<serde_json::Value>,
    #[serde(rename = "typeDescription")]
    type_description: Option<String>,
    #[serde(rename = "imageUrl")]
    image_url: Option<String>,
}

impl From<CompatibleSearchAnimeDetails> for SearchAnimeDetails {
    fn from(anime: CompatibleSearchAnimeDetails) -> Self {
        let anime_type = compatible_anime_type(anime.type_label.as_ref());
        Self {
            anime_id: anime.anime_id,
            bangumi_id: anime.bangumi_id,
            anime_title: anime.anime_title,
            r#type: anime_type,
            type_description: compatible_type_description(anime.type_description, anime.type_label),
            image_url: anime.image_url,
            start_date: None,
            episode_count: None,
            rating: None,
            is_favorited: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CompatibleSearchEpisodesAnime {
    #[serde(rename = "animeId")]
    anime_id: Option<i64>,
    #[serde(rename = "animeTitle")]
    anime_title: Option<String>,
    #[serde(rename = "type")]
    type_label: Option<serde_json::Value>,
    #[serde(rename = "typeDescription")]
    type_description: Option<String>,
    episodes: Option<Vec<CompatibleSearchEpisodeDetails>>,
}

impl From<CompatibleSearchEpisodesAnime> for SearchEpisodesAnime {
    fn from(anime: CompatibleSearchEpisodesAnime) -> Self {
        let anime_type = compatible_anime_type(anime.type_label.as_ref());
        Self {
            anime_id: anime.anime_id,
            anime_title: anime.anime_title,
            r#type: anime_type,
            type_description: compatible_type_description(anime.type_description, anime.type_label),
            episodes: anime
                .episodes
                .map(|episodes| episodes.into_iter().map(Into::into).collect()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CompatibleSearchEpisodeDetails {
    #[serde(rename = "episodeId")]
    episode_id: Option<i64>,
    #[serde(rename = "episodeTitle")]
    episode_title: Option<String>,
}

impl From<CompatibleSearchEpisodeDetails> for SearchEpisodeDetails {
    fn from(episode: CompatibleSearchEpisodeDetails) -> Self {
        Self {
            episode_id: episode.episode_id,
            episode_title: episode.episode_title,
        }
    }
}

fn compatible_type_description(
    type_description: Option<String>, type_label: Option<serde_json::Value>,
) -> Option<String> {
    type_description.or_else(|| {
        type_label
            .and_then(|value| value.as_str().map(str::to_string))
            .filter(|value| !value.trim().is_empty())
    })
}

fn compatible_anime_type(type_label: Option<&serde_json::Value>) -> Option<AnimeType> {
    serde_json::from_value(type_label?.clone()).ok()
}

#[derive(Clone)]
struct DanmakuClientConfig {
    base_url: String,
    credentials: DanmakuCredentials,
}

impl Default for DanmakuClientConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_DANMAKU_SERVER_URL.to_string(),
            credentials: DanmakuCredentials::BuiltIn,
        }
    }
}

impl DanmakuClientConfig {
    fn try_new(app_id: &str, app_secret: &str, base_url: Option<&str>) -> anyhow::Result<Self> {
        validate_danmaku_credentials(app_id, app_secret).map_err(anyhow::Error::msg)?;
        let is_custom_server = base_url.is_some();
        let credentials = if uses_builtin_danmaku_credentials(app_id, app_secret) {
            if is_custom_server {
                DanmakuCredentials::Anonymous
            } else {
                DanmakuCredentials::BuiltIn
            }
        } else {
            let app_id = app_id.trim().to_string();
            HeaderValue::from_str(&app_id)
                .map_err(|_| anyhow::anyhow!("App ID contains invalid header characters."))?;
            DanmakuCredentials::Custom {
                app_id,
                app_secret: app_secret.trim().to_string(),
            }
        };
        let base_url = normalize_danmaku_server_url(base_url.unwrap_or(DEFAULT_DANMAKU_SERVER_URL))
            .map_err(anyhow::Error::msg)?;

        Ok(Self {
            base_url,
            credentials,
        })
    }
}

static CONFIG: LazyLock<RwLock<DanmakuClientConfig>> =
    LazyLock::new(|| RwLock::new(DanmakuClientConfig::default()));

#[derive(Clone)]
struct RequestSigner {
    app_id: String,
    app_id_header: HeaderValue,
    app_secret: String,
}

impl RequestSigner {
    fn try_new(app_id: String, app_secret: String) -> anyhow::Result<Self> {
        let app_id_header = HeaderValue::from_str(&app_id)
            .map_err(|_| anyhow::anyhow!("App ID contains invalid header characters."))?;
        Ok(Self {
            app_id,
            app_id_header,
            app_secret,
        })
    }

    fn headers(&self, path: &str) -> HeaderMap {
        self.headers_at(path, chrono::Utc::now().timestamp())
    }

    fn headers_at(&self, path: &str, timestamp: i64) -> HeaderMap {
        let signature = BASE64_STANDARD.encode(Sha256::digest(
            format!("{}{timestamp}{path}{}", self.app_id, self.app_secret).as_bytes(),
        ));
        let mut headers = HeaderMap::new();
        headers.insert("x-appid", self.app_id_header.clone());
        headers.insert(
            "x-timestamp",
            HeaderValue::from_str(&timestamp.to_string()).expect("Timestamps are valid headers"),
        );
        headers.insert(
            "x-signature",
            HeaderValue::from_str(&signature).expect("Base64 signatures are valid headers"),
        );
        headers
    }
}

#[derive(Clone)]
pub struct DanmakuClient {
    client: reqwest::Client,
    base_url: String,
    signer: Option<RequestSigner>,
}

impl DanmakuClient {
    // dandanapi-client 0.1 fixes BASE_URI at compile time and initializes its signer through a
    // OnceLock. Keep its generated Request types, but dispatch here so source changes are safe.
    pub fn configure(app_id: &str, app_secret: &str, base_url: Option<&str>) -> anyhow::Result<()> {
        let config = DanmakuClientConfig::try_new(app_id, app_secret, base_url)?;
        *CONFIG
            .write()
            .map_err(|_| anyhow::anyhow!("Danmaku configuration lock is poisoned"))? = config;
        Ok(())
    }

    pub fn new() -> anyhow::Result<Self> {
        let config = CONFIG
            .read()
            .map_err(|_| anyhow::anyhow!("Danmaku configuration lock is poisoned"))?
            .clone();
        let signer = match config.credentials {
            DanmakuCredentials::BuiltIn => {
                let key = KEY
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("DANDANAPI_SECRET_KEY is not configured"))?;
                let app_secret = SecretGenerator::new(CIPHERTEXT.to_vec(), key.to_string())
                    .generate_plaintext()
                    .ok_or_else(|| anyhow::anyhow!("Failed to decrypt the built-in App Secret"))?;
                Some(RequestSigner::try_new(
                    DEFAULT_DANMAKU_APP_ID.to_string(),
                    app_secret,
                )?)
            }
            DanmakuCredentials::Anonymous => None,
            DanmakuCredentials::Custom { app_id, app_secret } => {
                Some(RequestSigner::try_new(app_id, app_secret)?)
            }
        };

        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(DANMAKU_CONNECT_TIMEOUT)
                .timeout(DANMAKU_REQUEST_TIMEOUT)
                .build()?,
            base_url: config.base_url,
            signer,
        })
    }

    async fn send<T: Request>(&self, request_kind: T) -> anyhow::Result<reqwest::Response> {
        let path = request_kind.path();
        let method = reqwest::Method::from_bytes(T::METHOD.as_str().as_bytes())?;
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base_url));
        if let Some(signer) = &self.signer {
            request = request.headers(signer.headers(&path));
        }

        if let Some(body) = request_kind.body() {
            request = request.json(body);
        }
        if let Some(params) = request_kind.params() {
            request = request.query(params);
        }

        Ok(request.send().await?.error_for_status()?)
    }

    async fn request<T: Request>(&self, request_kind: T) -> anyhow::Result<T::Response> {
        Ok(self.send(request_kind).await?.json::<T::Response>().await?)
    }

    async fn request_compatible_search<T, I>(&self, request_kind: T) -> anyhow::Result<Vec<I>>
    where
        T: Request,
        I: DeserializeOwned,
    {
        self.send(request_kind)
            .await?
            .json::<CompatibleSearchResponse<I>>()
            .await?
            .into_animes()
    }
}

impl DanmakuClient {
    pub async fn search_anime_details(
        &self, keyword: String, anime_type: AnimeType,
    ) -> anyhow::Result<Vec<SearchAnimeDetails>> {
        let animes = tokio::time::timeout(
            ANIME_DETAIL_SEARCH_TIMEOUT,
            self.request_compatible_search::<_, CompatibleSearchAnimeDetails>(SearchSearchAnime {
                params: SearchSearchAnimeParams {
                    keyword,
                    r#type: anime_type,
                    v2: true,
                },
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("anime detail search timed out"))??;
        Ok(animes.into_iter().map(Into::into).collect())
    }

    pub async fn search_animes(
        &self, params: SearchSearchEpisodesParams,
    ) -> anyhow::Result<Vec<SearchEpisodesAnime>> {
        Ok(self
            .request_compatible_search::<_, CompatibleSearchEpisodesAnime>(SearchSearchEpisodes {
                params,
            })
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub async fn search_episode(
        &self, params: SearchSearchEpisodesParams,
    ) -> anyhow::Result<EpisodeSearchResult> {
        let animes = self.search_animes(params).await?;
        Ok(Self::summarize_episode_search(animes))
    }

    fn summarize_episode_search(animes: Vec<SearchEpisodesAnime>) -> EpisodeSearchResult {
        let anime_count = animes.len();
        let episode_count = animes
            .iter()
            .map(|anime| anime.episodes.as_ref().map_or(0, Vec::len))
            .sum();

        EpisodeSearchResult {
            episode_match: Self::first_episode_match(animes),
            anime_count,
            episode_count,
        }
    }

    fn first_episode_match(animes: Vec<SearchEpisodesAnime>) -> Option<(i64, String)> {
        animes.into_iter().find_map(|anime| {
            let anime_title = anime.anime_title.unwrap_or_default();
            anime
                .episodes
                .unwrap_or_default()
                .into_iter()
                .find_map(|episode| {
                    let episode_id = episode.episode_id?;
                    let episode_title = episode.episode_title.unwrap_or_default();
                    let item_name = match (episode_title.is_empty(), anime_title.is_empty()) {
                        (false, false) => format!("{episode_title} - {anime_title}"),
                        (false, true) => episode_title,
                        (true, false) => anime_title.clone(),
                        (true, true) => String::new(),
                    };
                    Some((episode_id, item_name))
                })
        })
    }

    pub async fn get_comments(&self, episode_id: i64) -> anyhow::Result<DanmakuLoadResult> {
        let fetch_started = Instant::now();
        let response = self
            .request(CommentGetComment {
                episode_id,
                params: CommentGetCommentParams {
                    from: 0,
                    with_related: true,
                    ch_convert: 0,
                },
            })
            .await?;
        let fetch_elapsed = fetch_started.elapsed();

        let comments = response.comments.unwrap_or_default();
        let raw_count = comments.len();
        let convert_started = Instant::now();
        let danmaku = comments
            .into_iter()
            .map(DanmakuExt::into_danmaku)
            .filter(|danmaku| !danmaku.content.is_empty())
            .collect::<Vec<_>>();

        Ok(DanmakuLoadResult {
            danmaku,
            raw_count,
            fetch_elapsed,
            convert_elapsed: convert_started.elapsed(),
        })
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn custom_anime_type_labels_do_not_break_title_search() {
        let response = serde_json::from_value::<
            CompatibleSearchResponse<CompatibleSearchAnimeDetails>,
        >(serde_json::json!({
            "errorCode": 0,
            "success": true,
            "animes": [{
                "animeId": 1779737556,
                "animeTitle": "Example",
                "type": "日番",
                "imageUrl": "https://example.com/poster.jpg"
            }]
        }))
        .expect("compatible anime search response");

        let anime = SearchAnimeDetails::from(
            response
                .into_animes()
                .expect("successful response")
                .pop()
                .expect("anime result"),
        );

        assert_eq!(anime.anime_id, Some(1779737556));
        assert_eq!(anime.anime_title.as_deref(), Some("Example"));
        assert_eq!(anime.type_description.as_deref(), Some("日番"));
        assert!(anime.r#type.is_none());
    }

    #[test]
    fn custom_anime_type_labels_do_not_break_episode_search() {
        let response = serde_json::from_value::<
            CompatibleSearchResponse<CompatibleSearchEpisodesAnime>,
        >(serde_json::json!({
            "errorCode": 0,
            "success": true,
            "animes": [{
                "animeId": 42,
                "animeTitle": "Example",
                "type": "动漫",
                "episodes": [{ "episodeId": 4201, "episodeTitle": "Episode 1" }]
            }]
        }))
        .expect("compatible episode search response");

        let anime = SearchEpisodesAnime::from(
            response
                .into_animes()
                .expect("successful response")
                .pop()
                .expect("anime result"),
        );

        assert_eq!(anime.type_description.as_deref(), Some("动漫"));
        let episode = anime
            .episodes
            .expect("episodes")
            .pop()
            .expect("episode result");
        assert_eq!(episode.episode_id, Some(4201));
    }

    #[test]
    fn compatible_search_preserves_standard_anime_types() {
        let response = serde_json::from_value::<
            CompatibleSearchResponse<CompatibleSearchEpisodesAnime>,
        >(serde_json::json!({
            "errorCode": 0,
            "success": true,
            "animes": [{
                "animeId": 42,
                "animeTitle": "Example",
                "type": "movie",
                "typeDescription": "Anime Film",
                "episodes": [{ "episodeId": 4201, "episodeTitle": "Movie" }]
            }]
        }))
        .expect("compatible episode search response");

        let anime = SearchEpisodesAnime::from(
            response
                .into_animes()
                .expect("successful response")
                .pop()
                .expect("anime result"),
        );

        assert!(matches!(anime.r#type, Some(AnimeType::Movie)));
        assert_eq!(anime.type_description.as_deref(), Some("Anime Film"));
    }

    #[test]
    fn compatible_search_surfaces_api_errors_and_rejects_bad_shapes() {
        let response = serde_json::from_value::<
            CompatibleSearchResponse<CompatibleSearchAnimeDetails>,
        >(serde_json::json!({
            "errorCode": 503,
            "success": false,
            "errorMessage": "upstream unavailable"
        }))
        .expect("error response shape");
        let error = response.into_animes().expect_err("API error must fail");
        assert!(error.to_string().contains("upstream unavailable"));

        let malformed = serde_json::from_value::<
            CompatibleSearchResponse<CompatibleSearchAnimeDetails>,
        >(serde_json::json!({
            "errorCode": 0,
            "success": true,
            "animes": "not-an-array"
        }));
        assert!(malformed.is_err());
    }

    #[test]
    fn episode_search_summary_counts_candidates() {
        let animes = serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([
            {
                "animeId": 1,
                "animeTitle": "First",
                "episodes": [
                    { "episodeId": 11, "episodeTitle": "Episode 1" },
                    { "episodeId": 12, "episodeTitle": "Episode 2" }
                ]
            },
            {
                "animeId": 2,
                "animeTitle": "Second",
                "episodes": [{ "episodeId": 21, "episodeTitle": "Episode 1" }]
            }
        ]))
        .expect("valid episode search response");

        let result = DanmakuClient::summarize_episode_search(animes);

        assert_eq!(result.anime_count, 2);
        assert_eq!(result.episode_count, 3);
        assert_eq!(
            result.episode_match,
            Some((11, "Episode 1 - First".to_string()))
        );
    }
}

#[cfg(test)]
mod configuration_tests {
    use super::*;

    #[test]
    fn validates_runtime_configuration() {
        let built_in = DanmakuClientConfig::try_new("", "", None).expect("built-in config");
        assert_eq!(built_in.base_url, DEFAULT_DANMAKU_SERVER_URL);
        assert!(matches!(built_in.credentials, DanmakuCredentials::BuiltIn));

        let anonymous = DanmakuClientConfig::try_new("", "", Some("https://example.com/dandan/"))
            .expect("anonymous custom-server config");
        assert!(matches!(
            anonymous.credentials,
            DanmakuCredentials::Anonymous
        ));

        let custom = DanmakuClientConfig::try_new(
            "custom-app",
            "custom-secret",
            Some("https://example.com/dandan/"),
        )
        .expect("custom config");
        assert_eq!(custom.base_url, "https://example.com/dandan");
        assert!(matches!(
            custom.credentials,
            DanmakuCredentials::Custom { .. }
        ));

        assert!(DanmakuClientConfig::try_new("custom-app", "", None).is_err());
        assert!(DanmakuClientConfig::try_new("bad\napp", "secret", None).is_err());
        assert!(
            DanmakuClientConfig::try_new("custom-app", "secret", Some("file:///tmp/api")).is_err()
        );
    }

    #[test]
    fn signs_requests_like_dandanapi_client() {
        let signer = RequestSigner::try_new("test-app".to_string(), "test-secret".to_string())
            .expect("valid signer");
        let path = "/api/v2/test";
        let timestamp = 1_700_000_000;
        let headers = signer.headers_at(path, timestamp);
        let expected = BASE64_STANDARD.encode(Sha256::digest(
            format!("test-app{timestamp}{path}test-secret").as_bytes(),
        ));

        assert_eq!(headers["x-appid"], "test-app");
        assert_eq!(headers["x-timestamp"], timestamp.to_string());
        assert_eq!(headers["x-signature"], expected);
    }
}
