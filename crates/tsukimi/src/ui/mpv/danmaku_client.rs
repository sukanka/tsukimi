use std::{
    ops::Deref,
    sync::OnceLock,
    time::{
        Duration,
        Instant,
    },
};

use dandanapi_client::*;
use mutsumi::Danmaku;

use crate::ui::mpv::danmaku::DanmakuExt;

const KEY: Option<&str> = option_env!("DANDANAPI_SECRET_KEY");
const CIPHERTEXT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/data/dandanapi-secret.age"
));
const X_APPID: &str = "e9imrhcexn";

#[derive(Clone)]
pub struct DanmakuClient(DanDanClient);

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

impl DanmakuClient {
    pub fn instance() -> Option<Self> {
        static CLIENT: OnceLock<Option<DanmakuClient>> = OnceLock::new();
        CLIENT
            .get_or_init(|| {
                let Some(key) = KEY.map(str::trim).filter(|key| !key.is_empty()) else {
                    tracing::error!("Failed to initialize danmaku client: DANDANAPI_SECRET_KEY is not configured");
                    return None;
                };
                if DanDanClient::init(
                    X_APPID.to_string(),
                    SecretGenerator::new(CIPHERTEXT.to_vec(), key.to_string()),
                ).is_err() {
                    tracing::warn!("DanDanClient already initialized, using existing instance...");
                }
                Some(Self(DanDanClient::instance()))
            })
            .clone()
    }
}

impl Deref for DanmakuClient {
    type Target = DanDanClient;

    fn deref(&self) -> &Self::Target {
        &self.0
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
            .route(CommentGetComment {
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
mod tests {
    use super::*;

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
