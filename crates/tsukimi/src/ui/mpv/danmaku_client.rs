use std::{
    ops::Deref,
    sync::Once,
};

use dandanapi_client::*;
use mutsumi::Danmaku;

use crate::ui::{
    models::{
        SETTINGS,
        settings::DanmakuServer,
    },
    mpv::danmaku::DanmakuExt,
};

const X_APPID: &str = "e9imrhcexn";
pub(crate) const OFFICIAL_SERVER_NAME: &str = "DanDanPlay";

#[derive(Clone)]
pub struct DanmakuClient {
    client: DanDanClient,
    source: DanmakuServer,
    app_id: Option<String>,
}

impl DanmakuClient {
    pub fn instance() -> Option<Self> {
        let servers = SETTINGS.danmaku_servers();
        let server = usize::try_from(SETTINGS.danmaku_active_server())
            .ok()
            .and_then(|index| servers.get(index));
        match Self::for_source(server) {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(%error, "Invalid Danmaku source settings");
                None
            }
        }
    }

    /// Builds a client using the saved global credentials. `None` selects DanDanPlay.
    pub fn for_source(server: Option<&DanmakuServer>) -> dandanapi_client::Result<Self> {
        let (app_id, app_secret) = SETTINGS.danmaku_credentials();
        Self::with_source(server, &app_id, &app_secret)
    }

    pub(crate) fn with_source(
        server: Option<&DanmakuServer>, app_id: &str, app_secret: &str,
    ) -> dandanapi_client::Result<Self> {
        let (app_id, app_secret) = (app_id.trim(), app_secret.trim());
        // Older settings can store the built-in App ID by itself.
        let credentials = (!app_secret.is_empty() || (!app_id.is_empty() && app_id != X_APPID))
            .then_some((app_id, app_secret));
        let (name, url) = server.map_or((OFFICIAL_SERVER_NAME, DanDanClient::BASE_URI), |server| {
            (server.name.as_str(), server.url.as_str())
        });

        let client = if server.is_none() && credentials.is_none() {
            static INIT: Once = Once::new();
            INIT.call_once(|| {
                if DanDanClient::init(X_APPID.to_owned(), yzlm::yzlm()).is_err() {
                    tracing::warn!("DanDanClient already initialized, using existing instance");
                }
            });
            DanDanClient::instance()
        } else {
            DanDanClient::instance().with_base_url(url, credentials)?
        };

        Ok(Self {
            source: DanmakuServer {
                name: name.to_owned(),
                url: client.base_uri().trim_end_matches('/').to_owned(),
            },
            client,
            app_id: credentials
                .map(|(app_id, _)| app_id)
                .or_else(|| server.is_none().then_some(X_APPID))
                .map(str::to_owned),
        })
    }

    pub fn source(&self) -> &DanmakuServer {
        &self.source
    }

    /// The normalized base URL and signing App ID of this client.
    pub fn source_identity(&self) -> (&str, Option<&str>) {
        (&self.source.url, self.app_id.as_deref())
    }
}

impl Deref for DanmakuClient {
    type Target = DanDanClient;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl DanmakuClient {
    pub async fn search_anime_details(
        &self, keyword: String, anime_type: AnimeType,
    ) -> anyhow::Result<Vec<SearchAnimeDetails>> {
        Ok(self
            .route(SearchSearchAnime {
                params: SearchSearchAnimeParams {
                    keyword,
                    r#type: anime_type,
                    v2: true,
                },
            })
            .await?
            .animes
            .unwrap_or_default())
    }

    pub async fn search_animes(
        &self, params: SearchSearchEpisodesParams,
    ) -> anyhow::Result<Vec<SearchEpisodesAnime>> {
        Ok(self
            .route(SearchSearchEpisodes { params })
            .await?
            .animes
            .unwrap_or_default())
    }

    pub async fn search_episode(
        &self, params: SearchSearchEpisodesParams,
    ) -> anyhow::Result<Option<(i64, String)>> {
        let anime = self.search_animes(params).await?;

        Ok(Self::first_episode_match(anime))
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

    pub async fn get_comments(&self, episode_id: i64) -> anyhow::Result<Option<Vec<Danmaku>>> {
        let response = self
            .route(CommentGetComment {
                episode_id,
                params: CommentGetCommentParams {
                    from: 0,
                    with_related: true,
                    ch_convert: 0,
                },
            })
            .await?;

        let Some(comments) = response.comments else {
            return Ok(None);
        };

        let danmaku = comments
            .into_iter()
            .map(DanmakuExt::into_danmaku)
            .filter(|danmaku| !danmaku.content.is_empty())
            .collect::<Vec<_>>();

        Ok((!danmaku.is_empty()).then_some(danmaku))
    }
}
