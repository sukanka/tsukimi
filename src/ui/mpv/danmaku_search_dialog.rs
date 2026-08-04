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
    SearchAnimeDetails,
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

struct AnimeCandidateSearchResult {
    details: Vec<SearchAnimeDetails>,
    episode_animes: Vec<SearchEpisodesAnime>,
}

struct MergedAnimeCandidates {
    items: Vec<SimpleListItem>,
    episodes: HashMap<String, Vec<SearchEpisodeDetails>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateTypeFamily {
    TvSeries,
    TvSpecial,
    Ova,
    Movie,
    MusicVideo,
    Web,
    Other,
    JpMovie,
    JpDrama,
    TmdbTv,
    TmdbMovie,
}

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
        let fallback_anime_type = if item.item_type() == MOVIE {
            AnimeType::Movie
        } else {
            AnimeType::Tvseries
        };
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

                let mut tmdb_episode_animes = None;
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
                        tmdb_episode_animes = Some(animes);
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
                let used_tmdb = tmdb_episode_animes.is_some();
                let episode_animes = if let Some(episode_animes) = tmdb_episode_animes {
                    episode_animes
                } else {
                    let search_started = Instant::now();
                    let title_result = spawn_tokio(async move {
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
                    match &title_result {
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
                    let Ok(animes) = title_result else {
                        return;
                    };
                    animes
                };

                let is_empty = obj.set_anime_candidates(
                    AnimeCandidateSearchResult {
                        details: Vec::new(),
                        episode_animes: episode_animes.clone(),
                    },
                    &trace_title,
                    None,
                );
                if is_empty {
                    return;
                }

                let search_started = Instant::now();
                let details_title = trace_title.clone();
                let details_result = spawn_tokio(async move {
                    let client = DanmakuClient::new()?;
                    client
                        .search_anime_details(details_title, fallback_anime_type)
                        .await
                })
                .await;
                let stale = !obj.is_current_search(generation);
                match &details_result {
                    Ok(details) => tracing::info!(
                        flow = "manual_candidates",
                        strategy = if used_tmdb {
                            "tmdb_metadata"
                        } else {
                            "title_metadata"
                        },
                        generation,
                        title = trace_title,
                        detail_count = details.len(),
                        stale,
                        elapsed_ms = search_started.elapsed().as_millis(),
                        "Danmaku anime candidate metadata search completed"
                    ),
                    Err(error) => tracing::warn!(
                        flow = "manual_candidates",
                        strategy = if used_tmdb {
                            "tmdb_metadata"
                        } else {
                            "title_metadata"
                        },
                        generation,
                        title = trace_title,
                        %error,
                        stale,
                        elapsed_ms = search_started.elapsed().as_millis(),
                        "Danmaku anime candidate metadata search failed; keeping episode candidates"
                    ),
                }
                if stale {
                    return;
                }

                if let Ok(details) = details_result
                    && !details.is_empty()
                {
                    obj.set_anime_candidates(
                        AnimeCandidateSearchResult {
                            details,
                            episode_animes,
                        },
                        &trace_title,
                        None,
                    );
                }
            }
        ));
    }

    fn has_usable_episode_candidates(animes: &[SearchEpisodesAnime]) -> bool {
        animes.iter().any(Self::has_usable_episode_candidate)
    }

    fn has_usable_episode_candidate(anime: &SearchEpisodesAnime) -> bool {
        anime.anime_id.is_some()
            && anime
                .episodes
                .as_ref()
                .is_some_and(|episodes| episodes.iter().any(|episode| episode.episode_id.is_some()))
    }

    fn set_anime_candidates(
        &self, candidates: AnimeCandidateSearchResult, search_title: &str,
        expected_type: Option<CandidateTypeFamily>,
    ) -> bool {
        let merged = Self::merge_anime_candidates(candidates, search_title, expected_type);
        let is_empty = merged.items.is_empty();
        self.imp().candidate_episodes.replace(merged.episodes);
        self.imp().view.set_store::<true>(merged.items);
        is_empty
    }

    fn merge_anime_candidates(
        candidates: AnimeCandidateSearchResult, search_title: &str,
        expected_type: Option<CandidateTypeFamily>,
    ) -> MergedAnimeCandidates {
        let mut episodes = HashMap::new();
        let mut items = Vec::new();

        for mut anime in candidates.episode_animes {
            let Some(anime_id) = anime.anime_id else {
                continue;
            };
            if expected_type.is_some_and(|expected_type| {
                Self::candidate_type_family(
                    anime.r#type.as_ref(),
                    anime.type_description.as_deref(),
                )
                .is_some_and(|actual_type| actual_type != expected_type)
            }) {
                continue;
            }
            let candidate_id = anime_id.to_string();
            if episodes.contains_key(&candidate_id) {
                continue;
            }

            let image_url = Self::candidate_image_url(&anime, &candidates.details);
            let valid_episodes = anime
                .episodes
                .take()
                .unwrap_or_default()
                .into_iter()
                .filter(|episode| episode.episode_id.is_some())
                .collect::<Vec<_>>();
            if valid_episodes.is_empty() {
                continue;
            }

            let mut item = SimpleListItem::from(anime);
            item.image_url = image_url;
            item.series_name = Some(search_title.to_string());
            episodes.insert(candidate_id, valid_episodes);
            items.push(item);
        }

        MergedAnimeCandidates { items, episodes }
    }

    fn candidate_image_url(
        anime: &SearchEpisodesAnime, details: &[SearchAnimeDetails],
    ) -> Option<String> {
        let has_image = |detail: &SearchAnimeDetails| {
            detail
                .image_url
                .as_deref()
                .is_some_and(|url| !url.trim().is_empty())
        };

        if let Some(anime_id) = anime.anime_id
            && let Some(detail) = details
                .iter()
                .find(|detail| detail.anime_id == Some(anime_id) && has_image(detail))
        {
            return detail.image_url.clone();
        }

        let title = anime.anime_title.as_deref()?;
        let normalized_title = Self::normalized_candidate_title(title);
        if !normalized_title.is_empty()
            && let Some(detail) = Self::unique_matching_detail(details, |detail| {
                has_image(detail)
                    && Self::candidate_types_compatible(anime, detail)
                    && detail.anime_title.as_deref().is_some_and(|detail_title| {
                        Self::normalized_candidate_title(detail_title) == normalized_title
                    })
            })
        {
            return detail.image_url.clone();
        }

        let normalized_base_title = Self::normalized_candidate_base_title(title);
        if normalized_base_title.is_empty() {
            return None;
        }
        Self::unique_matching_detail(details, |detail| {
            has_image(detail)
                && Self::candidate_types_compatible(anime, detail)
                && detail.anime_title.as_deref().is_some_and(|detail_title| {
                    Self::normalized_candidate_base_title(detail_title) == normalized_base_title
                })
        })
        .and_then(|detail| detail.image_url.clone())
    }

    fn unique_matching_detail(
        details: &[SearchAnimeDetails], mut predicate: impl FnMut(&SearchAnimeDetails) -> bool,
    ) -> Option<&SearchAnimeDetails> {
        let mut matches = details.iter().filter(|detail| predicate(detail));
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    fn candidate_types_compatible(
        anime: &SearchEpisodesAnime, detail: &SearchAnimeDetails,
    ) -> bool {
        let anime_type =
            Self::candidate_type_family(anime.r#type.as_ref(), anime.type_description.as_deref());
        let detail_type =
            Self::candidate_type_family(detail.r#type.as_ref(), detail.type_description.as_deref());
        anime_type
            .zip(detail_type)
            .is_none_or(|(anime_type, detail_type)| {
                anime_type == detail_type
                    || matches!(
                        (anime_type, detail_type),
                        (CandidateTypeFamily::TvSeries, CandidateTypeFamily::TmdbTv)
                            | (CandidateTypeFamily::TmdbTv, CandidateTypeFamily::TvSeries)
                            | (CandidateTypeFamily::Movie, CandidateTypeFamily::TmdbMovie)
                            | (CandidateTypeFamily::TmdbMovie, CandidateTypeFamily::Movie)
                    )
            })
    }

    fn candidate_type_family(
        anime_type: Option<&AnimeType>, type_description: Option<&str>,
    ) -> Option<CandidateTypeFamily> {
        anime_type
            .and_then(Self::candidate_type_family_from_anime_type)
            .or_else(|| type_description.and_then(Self::candidate_type_family_from_label))
    }

    fn candidate_type_family_from_anime_type(
        anime_type: &AnimeType,
    ) -> Option<CandidateTypeFamily> {
        match anime_type {
            AnimeType::Tvseries => Some(CandidateTypeFamily::TvSeries),
            AnimeType::Tvspecial => Some(CandidateTypeFamily::TvSpecial),
            AnimeType::Ova => Some(CandidateTypeFamily::Ova),
            AnimeType::Movie => Some(CandidateTypeFamily::Movie),
            AnimeType::Musicvideo => Some(CandidateTypeFamily::MusicVideo),
            AnimeType::Web => Some(CandidateTypeFamily::Web),
            AnimeType::Other => Some(CandidateTypeFamily::Other),
            AnimeType::Jpmovie => Some(CandidateTypeFamily::JpMovie),
            AnimeType::Jpdrama => Some(CandidateTypeFamily::JpDrama),
            AnimeType::Tmdbtv => Some(CandidateTypeFamily::TmdbTv),
            AnimeType::Tmdbmovie => Some(CandidateTypeFamily::TmdbMovie),
            AnimeType::Unknown => None,
        }
    }

    fn candidate_type_family_from_label(label: &str) -> Option<CandidateTypeFamily> {
        let label = label
            .trim()
            .trim_matches(|character| matches!(character, '【' | '】' | '〖' | '〗' | '[' | ']'))
            .to_lowercase()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        match label.as_str() {
            "tvseries" | "tv动画" | "电视动画" | "日番" | "动漫" | "动画" => {
                Some(CandidateTypeFamily::TvSeries)
            }
            "tvspecial" | "tv特别篇" | "特别篇" => Some(CandidateTypeFamily::TvSpecial),
            "ova" | "oad" => Some(CandidateTypeFamily::Ova),
            "movie" | "电影" | "剧场版" => Some(CandidateTypeFamily::Movie),
            "musicvideo" | "音乐视频" | "mv" => Some(CandidateTypeFamily::MusicVideo),
            "web" | "网络动画" => Some(CandidateTypeFamily::Web),
            "other" | "其他" => Some(CandidateTypeFamily::Other),
            "jpmovie" | "日本电影" | "日影" => Some(CandidateTypeFamily::JpMovie),
            "jpdrama" | "日本电视剧" | "日剧" => Some(CandidateTypeFamily::JpDrama),
            "tmdbtv" | "tmdb电视剧" => Some(CandidateTypeFamily::TmdbTv),
            "tmdbmovie" | "tmdb电影" => Some(CandidateTypeFamily::TmdbMovie),
            _ => None,
        }
    }

    fn normalized_candidate_title(title: &str) -> String {
        let (base, sources) = Self::candidate_title_parts(title);
        if sources.is_empty() {
            base
        } else {
            format!("{base}|from:{}", sources.join("&"))
        }
    }

    fn normalized_candidate_base_title(title: &str) -> String {
        Self::candidate_title_parts(title).0
    }

    fn candidate_title_parts(title: &str) -> (String, Vec<String>) {
        let lowercase = title.trim().to_lowercase();
        let mut base = lowercase.as_str();
        let mut sources = Vec::new();
        if let Some(index) = lowercase.rfind("from ") {
            let candidate_base = &lowercase[..index];
            let stripped_base = Self::strip_trailing_candidate_category(candidate_base);
            let source_suffix = &lowercase[index + "from ".len()..];
            let candidate_sources = source_suffix
                .split('&')
                .map(|source| {
                    source
                        .chars()
                        .filter(|character| !character.is_whitespace())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            let has_recognized_category = stripped_base.len() < candidate_base.trim_end().len();
            let has_valid_sources = !candidate_sources.is_empty()
                && candidate_sources.iter().all(|source| {
                    !source.is_empty()
                        && source.chars().all(|character| {
                            character.is_alphanumeric() || matches!(character, '-' | '_' | '.')
                        })
                });
            if has_recognized_category && has_valid_sources {
                base = stripped_base;
                sources = candidate_sources;
            }
        }

        let base = base
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        sources.sort_unstable();
        sources.dedup();
        (base, sources)
    }

    fn strip_trailing_candidate_category(title: &str) -> &str {
        let title = title.trim_end();
        for (opening, closing) in [('【', '】'), ('〖', '〗'), ('[', ']')] {
            if title.ends_with(closing)
                && let Some(index) = title.rfind(opening)
            {
                let category_start = index + opening.len_utf8();
                let category_end = title.len() - closing.len_utf8();
                if Self::candidate_type_family_from_label(&title[category_start..category_end])
                    .is_some()
                {
                    return title[..index].trim_end();
                }
            }
        }
        title
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
        let expected_type = Self::candidate_type_family_from_anime_type(&anime_type);
        let details_title = title.clone();
        let details_anime_type = anime_type.clone();
        let trace_title = title.clone();
        let trace_anime_type = anime_type.clone();
        let generation = self.next_search_generation();
        self.set_searching(true);
        let search_started = Instant::now();
        let result = spawn_tokio(async move {
            let client = DanmakuClient::new()?;
            client
                .search_animes(SearchSearchEpisodesParams {
                    anime: Some(title),
                    tmdb_id: None,
                    tmdb_id_type: 0,
                    episode: None,
                    v2: true,
                })
                .await
        })
        .await;
        let stale = !self.is_current_search(generation);
        match &result {
            Ok(animes) => tracing::info!(
                flow = "manual",
                strategy = "title_episodes",
                generation,
                title = trace_title,
                anime_type = ?trace_anime_type,
                anime_count = animes.len(),
                episode_count = animes
                    .iter()
                    .map(|anime| anime.episodes.as_ref().map_or(0, Vec::len))
                    .sum::<usize>(),
                stale,
                elapsed_ms = search_started.elapsed().as_millis(),
                "Danmaku anime episode search completed"
            ),
            Err(error) => tracing::warn!(
                flow = "manual",
                strategy = "title_episodes",
                generation,
                title = trace_title,
                anime_type = ?trace_anime_type,
                %error,
                stale,
                elapsed_ms = search_started.elapsed().as_millis(),
                "Danmaku anime episode search failed"
            ),
        }
        if stale {
            return;
        }
        self.set_searching(false);

        let episode_animes = match result {
            Ok(animes) => animes,
            Err(error) => {
                self.show_toast(format!("{}: {error}", gettext("Search failed")));
                return;
            }
        };
        let is_empty = self.set_anime_candidates(
            AnimeCandidateSearchResult {
                details: Vec::new(),
                episode_animes: episode_animes.clone(),
            },
            &trace_title,
            expected_type,
        );
        if is_empty {
            self.show_toast(gettext("No matching anime found"));
            return;
        }

        let search_started = Instant::now();
        let details_result = spawn_tokio(async move {
            let client = DanmakuClient::new()?;
            client
                .search_anime_details(details_title, details_anime_type)
                .await
        })
        .await;
        let stale = !self.is_current_search(generation);
        match &details_result {
            Ok(details) => tracing::info!(
                flow = "manual",
                strategy = "title_metadata",
                generation,
                title = trace_title,
                anime_type = ?trace_anime_type,
                detail_count = details.len(),
                stale,
                elapsed_ms = search_started.elapsed().as_millis(),
                "Danmaku anime metadata search completed"
            ),
            Err(error) => tracing::warn!(
                flow = "manual",
                strategy = "title_metadata",
                generation,
                title = trace_title,
                anime_type = ?trace_anime_type,
                %error,
                stale,
                elapsed_ms = search_started.elapsed().as_millis(),
                "Danmaku anime metadata search failed; keeping episode candidates"
            ),
        }
        if stale {
            return;
        }

        if let Ok(details) = details_result
            && !details.is_empty()
        {
            self.set_anime_candidates(
                AnimeCandidateSearchResult {
                    details,
                    episode_animes,
                },
                &trace_title,
                expected_type,
            );
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
                .use_markup(false)
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
        let episode_search_title = item
            .series_name()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| anime_title.clone());
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
        let trace_search_title = episode_search_title.clone();
        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let search_started = Instant::now();
                let result = spawn_tokio(async move {
                    let client = DanmakuClient::new()?;
                    client
                        .search_animes(SearchSearchEpisodesParams {
                            anime: Some(episode_search_title),
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
                        search_title = trace_search_title,
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
                        search_title = trace_search_title,
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
                        let episodes =
                            Self::select_candidate_episodes(&animes, &anime_id, &trace_anime_title);
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

    fn select_candidate_episodes(
        animes: &[SearchEpisodesAnime], anime_id: &str, anime_title: &str,
    ) -> Vec<SearchEpisodeDetails> {
        if let Some(anime) = animes.iter().find(|anime| {
            Self::has_usable_episode_candidate(anime)
                && anime
                    .anime_id
                    .is_some_and(|candidate_id| candidate_id.to_string() == anime_id)
        }) {
            return anime.episodes.clone().unwrap_or_default();
        }

        for normalize in [
            Self::normalized_candidate_title as fn(&str) -> String,
            Self::normalized_candidate_base_title,
        ] {
            let normalized_title = normalize(anime_title);
            if normalized_title.is_empty() {
                continue;
            }
            let mut matches = animes.iter().filter(|anime| {
                Self::has_usable_episode_candidate(anime)
                    && anime
                        .anime_title
                        .as_deref()
                        .is_some_and(|title| normalize(title) == normalized_title)
            });
            if let Some(first) = matches.next()
                && matches.next().is_none()
            {
                return first.episodes.clone().unwrap_or_default();
            }
        }

        Vec::new()
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
        let missing_anime_id =
            serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([{
                "animeId": null,
                "animeTitle": "Missing anime ID",
                "episodes": [{ "episodeId": 22, "episodeTitle": "Episode 1" }]
            }]))
            .expect("candidate response without an anime ID");

        assert!(!DanmakuSearchDialog::has_usable_episode_candidates(&empty));
        assert!(DanmakuSearchDialog::has_usable_episode_candidates(&usable));
        assert!(!DanmakuSearchDialog::has_usable_episode_candidates(
            &missing_anime_id
        ));
    }

    #[test]
    fn candidate_merge_adds_cover_without_replacing_episode_identity() {
        let episode_animes =
            serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([{
                "animeId": 900,
                "animeTitle": "薰香花朵凛然绽放(2025)【TV动画】from dandan&animeko&bilibili",
                "episodes": [{ "episodeId": 21, "episodeTitle": "Episode 1" }]
            }]))
            .expect("episode candidates");
        let details = serde_json::from_value::<Vec<SearchAnimeDetails>>(serde_json::json!([{
            "animeId": 123,
            "animeTitle": "薰香花朵凛然绽放(2025)【日番】from bilibili&dandan&animeko",
            "imageUrl": "https://example.com/cover.jpg"
        }]))
        .expect("anime details");

        let merged = DanmakuSearchDialog::merge_anime_candidates(
            AnimeCandidateSearchResult {
                details,
                episode_animes,
            },
            "薰香花朵凛然绽放",
            None,
        );

        assert_eq!(merged.items.len(), 1);
        assert_eq!(merged.items[0].id, "900");
        assert_eq!(
            merged.items[0].image_url.as_deref(),
            Some("https://example.com/cover.jpg")
        );
        assert_eq!(
            merged.items[0].series_name.as_deref(),
            Some("薰香花朵凛然绽放")
        );
        assert_eq!(merged.episodes["900"][0].episode_id, Some(21));
    }

    #[test]
    fn candidate_merge_does_not_guess_an_ambiguous_cover() {
        let episode_animes =
            serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([{
                "animeId": 900,
                "animeTitle": "Same title",
                "episodes": [{ "episodeId": 21, "episodeTitle": "Episode 1" }]
            }]))
            .expect("episode candidates");
        let details = serde_json::from_value::<Vec<SearchAnimeDetails>>(serde_json::json!([
            {
                "animeId": 123,
                "animeTitle": "Same title",
                "imageUrl": "https://example.com/first.jpg"
            },
            {
                "animeId": 456,
                "animeTitle": "Same title",
                "imageUrl": "https://example.com/second.jpg"
            }
        ]))
        .expect("ambiguous anime details");

        let merged = DanmakuSearchDialog::merge_anime_candidates(
            AnimeCandidateSearchResult {
                details,
                episode_animes,
            },
            "Same title",
            None,
        );

        assert_eq!(merged.items.len(), 1);
        assert_eq!(merged.items[0].image_url, None);
    }

    #[test]
    fn candidate_merge_respects_the_selected_anime_type() {
        let episode_animes =
            serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([
                {
                    "animeId": 900,
                    "animeTitle": "Same title【TV动画】from dandan",
                    "typeDescription": "TV动画",
                    "episodes": [{ "episodeId": 21, "episodeTitle": "Episode 1" }]
                },
                {
                    "animeId": 901,
                    "animeTitle": "Same title【电影】from dandan",
                    "typeDescription": "电影",
                    "episodes": [{ "episodeId": 22, "episodeTitle": "Movie" }]
                }
            ]))
            .expect("mixed-type episode candidates");

        let merged = DanmakuSearchDialog::merge_anime_candidates(
            AnimeCandidateSearchResult {
                details: Vec::new(),
                episode_animes,
            },
            "Same title",
            Some(CandidateTypeFamily::Movie),
        );

        assert_eq!(merged.items.len(), 1);
        assert_eq!(merged.items[0].id, "901");
        assert!(merged.episodes.contains_key("901"));
    }

    #[test]
    fn title_matching_does_not_strip_regular_from_or_bracket_suffixes() {
        assert_ne!(
            DanmakuSearchDialog::normalized_candidate_title("Title [Season 1]"),
            DanmakuSearchDialog::normalized_candidate_title("Title [Season 2]")
        );
        assert_eq!(
            DanmakuSearchDialog::normalized_candidate_base_title("Re:ZERO From Zero"),
            "re:zerofromzero"
        );
    }

    #[test]
    fn title_cover_fallback_does_not_cross_known_anime_types() {
        let episode_animes =
            serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([{
                "animeId": 900,
                "animeTitle": "Same title【TV动画】from dandan",
                "typeDescription": "TV动画",
                "episodes": [{ "episodeId": 21, "episodeTitle": "Episode 1" }]
            }]))
            .expect("TV episode candidate");
        let details = serde_json::from_value::<Vec<SearchAnimeDetails>>(serde_json::json!([{
            "animeId": 901,
            "animeTitle": "Same title【电影】from dandan",
            "typeDescription": "电影",
            "imageUrl": "https://example.com/movie.jpg"
        }]))
        .expect("movie details");

        let merged = DanmakuSearchDialog::merge_anime_candidates(
            AnimeCandidateSearchResult {
                details,
                episode_animes,
            },
            "Same title",
            None,
        );

        assert_eq!(merged.items.len(), 1);
        assert_eq!(merged.items[0].image_url, None);
    }

    #[test]
    fn episode_selection_falls_back_to_the_normalized_title() {
        let animes = serde_json::from_value::<Vec<SearchEpisodesAnime>>(serde_json::json!([{
            "animeId": 900,
            "animeTitle": "薰香花朵凛然绽放(2025)【TV动画】from dandan&animeko&bilibili",
            "episodes": [{ "episodeId": 21, "episodeTitle": "Episode 1" }]
        }]))
        .expect("episode candidates");

        let episodes = DanmakuSearchDialog::select_candidate_episodes(
            &animes,
            "123",
            "薰香花朵凛然绽放(2025)【日番】from bilibili&dandan&animeko",
        );

        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].episode_id, Some(21));
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
