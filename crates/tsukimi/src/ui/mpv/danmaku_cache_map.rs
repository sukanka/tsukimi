use std::collections::HashMap;

use serde::{
    Deserialize,
    Serialize,
};

use crate::{
    client::jellyfin_client::JELLYFIN_CLIENT,
    ui::{
        models::SETTINGS,
        mpv::danmaku_client::DanmakuClient,
        provider::tu_item::TuItem,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedDanmaku {
    pub episode_id: i64,
    pub item_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum DanmakuCacheEntry {
    Series {
        season: u32,
        local_episode: u32,
        selected_episode: u32,
        episode_ids: Vec<i64>,
    },
    Item {
        episode_id: i64,
    },
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DanmakuCacheMap {
    entries: HashMap<String, DanmakuCacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct DanmakuCacheScope {
    account_name: String,
    user_id: String,
    endpoint: Option<String>,
    danmaku_source: String,
}

impl DanmakuCacheScope {
    fn current() -> Self {
        let session = JELLYFIN_CLIENT.session();
        Self {
            account_name: session.account.servername.clone(),
            user_id: session.account.user_id.clone(),
            endpoint: session
                .url_headers
                .as_ref()
                .map(|(url, _headers)| url.to_string()),
            danmaku_source: DanmakuClient::source_fingerprint(),
        }
    }
}

impl DanmakuCacheMap {
    pub fn load() -> Self {
        serde_json::from_str(&SETTINGS.danmaku_cache_map()).unwrap_or_default()
    }

    pub fn cached_danmaku(&self, item: &TuItem) -> Option<CachedDanmaku> {
        let entry = self.entries.get(&Self::cache_key(item)?)?;
        let episode_id = match entry {
            DanmakuCacheEntry::Series {
                season,
                local_episode,
                selected_episode,
                episode_ids,
            } => {
                if item.parent_index_number() != *season || item.index_number() == 0 {
                    return None;
                }

                let index = i64::from(*selected_episode) + i64::from(item.index_number())
                    - i64::from(*local_episode);
                if index <= 0 {
                    return None;
                }
                *episode_ids.get(index as usize - 1)?
            }
            DanmakuCacheEntry::Item { episode_id } => *episode_id,
        };

        Some(CachedDanmaku {
            episode_id,
            item_name: Self::item_name(item),
        })
    }

    pub fn remember_manual_selection(
        &mut self, current_item: &TuItem, selected_episode: &TuItem, available_episodes: &[TuItem],
    ) -> anyhow::Result<()> {
        let key = Self::cache_key(current_item)
            .ok_or_else(|| anyhow::anyhow!("Current item has no stable cache key"))?;
        let selected_episode_id = Self::episode_id(selected_episode)?;

        let entry = if current_item.series_name().is_some() {
            let local_episode = current_item.index_number();
            let selected_episode = selected_episode.index_number();
            if local_episode == 0 || selected_episode == 0 {
                anyhow::bail!("Series episode index is unavailable");
            }

            let episode_ids = available_episodes
                .iter()
                .map(Self::episode_id)
                .collect::<anyhow::Result<Vec<_>>>()?;
            if episode_ids.is_empty() {
                anyhow::bail!("Danmaku episode list is empty");
            }

            DanmakuCacheEntry::Series {
                season: current_item.parent_index_number(),
                local_episode,
                selected_episode,
                episode_ids,
            }
        } else {
            DanmakuCacheEntry::Item {
                episode_id: selected_episode_id,
            }
        };

        self.entries.insert(key, entry);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        let value = serde_json::to_string(self)?;
        SETTINGS.set_danmaku_cache_map(&value)?;
        Ok(())
    }

    fn cache_key(item: &TuItem) -> Option<String> {
        let media_key = if item.series_name().is_some() {
            if let Some(series_id) = item.series_id().filter(|id| !id.is_empty()) {
                format!("series-id:{series_id}")
            } else {
                item.series_name()
                    .filter(|name| !name.trim().is_empty())
                    .map(|name| format!("series-name:{}", name.trim().to_lowercase()))?
            }
        } else {
            let id = item.id();
            (!id.is_empty()).then(|| format!("item:{id}"))?
        };

        Self::scoped_cache_key(&DanmakuCacheScope::current(), &media_key)
    }

    fn scoped_cache_key(scope: &DanmakuCacheScope, media_key: &str) -> Option<String> {
        serde_json::to_string(&("v2", scope, media_key)).ok()
    }

    fn episode_id(item: &TuItem) -> anyhow::Result<i64> {
        item.id()
            .parse()
            .map_err(|error| anyhow::anyhow!("Invalid danmaku episode ID: {error}"))
    }

    fn item_name(item: &TuItem) -> String {
        item.series_name().map_or_else(
            || item.name(),
            |series_name| format!("{} - {series_name}", item.name()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(endpoint: &str, danmaku_source: &str) -> DanmakuCacheScope {
        DanmakuCacheScope {
            account_name: "home".into(),
            user_id: "user".into(),
            endpoint: Some(endpoint.into()),
            danmaku_source: danmaku_source.into(),
        }
    }

    #[test]
    fn manual_matches_are_scoped_to_media_server_and_danmaku_source() {
        let official = DanmakuCacheMap::scoped_cache_key(
            &scope(
                "https://home.example/",
                "https://api.dandanplay.net|official",
            ),
            "item:episode",
        );
        let remote = DanmakuCacheMap::scoped_cache_key(
            &scope(
                "https://remote.example/",
                "https://api.dandanplay.net|official",
            ),
            "item:episode",
        );
        let custom = DanmakuCacheMap::scoped_cache_key(
            &scope("https://home.example/", "https://danmaku.example|anonymous"),
            "item:episode",
        );

        assert_ne!(official, remote);
        assert_ne!(official, custom);
    }
}
