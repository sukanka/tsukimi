use std::{
    cell::{
        Cell,
        RefCell,
    },
    collections::HashMap,
    time::Instant,
};

use adw::{
    prelude::*,
    subclass::prelude::*,
};
use dandanapi_client::{
    AnimeType,
    SearchEpisodeDetails,
    SearchEpisodesAnime,
    SearchSearchEpisodesParams,
};
use gettextrs::gettext;
use gtk::{
    glib,
    template_callbacks,
};

use crate::{
    client::structs::SimpleListItem,
    ui::{
        mpv::{
            danmaku_cache_map::DanmakuCacheMap,
            page::MPVPage,
        },
        provider::tu_item::{
            DANMAKU_ANIME,
            MOVIE,
            TuItem,
        },
        widgets::{
            single_grid::imp::ViewType,
            tuview_scrolled::TuViewScrolled,
        },
    },
    utils::{
        spawn,
        spawn_tokio,
    },
};

use super::danmaku_client::DanmakuClient;

mod imp {
    use glib::subclass::InitializingObject;
    use gtk::{
        CompositeTemplate,
        glib,
    };

    use super::*;

    #[derive(Default, CompositeTemplate, glib::Properties)]
    #[template(resource = "/moe/tsuna/tsukimi/ui/danmaku_search_dialog.ui")]
    #[properties(wrapper_type = super::DanmakuSearchDialog)]
    pub struct DanmakuSearchDialog {
        #[template_child]
        pub view: TemplateChild<TuViewScrolled>,
        #[template_child]
        pub episode_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub navigation_view: TemplateChild<adw::NavigationView>,
        #[template_child]
        pub episodes_page: TemplateChild<adw::NavigationPage>,
        #[template_child]
        pub title_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub anime_type_dropdown: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub search_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub toast: TemplateChild<adw::ToastOverlay>,

        pub page: glib::WeakRef<MPVPage>,
        pub episodes: RefCell<Vec<TuItem>>,
        pub candidate_episodes: RefCell<HashMap<String, Vec<SearchEpisodeDetails>>>,
        pub search_generation: Cell<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DanmakuSearchDialog {
        const NAME: &'static str = "DanmakuSearchDialog";
        type Type = super::DanmakuSearchDialog;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
            klass.install_action_async(
                "danmaku-search-dialog.search",
                None,
                |dialog, _, _| async move {
                    dialog.search().await;
                },
            );
        }

        fn instance_init(obj: &InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for DanmakuSearchDialog {
        fn constructed(&self) {
            self.parent_constructed();
            self.view.set_view_type(ViewType::GridView);
        }
    }

    impl WidgetImpl for DanmakuSearchDialog {}
    impl AdwDialogImpl for DanmakuSearchDialog {}
}

glib::wrapper! {
    pub struct DanmakuSearchDialog(ObjectSubclass<imp::DanmakuSearchDialog>)
        @extends gtk::Widget, adw::Dialog, adw::PreferencesDialog, @implements gtk::Accessible, gtk::Root, gtk::Buildable, gtk::ConstraintTarget;
}

#[template_callbacks]
impl DanmakuSearchDialog {
    pub fn new(page: &MPVPage) -> Self {
        let dialog: Self = glib::Object::new();
        dialog.imp().page.set(Some(page));
        if let Some(item) = dialog.prefill_from_current_video() {
            dialog.load_tmdb_candidates(item);
        }
        dialog
    }

    fn prefill_from_current_video(&self) -> Option<TuItem> {
        let item = self
            .imp()
            .page
            .upgrade()
            .and_then(|page| page.current_video())?;

        self.imp()
            .title_entry
            .set_text(&item.series_name().unwrap_or_else(|| item.name()));
        if item.item_type() == MOVIE {
            self.imp().anime_type_dropdown.set_selected(3);
        }
        Some(item)
    }

    fn next_search_generation(&self) -> u64 {
        let generation = self.imp().search_generation.get().wrapping_add(1);
        self.imp().search_generation.set(generation);
        generation
    }

    fn is_current_search(&self, generation: u64) -> bool {
        self.imp().search_generation.get() == generation
    }

    fn load_tmdb_candidates(&self, item: TuItem) {
        let Some(page) = self.imp().page.upgrade() else {
            return;
        };
        let generation = self.next_search_generation();
        let tmdb_id_type = i32::from(item.item_type() == MOVIE);
        let fallback_title = item
            .series_name()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| item.name());

        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let tmdb_id = page.resolve_danmaku_tmdb_id(&item).await;
                if !obj.is_current_search(generation) {
                    return;
                }

                if let Some(tmdb_id) = tmdb_id {
                    let search_started = Instant::now();
                    let tmdb_result = spawn_tokio(async move {
                        let client = DanmakuClient::new()?;
                        client
                            .search_animes(SearchSearchEpisodesParams {
                                anime: None,
                                tmdb_id: Some(tmdb_id),
                                tmdb_id_type,
                                episode: None,
                                v2: true,
                            })
                            .await
                    })
                    .await;
                    let stale = !obj.is_current_search(generation);
                    match &tmdb_result {
                        Ok(animes) => tracing::info!(
                            flow = "manual_candidates",
                            strategy = "tmdb",
                            generation,
                            tmdb_id,
                            tmdb_id_type,
                            anime_count = animes.len(),
                            episode_count = animes
                                .iter()
                                .map(|anime| anime.episodes.as_ref().map_or(0, Vec::len))
                                .sum::<usize>(),
                            stale,
                            elapsed_ms = search_started.elapsed().as_millis(),
                            "Danmaku anime candidate search completed"
                        ),
                        Err(error) => tracing::warn!(
                            flow = "manual_candidates",
                            strategy = "tmdb",
                            generation,
                            tmdb_id,
                            tmdb_id_type,
                            %error,
                            stale,
                            elapsed_ms = search_started.elapsed().as_millis(),
                            "Danmaku anime candidate search failed"
                        ),
                    }
                    if stale {
                        return;
                    }

                    if let Ok(animes) = tmdb_result
                        && Self::has_usable_episode_candidates(&animes)
                    {
                        obj.set_anime_candidates(animes);
                        return;
                    }
                } else {
                    tracing::debug!(
                        flow = "manual_candidates",
                        strategy = "tmdb",
                        generation,
                        "No TMDB ID resolved; using the title fallback"
                    );
                }

                let trace_title = fallback_title.clone();
                let search_started = Instant::now();
                let result = spawn_tokio(async move {
                    let client = DanmakuClient::new()?;
                    client
                        .search_animes(SearchSearchEpisodesParams {
                            anime: Some(fallback_title),
                            tmdb_id: None,
                            tmdb_id_type,
                            episode: None,
                            v2: true,
                        })
                        .await
                })
                .await;
                let stale = !obj.is_current_search(generation);
                match &result {
                    Ok(animes) => tracing::info!(
                        flow = "manual_candidates",
                        strategy = "title_fallback",
                        generation,
                        title = trace_title,
                        anime_count = animes.len(),
                        episode_count = animes
                            .iter()
                            .map(|anime| anime.episodes.as_ref().map_or(0, Vec::len))
                            .sum::<usize>(),
                        stale,
                        elapsed_ms = search_started.elapsed().as_millis(),
                        "Danmaku anime candidate fallback search completed"
                    ),
                    Err(error) => tracing::warn!(
                        flow = "manual_candidates",
                        strategy = "title_fallback",
                        generation,
                        title = trace_title,
                        %error,
                        stale,
                        elapsed_ms = search_started.elapsed().as_millis(),
                        "Danmaku anime candidate fallback search failed"
                    ),
                }
                if stale {
                    return;
                }

                if let Ok(animes) = result {
                    obj.set_anime_candidates(animes);
                }
            }
        ));
    }

    fn has_usable_episode_candidates(animes: &[SearchEpisodesAnime]) -> bool {
        animes.iter().any(|anime| {
            anime
                .episodes
                .as_ref()
                .is_some_and(|episodes| episodes.iter().any(|episode| episode.episode_id.is_some()))
        })
    }

    fn set_anime_candidates(&self, animes: Vec<SearchEpisodesAnime>) {
        let mut episodes = HashMap::new();
        let items = animes
            .into_iter()
            .map(|anime| {
                if let Some(anime_id) = anime.anime_id {
                    let valid_episodes = anime
                        .episodes
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|episode| episode.episode_id.is_some())
                        .collect::<Vec<_>>();
                    if !valid_episodes.is_empty() {
                        episodes.insert(anime_id.to_string(), valid_episodes);
                    }
                }
                SimpleListItem::from(anime)
            })
            .collect::<Vec<_>>();
        self.imp().candidate_episodes.replace(episodes);
        self.imp().view.set_store::<true>(items);
    }

    #[template_callback]
    fn search_cb(&self) {
        let _ = self.activate_action("danmaku-search-dialog.search", None);
    }

    fn set_searching(&self, searching: bool) {
        self.imp()
            .search_stack
            .set_visible_child_name(if searching { "loading" } else { "search" });
        self.action_set_enabled("danmaku-search-dialog.search", !searching);
    }

    fn show_toast(&self, message: impl Into<String>) {
        self.imp().toast.add_toast(
            adw::Toast::builder()
                .timeout(2)
                .use_markup(false)
                .title(message.into())
                .build(),
        );
    }

    fn selected_anime_type(&self) -> AnimeType {
        match self.imp().anime_type_dropdown.selected() {
            0 => AnimeType::Tvseries,
            1 => AnimeType::Tvspecial,
            2 => AnimeType::Ova,
            3 => AnimeType::Movie,
            4 => AnimeType::Musicvideo,
            5 => AnimeType::Web,
            6 => AnimeType::Other,
            7 => AnimeType::Jpmovie,
            8 => AnimeType::Jpdrama,
            9 => AnimeType::Tmdbtv,
            10 => AnimeType::Tmdbmovie,
            _ => AnimeType::Tvseries,
        }
    }

    async fn search(&self) {
        let title = self.imp().title_entry.text().trim().to_string();
        if title.chars().count() < 2 {
            self.show_toast(gettext("Enter at least two characters"));
            return;
        }

        let anime_type = self.selected_anime_type();
        let trace_title = title.clone();
        let trace_anime_type = anime_type.clone();
        let generation = self.next_search_generation();
        self.set_searching(true);
        let search_started = Instant::now();
        let result = spawn_tokio(async move {
            let client = DanmakuClient::new()?;
            client.search_anime_details(title, anime_type).await
        })
        .await;
        let stale = !self.is_current_search(generation);
        match &result {
            Ok(animes) => tracing::info!(
                flow = "manual",
                strategy = "title",
                generation,
                title = trace_title,
                anime_type = ?trace_anime_type,
                anime_count = animes.len(),
                stale,
                elapsed_ms = search_started.elapsed().as_millis(),
                "Danmaku anime search completed"
            ),
            Err(error) => tracing::warn!(
                flow = "manual",
                strategy = "title",
                generation,
                title = trace_title,
                anime_type = ?trace_anime_type,
                %error,
                stale,
                elapsed_ms = search_started.elapsed().as_millis(),
                "Danmaku anime search failed"
            ),
        }
        if stale {
            return;
        }
        self.set_searching(false);

        match result {
            Ok(animes) => {
                self.imp().candidate_episodes.borrow_mut().clear();
                let items = animes
                    .into_iter()
                    .map(SimpleListItem::from)
                    .collect::<Vec<_>>();
                let is_empty = items.is_empty();
                self.imp().view.set_store::<true>(items);
                if is_empty {
                    self.show_toast(gettext("No matching anime found"));
                }
            }
            Err(error) => {
                self.show_toast(format!("{}: {error}", gettext("Search failed")));
            }
        }
    }

    fn set_episode_items(&self, items: Vec<SimpleListItem>) {
        while let Some(child) = self.imp().episode_list.first_child() {
            self.imp().episode_list.remove(&child);
        }

        let items = items
            .into_iter()
            .map(TuItem::from_simple)
            .collect::<Vec<_>>();
        for item in &items {
            let row = adw::ActionRow::builder()
                .title(item.name())
                .activatable(true)
                .build();
            row.connect_activated(glib::clone!(
                #[strong]
                item,
                move |row| item.activate(row)
            ));
            self.imp().episode_list.append(&row);
        }
        self.imp().episodes.replace(items);
    }

    pub fn apply_episode(&self, item: TuItem) {
        let Ok(episode_id) = item.id().parse::<i64>() else {
            self.show_toast(gettext("Invalid episode ID"));
            return;
        };
        let Some(page) = self.imp().page.upgrade() else {
            self.close();
            return;
        };

        let Some(current_item) = page.current_video() else {
            self.close();
            return;
        };
        let available_episodes = self.imp().episodes.borrow().clone();
        let item_name = item.series_name().map_or_else(
            || item.name(),
            |series_name| format!("{} - {series_name}", item.name()),
        );

        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                match page.apply_manual_danmaku(episode_id, item_name).await {
                    Ok(true) => {
                        let mut cache = DanmakuCacheMap::load();
                        if let Err(error) = cache.remember_manual_selection(
                            &current_item,
                            &item,
                            &available_episodes,
                        ) {
                            tracing::warn!("Failed to cache manual danmaku match: {error}");
                        }
                        obj.close();
                    }
                    Ok(false) => obj.show_toast(gettext("No danmaku found")),
                    Err(error) => {
                        obj.show_toast(format!("{}: {error}", gettext("Failed to load danmaku")))
                    }
                }
            }
        ));
    }

    pub fn open_anime(&self, item: TuItem) {
        if item.item_type() != DANMAKU_ANIME {
            return;
        }

        self.imp().episodes_page.set_title(&item.name());
        self.set_episode_items(Vec::new());
        self.imp()
            .navigation_view
            .push(&self.imp().episodes_page.get());

        let anime_id = item.id();
        let anime_title = item.name();
        let episode_series_name = anime_title.clone();
        let generation = self.next_search_generation();
        self.set_searching(false);

        if let Some(episodes) = self
            .imp()
            .candidate_episodes
            .borrow()
            .get(&anime_id)
            .cloned()
        {
            tracing::debug!(
                flow = "manual",
                strategy = "cached_tmdb_candidate",
                generation,
                anime_id,
                episode_count = episodes.len(),
                "Using cached Danmaku episode candidates"
            );
            self.set_episode_items(Self::episode_items(episodes, &episode_series_name));
            return;
        }

        let trace_anime_title = anime_title.clone();
        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let search_started = Instant::now();
                let result = spawn_tokio(async move {
                    let client = DanmakuClient::new()?;
                    client
                        .search_animes(SearchSearchEpisodesParams {
                            anime: Some(anime_title),
                            tmdb_id: None,
                            tmdb_id_type: 0,
                            episode: None,
                            v2: true,
                        })
                        .await
                })
                .await;
                let stale = !obj.is_current_search(generation);
                match &result {
                    Ok(animes) => tracing::info!(
                        flow = "manual",
                        strategy = "anime_title",
                        generation,
                        anime_id,
                        anime_title = trace_anime_title,
                        anime_count = animes.len(),
                        episode_count = animes
                            .iter()
                            .map(|anime| anime.episodes.as_ref().map_or(0, Vec::len))
                            .sum::<usize>(),
                        stale,
                        elapsed_ms = search_started.elapsed().as_millis(),
                        "Danmaku episode candidate search completed"
                    ),
                    Err(error) => tracing::warn!(
                        flow = "manual",
                        strategy = "anime_title",
                        generation,
                        anime_id,
                        anime_title = trace_anime_title,
                        %error,
                        stale,
                        elapsed_ms = search_started.elapsed().as_millis(),
                        "Danmaku episode candidate search failed"
                    ),
                }
                if stale {
                    return;
                }

                match result {
                    Ok(animes) => {
                        let episodes = animes
                            .into_iter()
                            .find(|anime| {
                                anime.anime_id.map(|id| id.to_string()).as_deref()
                                    == Some(anime_id.as_str())
                            })
                            .and_then(|anime| anime.episodes)
                            .unwrap_or_default();
                        let episodes = Self::episode_items(episodes, &episode_series_name);
                        let is_empty = episodes.is_empty();
                        obj.set_episode_items(episodes);
                        if is_empty {
                            obj.show_toast(gettext("No episodes found"));
                        }
                    }
                    Err(error) => {
                        obj.show_toast(format!("{}: {error}", gettext("Failed to load episodes")));
                    }
                }
            }
        ));
    }

    fn episode_items(
        episodes: Vec<SearchEpisodeDetails>, series_name: &str,
    ) -> Vec<SimpleListItem> {
        episodes
            .into_iter()
            .filter(|episode| episode.episode_id.is_some())
            .enumerate()
            .map(|(index, episode)| {
                let mut item = SimpleListItem::from(episode);
                item.index_number = u32::try_from(index + 1).ok();
                item.series_name = Some(series_name.to_string());
                item
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_search_requires_a_usable_episode_id() {
        let empty = serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([{
            "animeId": 1,
            "animeTitle": "Empty",
            "episodes": [{ "episodeId": null, "episodeTitle": "Unknown" }]
        }]))
        .expect("empty candidate response");
        let usable = serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([{
            "animeId": 2,
            "animeTitle": "Usable",
            "episodes": [{ "episodeId": 21, "episodeTitle": "Episode 1" }]
        }]))
        .expect("usable candidate response");

        assert!(!DanmakuSearchDialog::has_usable_episode_candidates(&empty));
        assert!(DanmakuSearchDialog::has_usable_episode_candidates(&usable));
    }

    #[test]
    fn episode_items_drop_entries_without_an_id() {
        let episodes = serde_json::from_value::<Vec<SearchEpisodeDetails>>(serde_json::json!([
            { "episodeId": null, "episodeTitle": "Unknown" },
            { "episodeId": 21, "episodeTitle": "Episode 1" }
        ]))
        .expect("episode response");

        let items = DanmakuSearchDialog::episode_items(episodes, "Series");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "21");
        assert_eq!(items[0].index_number, Some(1));
    }
}
