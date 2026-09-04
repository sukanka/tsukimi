use std::{
    collections::VecDeque,
    time::{
        Duration,
        Instant,
    },
};

use adw::prelude::*;
use dandanapi_client::SearchSearchEpisodesParams;
use gettextrs::gettext;
use glib::Object;
use gtk::{
    Builder,
    PopoverMenu,
    gdk::Rectangle,
    gio,
    glib::{
        self,
        translate::IntoGlib,
    },
    subclass::prelude::*,
};
use itertools::Itertools;
use mutsumi::*;
use url::Url;

use super::{
    DanmakuPopoverStatus,
    danmaku_cache_map::DanmakuCacheMap,
    danmaku_client::{
        DanmakuClient,
        DanmakuLoadResult,
        EpisodeSearchResult,
    },
    sink::MPVPlaySink,
    video_scale::VideoScale,
};
use crate::{
    client::{
        error::UserFacingError,
        jellyfin_client::{
            BackType,
            JELLYFIN_CLIENT,
        },
        structs::{
            Back,
            MediaSegmentType,
            MediaSource,
            MediaStream,
        },
    },
    close_on_error,
    ui::{
        GlobalToast,
        models::SETTINGS,
        provider::tu_item::{
            MOVIE,
            TuItem,
            item_type::EPISODE,
        },
        widgets::{
            check_row::CheckRow,
            item::SelectedVideoSubInfo,
            item_utils::{
                make_subtitle_version_choice,
                make_video_version_choice_from_matcher,
            },
            song_widget::format_duration,
            window::Window,
        },
    },
    utils::{
        spawn,
        spawn_g_timeout,
        spawn_tokio,
        spawn_tokio_blocking,
    },
};

const MIN_MOTION_TIME: i64 = 100000;
const PREV_CHAPTER_KEY: gtk::gdk::Key = gtk::gdk::Key::Page_Down;
const NEXT_CHAPTER_KEY: gtk::gdk::Key = gtk::gdk::Key::Page_Up;
const EXTERNAL_DANMAKU_CHUNK_BYTES: usize = 64 * 1024;
const MAX_EXTERNAL_DANMAKU_BYTES: usize = 16 * 1024 * 1024;

fn danmaku_timeline(items: &[Danmaku]) -> Vec<f64> {
    items
        .iter()
        .filter_map(|item| {
            (item.start.is_finite() && item.start >= 0.0).then_some(item.start / 1000.0)
        })
        .collect()
}

async fn fetch_danmaku_track(url: &str) -> anyhow::Result<Vec<Danmaku>> {
    let file = gio::File::for_uri(url);
    let stream = file.read_future(glib::Priority::DEFAULT).await?;
    let mut bytes = Vec::new();
    loop {
        let chunk = stream
            .read_bytes_future(EXTERNAL_DANMAKU_CHUNK_BYTES, glib::Priority::DEFAULT)
            .await?;
        if chunk.is_empty() {
            break;
        }
        if bytes.len().saturating_add(chunk.len()) > MAX_EXTERNAL_DANMAKU_BYTES {
            let _ = stream.close_future(glib::Priority::DEFAULT).await;
            anyhow::bail!("External danmaku track exceeds the 16 MiB limit");
        }
        bytes.extend_from_slice(chunk.as_ref());
    }
    stream.close_future(glib::Priority::DEFAULT).await?;

    spawn_tokio_blocking(move || -> anyhow::Result<Vec<Danmaku>> {
        let xml = String::from_utf8(bytes)?;
        Ok(mutsumi::parse_bilibili_xml(&xml)?)
    })
    .await
}

fn external_source_for_log(source: &str) -> String {
    let Ok(mut source) = url::Url::parse(source) else {
        return "<invalid external source>".into();
    };
    source.set_query(None);
    source.set_fragment(None);
    let _ = source.set_password(None);
    let _ = source.set_username("");
    source.to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackCacheScope {
    account_name: String,
    user_id: String,
    endpoint: Option<String>,
}

impl PlaybackCacheScope {
    fn current() -> Self {
        let session = JELLYFIN_CLIENT.session();
        Self {
            account_name: session.account.servername.clone(),
            user_id: session.account.user_id.clone(),
            endpoint: session
                .url_headers
                .as_ref()
                .map(|(url, _headers)| url.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackCacheKey {
    scope: PlaybackCacheScope,
    item_id: String,
    media_source_id: Option<String>,
    video_index: Option<i64>,
    subtitle_index: Option<i64>,
    video_matcher: Option<String>,
}

impl PlaybackCacheKey {
    fn new(
        scope: PlaybackCacheScope, item_id: String, selected: Option<&SelectedVideoSubInfo>,
        video_matcher: Option<String>,
    ) -> Self {
        Self {
            scope,
            item_id,
            media_source_id: selected.map(|selected| selected.media_source_id.clone()),
            video_index: selected.map(|selected| selected.video_index),
            subtitle_index: selected.map(|selected| selected.sub_index),
            video_matcher,
        }
    }
}

fn should_refresh_danmaku(
    item_changed: bool, cached: Option<&PlaybackCacheKey>, requested: &PlaybackCacheKey,
) -> bool {
    item_changed || cached.is_some_and(|cached| cached != requested)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackRequest {
    generation: u64,
    cache_key: PlaybackCacheKey,
}

#[derive(Default)]
struct PlaybackLoadState {
    issued: VecDeque<PlaybackRequest>,
    active: Option<PlaybackRequest>,
}

impl PlaybackLoadState {
    fn issued(&mut self, request: PlaybackRequest) {
        self.issued.push_back(request);
    }

    fn start_file(&mut self) -> Option<&PlaybackRequest> {
        if let Some(request) = self.issued.pop_front() {
            self.active = Some(request);
        }
        self.active.as_ref()
    }

    fn file_loaded(&self, current_generation: u64) -> Option<&PlaybackCacheKey> {
        self.active
            .as_ref()
            .filter(|request| request.generation == current_generation)
            .map(|request| &request.cache_key)
    }

    fn resume_cached(&mut self, request: PlaybackRequest) {
        self.active = Some(request);
    }

    fn take_active(&mut self) -> Option<PlaybackRequest> {
        self.active.take()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManualDanmakuTransaction {
    external_source: Option<String>,
    had_rendered_danmaku: bool,
}

impl ManualDanmakuTransaction {
    fn new(external_source: Option<String>, had_rendered_danmaku: bool) -> Self {
        Self {
            external_source,
            had_rendered_danmaku,
        }
    }

    fn preserve_renderer(&self) -> bool {
        self.had_rendered_danmaku
    }

    fn external_source_to_restore(&self) -> Option<&str> {
        (!self.had_rendered_danmaku)
            .then_some(self.external_source.as_deref())
            .flatten()
    }
}

#[derive(Clone, Copy)]
enum MpvTrackKind {
    Audio,
    Subtitle,
}

impl MpvTrackKind {
    fn track_kind(self) -> TrackKind {
        match self {
            Self::Audio => TrackKind::Audio,
            Self::Subtitle => TrackKind::Subtitle,
        }
    }

    fn property(self) -> &'static str {
        match self {
            Self::Audio => "aid",
            Self::Subtitle => "sid",
        }
    }
}

enum MediaSourceFallback {
    DirectPlay(String),
    PlaybackInfo,
}

mod imp {

    use std::cell::{
        Cell,
        RefCell,
    };

    use adw::prelude::*;
    use gettextrs::gettext;
    use glib::subclass::InitializingObject;
    use gtk::{
        CompositeTemplate,
        PopoverMenu,
        glib,
        subclass::prelude::*,
    };

    use mpris_server::LocalServer;
    use once_cell::sync::OnceCell;

    use super::MediaSourceFallback;

    use crate::{
        APP_ID,
        client::structs::{
            Back,
            MediaSegment,
        },
        ui::{
            models::SETTINGS,
            mpv::{
                DanmakuPopover,
                VolumeBar,
                danmaku_sync::DanmakuSync,
                menu_actions::MenuActions,
                sink::MPVPlaySink,
                video_scale::VideoScale,
            },
            provider::tu_item::TuItem,
        },
    };

    #[derive(CompositeTemplate, Default, glib::Properties)]
    #[template(resource = "/moe/tsuna/tsukimi/ui/mpvpage.ui")]
    #[properties(wrapper_type = super::MPVPage)]
    pub struct MPVPage {
        #[property(get, set, nullable)]
        pub url: RefCell<Option<String>>,
        #[property(get, set = Self::set_fullscreened, explicit_notify)]
        pub fullscreened: Cell<bool>,
        #[property(get, set = Self::set_paused)]
        pub paused: Cell<bool>,
        #[template_child]
        pub video: TemplateChild<MPVPlaySink>,
        #[template_child]
        pub danmakw: TemplateChild<mutsumi::Danmakw>,
        #[template_child]
        pub danmaku_popover_content: TemplateChild<DanmakuPopover>,
        #[template_child]
        pub bottom_revealer: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub top_revealer: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub play_pause_image: TemplateChild<gtk::Image>,
        #[template_child]
        pub video_scale: TemplateChild<VideoScale>,
        #[template_child]
        pub progress_time_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub duration_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub spinner: TemplateChild<adw::Spinner>,
        #[template_child]
        pub loading_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub skip_segment_revealer: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub skip_segment_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub network_speed_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub network_speed_label_2: TemplateChild<gtk::Button>,
        #[template_child]
        pub playback_speed_indicator: TemplateChild<gtk::Button>,
        #[template_child]
        pub playback_speed_button_content: TemplateChild<adw::ButtonContent>,
        #[template_child]
        pub audio_tracks_menu_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub subtitle_tracks_menu_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub title_label1: TemplateChild<gtk::Label>,
        #[template_child]
        pub title_label2: TemplateChild<gtk::Label>,
        #[template_child]
        pub sub_listbox: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub audio_listbox: TemplateChild<gtk::ListBox>,
        pub timeout: RefCell<Option<glib::source::SourceId>>,
        pub back_timeout: RefCell<Option<glib::source::SourceId>>,
        pub back: RefCell<Option<Back>>,
        pub seeking: Cell<bool>,
        pub(super) danmaku_sync: DanmakuSync,
        pub last_playback_position: Cell<f64>,
        pub x: Cell<f64>,
        pub y: Cell<f64>,
        pub last_motion_time: Cell<i64>,
        pub suburl: RefCell<Option<String>>,
        pub(super) media_source_fallback: RefCell<Option<MediaSourceFallback>>,
        pub skippable_segments: RefCell<Option<Vec<MediaSegment>>>,
        pub current_segment_end: Cell<Option<f64>>,
        pub popover: RefCell<Option<PopoverMenu>>,
        pub popover_count: Cell<u32>,
        pub menu_actions: MenuActions,

        pub mpris_server: OnceCell<LocalServer<super::MPVPage>>,

        pub mpris_art_url: RefCell<Option<String>>,

        #[template_child]
        pub playback_speed_adj: TemplateChild<gtk::Adjustment>,

        #[template_child]
        pub volume_adj: TemplateChild<gtk::Adjustment>,

        #[template_child]
        pub volume_button: TemplateChild<gtk::MenuButton>,

        #[template_child]
        pub volume_bar: TemplateChild<VolumeBar>,

        #[property(get, set, nullable)]
        pub current_video: RefCell<Option<TuItem>>,
        pub current_episode_list: RefCell<Vec<TuItem>>,

        #[property(get, set, default_value = true)]
        pub key_vaild: Cell<bool>,

        pub video_version_matcher: RefCell<Option<String>>,
        pub last_nonzero_volume: Cell<i64>,
        pub danmaku_count: Cell<usize>,
        pub danmaku_generation: Cell<u64>,
        pub external_danmaku_source: RefCell<Option<String>>,
        pub file_loaded: Cell<bool>,
        pub(super) cached_playback: RefCell<Option<super::PlaybackCacheKey>>,
        pub(super) playback_loads: RefCell<super::PlaybackLoadState>,
        pub(super) play_generation: Cell<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MPVPage {
        const NAME: &'static str = "MPVPage";
        type Type = super::MPVPage;
        type ParentType = adw::NavigationPage;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
            klass.install_action("mpv.play-pause", None, move |mpv, _action, _parameter| {
                mpv.on_play_pause_clicked();
            });
            klass.install_action("mpv.show-info", None, move |mpv, _action, _parameter| {
                mpv.on_info_clicked();
            });
            klass.install_action("mpv.backward", None, move |mpv, _action, _parameter| {
                mpv.on_backward();
            });
            klass.install_action("mpv.forward", None, move |mpv, _action, _parameter| {
                mpv.on_forward();
            });
            klass.install_action("mpv.chapter-prev", None, move |mpv, _action, _parameter| {
                mpv.chapter_prev();
            });
            klass.install_action("mpv.chapter-next", None, move |mpv, _action, _parameter| {
                mpv.chapter_next();
            });
            klass.install_action(
                "mpv.show-settings",
                None,
                move |mpv, _action, _parameter| {
                    mpv.on_sidebar_clicked();
                },
            );
            klass.install_action(
                "mpv.show-playlist",
                None,
                move |mpv, _action, _parameter| {
                    mpv.on_playlist_clicked();
                },
            );
            klass.install_action_async(
                "mpv.next-video",
                None,
                |mpv, _action, _parameter| async move {
                    mpv.on_next_video().await;
                },
            );
            klass.install_action_async(
                "mpv.previous-video",
                None,
                move |mpv, _action, _parameter| async move {
                    mpv.on_previous_video().await;
                },
            );
        }

        fn instance_init(obj: &InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for MPVPage {
        fn constructed(&self) {
            self.parent_constructed();

            SETTINGS
                .bind(
                    "mpv-show-buffer-speed",
                    &self.network_speed_label_2.get(),
                    "visible",
                )
                .build();

            SETTINGS
                .bind("mpv-default-volume", &self.volume_adj.get(), "value")
                .build();

            self.video_scale.set_player(Some(&self.video.get()));

            let obj = self.obj();

            self.danmaku_popover_content.set_page(&obj);
            obj.set_popover();

            obj.connect_root_notify(|obj| {
                if let Some(window) = obj.root().and_downcast::<gtk::Window>() {
                    window
                        .bind_property("fullscreened", obj, "fullscreened")
                        .sync_create()
                        .build();
                }
            });

            obj.listen_events();

            // Initialize MPRIS server

            glib::spawn_future_local(glib::clone!(
                #[weak(rename_to = imp)]
                self,
                async move {
                    let app_id = format!("{}.{}", APP_ID, "mpv");
                    if let Err(e) = imp.obj().initialize_mpris(&app_id).await {
                        tracing::warn!("Failed to initialize mpris server: {}", e);
                    }
                }
            ));
        }
    }

    impl WidgetImpl for MPVPage {}

    impl WindowImpl for MPVPage {}

    impl ApplicationWindowImpl for MPVPage {}

    impl adw::subclass::navigation_page::NavigationPageImpl for MPVPage {}

    impl MPVPage {
        fn set_fullscreened(&self, fullscreened: bool) {
            if fullscreened == self.fullscreened.get() {
                return;
            }

            self.fullscreened.set(fullscreened);

            self.obj().notify_fullscreened();
        }

        fn set_paused(&self, paused: bool) {
            let play_pause_image = self.play_pause_image.get();
            let menu_actions_play_pause_button = self.menu_actions.imp().play_pause_button.get();
            if paused {
                play_pause_image.set_icon_name(Some("media-playback-start-symbolic"));
                play_pause_image.set_tooltip_text(Some(&gettext("Play")));
                menu_actions_play_pause_button.set_icon_name("media-playback-start-symbolic");
                menu_actions_play_pause_button.set_tooltip_text(Some(&gettext("Play")));
            } else {
                play_pause_image.set_icon_name(Some("media-playback-pause-symbolic"));
                play_pause_image.set_tooltip_text(Some(&gettext("Pause")));
                menu_actions_play_pause_button.set_icon_name("media-playback-pause-symbolic");
                menu_actions_play_pause_button.set_tooltip_text(Some(&gettext("Pause")));
            }
            self.paused.set(paused);

            if self.file_loaded.get()
                && !self.loading_box.is_visible()
                && self.danmaku_popover_content.enabled()
            {
                self.danmakw.set_paused(paused);
            }
        }
    }
}

glib::wrapper! {
    pub struct MPVPage(ObjectSubclass<imp::MPVPage>)
        @extends gtk::ApplicationWindow, gtk::Window, gtk::Widget ,adw::NavigationPage,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Default for MPVPage {
    fn default() -> Self {
        Self::new()
    }
}

#[gtk::template_callbacks]
impl MPVPage {
    pub fn new() -> Self {
        Object::new()
    }

    fn next_danmaku_generation(&self) -> u64 {
        let generation = self.imp().danmaku_generation.get().wrapping_add(1);
        self.imp().danmaku_generation.set(generation);
        generation
    }

    fn is_current_danmaku_request(&self, generation: u64, item_id: &str) -> bool {
        self.imp().danmaku_generation.get() == generation
            && self
                .current_video()
                .as_ref()
                .is_some_and(|item| item.id() == item_id)
    }

    fn has_external_danmaku_source(&self) -> bool {
        self.imp().external_danmaku_source.borrow().is_some()
    }

    fn load_external_danmaku_track(&self, track: Option<DanmakuTrack>) {
        let source = track.map(|track| track.external_url);
        if self.imp().external_danmaku_source.borrow().as_ref() == source.as_ref() {
            return;
        }

        let generation = self.next_danmaku_generation();
        let previous = self.imp().external_danmaku_source.replace(source.clone());
        if source.is_none() {
            if previous.is_some() {
                self.clear_danmaku(DanmakuPopoverStatus::Idle);
                if self.imp().danmaku_popover_content.enabled()
                    && let Some(item) = self.current_video()
                {
                    if let Some(client) = self.new_danmaku_client() {
                        self.auto_search_danmaku(client, &item);
                    }
                }
            }
            return;
        }

        let source = source.expect("external danmaku source is present");
        let source_for_log = external_source_for_log(&source);
        let imp = self.imp();
        imp.danmaku_count.set(0);
        imp.danmakw.stop_rendering();
        imp.danmakw.clear_danmaku();
        imp.video_scale.set_danmaku_timeline(Vec::new());
        imp.danmaku_popover_content
            .set_status(DanmakuPopoverStatus::Loading);

        spawn_g_timeout(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let started = std::time::Instant::now();
                let result = fetch_danmaku_track(&source).await;
                if obj.imp().danmaku_generation.get() != generation
                    || obj.imp().external_danmaku_source.borrow().as_deref()
                        != Some(source.as_str())
                {
                    return;
                }

                match result {
                    Ok(danmaku) if !danmaku.is_empty() => {
                        let count = danmaku.len();
                        obj.apply_external_danmaku(danmaku);
                        tracing::info!(
                            source = %source_for_log,
                            count,
                            elapsed_ms = started.elapsed().as_millis(),
                            "External danmaku track loaded"
                        );
                    }
                    Ok(_) => {
                        tracing::warn!(
                            source = %source_for_log,
                            "External danmaku track was empty"
                        );
                        obj.fallback_from_external_danmaku();
                    }
                    Err(_) => {
                        tracing::warn!(
                            source = %source_for_log,
                            elapsed_ms = started.elapsed().as_millis(),
                            "External danmaku track failed to load"
                        );
                        obj.fallback_from_external_danmaku();
                    }
                }
            }
        ));
    }

    fn apply_external_danmaku(&self, danmaku: Vec<Danmaku>) {
        let imp = self.imp();
        let count = danmaku.len();
        let timeline = danmaku_timeline(&danmaku);
        imp.danmakw.load_danmaku(danmaku);
        imp.video_scale.set_danmaku_timeline(timeline);
        imp.danmaku_count.set(count);
        imp.danmaku_popover_content
            .set_status(DanmakuPopoverStatus::ExternalLoaded(
                count,
                gettext("External danmaku track"),
            ));
        self.update_danmaku_rendering(imp.danmaku_popover_content.enabled());
    }

    fn fallback_from_external_danmaku(&self) {
        self.imp().external_danmaku_source.take();
        self.clear_danmaku(DanmakuPopoverStatus::Unavailable);
        if self.imp().danmaku_popover_content.enabled()
            && let Some(item) = self.current_video()
        {
            if let Some(client) = self.new_danmaku_client() {
                self.auto_search_danmaku(client, &item);
            }
        }
    }

    fn danmaku_search_title(item: &TuItem) -> String {
        item.series_name()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| item.season_name().filter(|name| !name.trim().is_empty()))
            .unwrap_or_else(|| item.name())
    }

    fn danmaku_search_episode(item: &TuItem) -> Option<String> {
        let is_episode = item.season_name().is_some() || item.series_name().is_some();
        is_episode
            .then(|| item.index_number())
            .filter(|episode| *episode > 0)
            .map(|episode| episode.to_string())
    }

    fn danmaku_search_params(
        anime: String, episode: Option<String>, tmdb_id: Option<i32>, tmdb_id_type: i32,
    ) -> SearchSearchEpisodesParams {
        SearchSearchEpisodesParams {
            anime: tmdb_id.is_none().then_some(anime),
            tmdb_id,
            tmdb_id_type,
            episode,
            v2: true,
        }
    }

    pub(super) async fn resolve_danmaku_tmdb_id(&self, item: &TuItem) -> Option<i32> {
        let mut ids = Vec::new();
        if item.item_type() != EPISODE {
            ids.push(("item", item.id()));
        }
        if let Some(season_id) = item.season_id() {
            ids.push(("season", season_id));
        }
        if let Some(series_id) = item.series_id() {
            ids.push(("series", series_id));
        }

        let resolve_started = Instant::now();
        for (lookup_source, id) in ids.into_iter().unique_by(|(_, id)| id.clone()) {
            let lookup_started = Instant::now();
            let lookup_id = id.clone();
            match spawn_tokio(async move { JELLYFIN_CLIENT.get_item_info(&id).await }).await {
                Ok(info) => {
                    if let Some(tmdb_id) = info
                        .provider_ids
                        .and_then(|provider_ids| provider_ids.tmdb)
                        .and_then(|id| id.parse().ok())
                    {
                        tracing::debug!(
                            lookup_source,
                            lookup_id,
                            tmdb_id,
                            elapsed_ms = lookup_started.elapsed().as_millis(),
                            "Jellyfin TMDB lookup matched"
                        );
                        return Some(tmdb_id);
                    }
                    tracing::debug!(
                        lookup_source,
                        lookup_id,
                        elapsed_ms = lookup_started.elapsed().as_millis(),
                        "Jellyfin item has no usable TMDB ID"
                    );
                }
                Err(error) => tracing::debug!(
                    lookup_source,
                    lookup_id,
                    %error,
                    elapsed_ms = lookup_started.elapsed().as_millis(),
                    "Jellyfin TMDB lookup failed"
                ),
            }
        }
        tracing::debug!(
            item_id = item.id(),
            elapsed_ms = resolve_started.elapsed().as_millis(),
            "No Jellyfin TMDB ID resolved"
        );
        None
    }

    fn trace_danmaku_search(
        strategy: &'static str, item_id: &str, generation: u64,
        params: &SearchSearchEpisodesParams, elapsed: Duration,
        result: &anyhow::Result<EpisodeSearchResult>, stale: bool,
    ) {
        match result {
            Ok(result) => tracing::info!(
                flow = "automatic",
                strategy,
                item_id,
                generation,
                anime = ?params.anime.as_deref(),
                episode = ?params.episode.as_deref(),
                tmdb_id = ?params.tmdb_id,
                tmdb_id_type = params.tmdb_id_type,
                anime_count = result.anime_count,
                episode_count = result.episode_count,
                matched = result.episode_match.is_some(),
                stale,
                elapsed_ms = elapsed.as_millis(),
                "Danmaku episode search completed"
            ),
            Err(error) => tracing::warn!(
                flow = "automatic",
                strategy,
                item_id,
                generation,
                anime = ?params.anime.as_deref(),
                episode = ?params.episode.as_deref(),
                tmdb_id = ?params.tmdb_id,
                tmdb_id_type = params.tmdb_id_type,
                %error,
                stale,
                elapsed_ms = elapsed.as_millis(),
                "Danmaku episode search failed"
            ),
        }
    }

    fn should_fallback_danmaku_title(
        tmdb_id: Option<i32>, result: &anyhow::Result<EpisodeSearchResult>, stale: bool,
    ) -> bool {
        tmdb_id.is_some()
            && !stale
            && match result {
                Ok(result) => result.episode_match.is_none(),
                Err(_) => true,
            }
    }

    fn trace_danmaku_load(
        flow: &'static str, item_id: &str, generation: u64, episode_id: i64, elapsed: Duration,
        result: &anyhow::Result<DanmakuLoadResult>, stale: bool,
    ) {
        match result {
            Ok(result) => tracing::info!(
                flow,
                item_id,
                generation,
                episode_id,
                raw_count = result.raw_count,
                count = result.danmaku.len(),
                fetch_elapsed_ms = result.fetch_elapsed.as_millis(),
                convert_elapsed_ms = result.convert_elapsed.as_millis(),
                elapsed_ms = elapsed.as_millis(),
                stale,
                "Danmaku comment request completed"
            ),
            Err(error) => tracing::warn!(
                flow,
                item_id,
                generation,
                episode_id,
                %error,
                elapsed_ms = elapsed.as_millis(),
                stale,
                "Danmaku comment loading failed"
            ),
        }
    }

    fn auto_search_danmaku(&self, client: DanmakuClient, item: &TuItem) {
        if self.has_external_danmaku_source() {
            return;
        }

        if let Some(cached) = DanmakuCacheMap::load().cached_danmaku(item) {
            let expected_configuration_generation = DanmakuClient::configuration_generation();
            let generation = self.imp().danmaku_generation.get();
            let item_id = item.id();
            spawn_g_timeout(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                async move {
                    if expected_configuration_generation
                        != DanmakuClient::configuration_generation()
                        || !obj.is_current_danmaku_request(generation, &item_id)
                        || !obj.imp().danmaku_popover_content.enabled()
                        || obj.has_external_danmaku_source()
                    {
                        return;
                    }
                    if let Err(error) = obj
                        .apply_danmaku_episode(
                            client,
                            cached.episode_id,
                            cached.item_name,
                            true,
                            "cached",
                            Some(expected_configuration_generation),
                        )
                        .await
                    {
                        tracing::warn!(%error, "Failed to apply cached danmaku");
                    }
                }
            ));
            return;
        }

        let generation = self.next_danmaku_generation();
        let item_id = item.id();
        let anime = Self::danmaku_search_title(item);
        let episode = Self::danmaku_search_episode(item);
        let tmdb_id_type = i32::from(item.item_type() == MOVIE);
        let item = item.clone();

        self.clear_danmaku(DanmakuPopoverStatus::Searching);

        spawn_g_timeout(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let tmdb_id = obj.resolve_danmaku_tmdb_id(&item).await;
                if !obj.is_current_danmaku_request(generation, &item_id) {
                    return;
                }

                if !obj.is_current_danmaku_request(generation, &item_id) {
                    return;
                }

                let search_client = client.clone();
                let params = Self::danmaku_search_params(
                    anime.clone(),
                    episode.clone(),
                    tmdb_id,
                    tmdb_id_type,
                );
                let search_started = Instant::now();
                let trace_params = params.clone();
                let mut search_result =
                    spawn_tokio(async move { search_client.search_episode(params).await }).await;
                let stale = !obj.is_current_danmaku_request(generation, &item_id);
                Self::trace_danmaku_search(
                    if tmdb_id.is_some() { "tmdb" } else { "title" },
                    &item_id,
                    generation,
                    &trace_params,
                    search_started.elapsed(),
                    &search_result,
                    stale,
                );

                if Self::should_fallback_danmaku_title(tmdb_id, &search_result, stale) {
                    let search_client = client.clone();
                    let fallback_params =
                        Self::danmaku_search_params(anime, episode, None, tmdb_id_type);
                    let search_started = Instant::now();
                    let trace_params = fallback_params.clone();
                    search_result =
                        spawn_tokio(
                            async move { search_client.search_episode(fallback_params).await },
                        )
                        .await;
                    Self::trace_danmaku_search(
                        "title_fallback",
                        &item_id,
                        generation,
                        &trace_params,
                        search_started.elapsed(),
                        &search_result,
                        !obj.is_current_danmaku_request(generation, &item_id),
                    );
                }

                if !obj.is_current_danmaku_request(generation, &item_id) {
                    return;
                }

                let (episode_id, item_name) = match search_result {
                    Ok(result) => match result.episode_match {
                        Some(episode_match) => episode_match,
                        None => {
                            obj.clear_danmaku(DanmakuPopoverStatus::NoMatching);
                            return;
                        }
                    },
                    Err(_) => {
                        obj.clear_danmaku(DanmakuPopoverStatus::Unavailable);
                        return;
                    }
                };

                obj.imp()
                    .danmaku_popover_content
                    .set_status(DanmakuPopoverStatus::Loading);

                let load_started = Instant::now();
                let comments_result =
                    spawn_tokio(async move { client.get_comments(episode_id).await }).await;
                let stale = !obj.is_current_danmaku_request(generation, &item_id);
                Self::trace_danmaku_load(
                    "automatic",
                    &item_id,
                    generation,
                    episode_id,
                    load_started.elapsed(),
                    &comments_result,
                    stale,
                );

                if stale {
                    return;
                }

                match comments_result {
                    Ok(result) if !result.danmaku.is_empty() => {
                        obj.apply_danmaku(result.danmaku, item_name, false)
                    }
                    Ok(_) => {
                        obj.clear_danmaku(DanmakuPopoverStatus::NoMatching);
                    }
                    Err(_) => {
                        obj.clear_danmaku(DanmakuPopoverStatus::Unavailable);
                    }
                }
            }
        ));
    }

    fn sync_danmaku_position(&self, time_millis: f64) {
        let imp = self.imp();
        imp.danmakw.start_clock();
        imp.danmakw.preroll_seek(time_millis);
        if self.paused() || imp.loading_box.is_visible() {
            imp.danmakw.pause_clock();
        }
    }

    fn resume_danmaku_after_seek(&self, time_millis: f64) {
        let imp = self.imp();
        self.sync_danmaku_position(time_millis);

        if !self.paused() {
            imp.danmakw.start_rendering();
        }
    }

    fn apply_danmaku(&self, danmaku: Vec<Danmaku>, item_name: String, manual: bool) {
        let imp = self.imp();
        imp.external_danmaku_source.take();
        let count = danmaku.len();
        let timeline = danmaku_timeline(&danmaku);
        imp.danmakw.load_danmaku(danmaku);
        imp.video_scale.set_danmaku_timeline(timeline);
        imp.danmaku_count.set(count);
        let status = if manual {
            DanmakuPopoverStatus::ManualLoaded(count, item_name)
        } else {
            DanmakuPopoverStatus::Loaded(count, item_name)
        };
        imp.danmaku_popover_content.set_status(status);
        self.update_danmaku_rendering(imp.danmaku_popover_content.enabled());
    }

    pub async fn apply_manual_danmaku(
        &self, client: DanmakuClient, expected_item_id: &str,
        expected_configuration_generation: u64, episode_id: i64, item_name: String,
    ) -> anyhow::Result<bool> {
        if expected_configuration_generation != DanmakuClient::configuration_generation() {
            anyhow::bail!("The danmaku source changed before loading danmaku");
        }
        if self
            .current_video()
            .as_ref()
            .is_none_or(|item| item.id() != expected_item_id)
        {
            anyhow::bail!("The playing video changed before loading danmaku");
        }
        self.apply_danmaku_episode(
            client,
            episode_id,
            item_name,
            true,
            "manual",
            Some(expected_configuration_generation),
        )
        .await
    }

    async fn apply_danmaku_episode(
        &self, client: DanmakuClient, episode_id: i64, item_name: String, manual: bool,
        flow: &'static str, expected_configuration_generation: Option<u64>,
    ) -> anyhow::Result<bool> {
        let Some(current_item) = self.current_video() else {
            anyhow::bail!("No video is currently playing");
        };
        let item_id = current_item.id();
        let transaction = ManualDanmakuTransaction::new(
            self.imp().external_danmaku_source.borrow().clone(),
            self.has_danmaku(),
        );
        let generation = self.next_danmaku_generation();
        if !transaction.preserve_renderer() {
            self.clear_danmaku(DanmakuPopoverStatus::Loading);
        }

        let load_started = Instant::now();
        let comments_result =
            spawn_tokio(async move { client.get_comments(episode_id).await }).await;
        let request_stale = !self.is_current_danmaku_request(generation, &item_id);
        let source_stale = expected_configuration_generation
            .is_some_and(|expected| expected != DanmakuClient::configuration_generation());
        let stale = request_stale || source_stale;
        Self::trace_danmaku_load(
            flow,
            &item_id,
            generation,
            episode_id,
            load_started.elapsed(),
            &comments_result,
            stale,
        );

        if stale {
            if source_stale && !request_stale {
                self.restore_danmaku_after_manual_failure(
                    &transaction,
                    DanmakuPopoverStatus::Unavailable,
                );
            }
            if source_stale {
                anyhow::bail!("The danmaku source changed while loading danmaku");
            }
            anyhow::bail!("The current video changed while loading danmaku");
        }

        match comments_result {
            Ok(result) if !result.danmaku.is_empty() => {
                self.apply_danmaku(result.danmaku, item_name, manual);
                Ok(true)
            }
            Ok(_) => {
                self.restore_danmaku_after_manual_failure(
                    &transaction,
                    DanmakuPopoverStatus::NoMatching,
                );
                Ok(false)
            }
            Err(error) => {
                self.restore_danmaku_after_manual_failure(
                    &transaction,
                    DanmakuPopoverStatus::Unavailable,
                );
                Err(error)
            }
        }
    }

    fn restore_danmaku_after_manual_failure(
        &self, transaction: &ManualDanmakuTransaction, fallback_status: DanmakuPopoverStatus,
    ) {
        if transaction.preserve_renderer() {
            return;
        }

        if let Some(source) = transaction.external_source_to_restore()
            && self.imp().external_danmaku_source.borrow().as_deref() == Some(source)
        {
            self.imp().external_danmaku_source.take();
            self.load_external_danmaku_track(Some(DanmakuTrack {
                external_url: source.to_owned(),
            }));
            return;
        }

        self.clear_danmaku(fallback_status);
    }

    fn clear_danmaku(&self, status: DanmakuPopoverStatus) {
        let imp = self.imp();
        imp.danmaku_count.set(0);
        imp.danmakw.clear_danmaku();
        imp.danmakw.stop_rendering();
        imp.video_scale.set_danmaku_timeline(Vec::new());
        imp.danmaku_popover_content.set_status(status);
    }

    fn new_danmaku_client(&self) -> Option<DanmakuClient> {
        match DanmakuClient::new() {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(%error, "Failed to initialize danmaku client");
                self.clear_danmaku(DanmakuPopoverStatus::SecretNotExist);
                None
            }
        }
    }

    pub fn danmaku_source_changed(&self) {
        if self.has_external_danmaku_source() {
            return;
        }

        let enabled = self.imp().danmaku_popover_content.enabled();
        self.next_danmaku_generation();
        self.clear_danmaku(if enabled {
            DanmakuPopoverStatus::Searching
        } else {
            DanmakuPopoverStatus::Disabled
        });
        if enabled
            && let Some(item) = self.current_video()
            && let Some(client) = self.new_danmaku_client()
        {
            self.auto_search_danmaku(client, &item);
        }
    }

    fn update_danmaku_rendering(&self, enabled: bool) {
        let imp = self.imp();
        let ready = enabled && self.has_danmaku() && imp.file_loaded.get();
        imp.danmakw.set_visible(ready);
        if !ready {
            imp.danmaku_sync.reset();
            imp.danmakw.stop_rendering();
            return;
        }
        self.sync_danmaku_position(imp.last_playback_position.get() * 1000.0);
        if !self.paused() && !imp.loading_box.is_visible() {
            imp.danmakw.start_rendering();
        } else {
            imp.danmakw.stop_rendering();
        }
    }

    pub fn on_danmaku_switch_state_set(&self, state: bool) {
        if state && !self.has_danmaku() {
            if let Some(item) = self.current_video()
                && let Some(client) = self.new_danmaku_client()
            {
                self.auto_search_danmaku(client, &item);
            }
            return;
        }

        if !state && !self.has_danmaku() {
            self.next_danmaku_generation();
            self.imp().external_danmaku_source.take();
            self.clear_danmaku(DanmakuPopoverStatus::Idle);
        }

        self.update_danmaku_rendering(state);
    }

    fn has_danmaku(&self) -> bool {
        self.imp().danmaku_count.get() > 0
    }

    pub fn danmakw(&self) -> Danmakw {
        self.imp().danmakw.get()
    }

    fn mark_stream_failed(&self) {
        let imp = self.imp();
        imp.file_loaded.set(false);
        imp.media_source_fallback.take();
        imp.danmaku_sync.reset();
        imp.danmakw.stop_rendering();
        imp.danmakw.set_visible(false);
        imp.playback_loads.borrow_mut().take_active();
        imp.cached_playback.take();
        imp.loading_box.set_visible(false);
        imp.spinner.set_visible(false);
        self.next_danmaku_generation();
        imp.external_danmaku_source.take();
        self.clear_danmaku(DanmakuPopoverStatus::Idle);
    }

    fn fail_playback(&self, message: impl Into<String>) {
        self.mark_stream_failed();
        self.toast(message);
        self.notify_stopped();
    }

    fn begin_play_request(&self) -> u64 {
        let generation = self.imp().play_generation.get().wrapping_add(1);
        self.imp().play_generation.set(generation);
        generation
    }

    fn request_is_current(&self, generation: u64) -> bool {
        self.imp().play_generation.get() == generation
    }

    fn cancel_pending_playback(&self) {
        self.begin_play_request();
    }

    pub fn play(
        &self, selected: Option<SelectedVideoSubInfo>, item: TuItem, episode_list: Vec<TuItem>,
        video_matcher: Option<String>, start_seconds: f64,
    ) {
        let video_changed = self
            .current_video()
            .as_ref()
            .is_none_or(|current| current.id() != item.id());
        let imp = self.imp();
        imp.file_loaded.set(false);
        imp.media_source_fallback.take();
        imp.danmaku_sync.reset();
        imp.danmakw.stop_rendering();
        imp.danmakw.set_visible(false);
        let (title1, title2) = if let Some(series_name) = item.series_name() {
            let episode_info = format!(
                "S{}E{}: {}",
                item.parent_index_number(),
                item.index_number(),
                item.name()
            );
            (series_name, Some(episode_info))
        } else {
            (item.name(), None)
        };

        self.imp().title_label1.set_text(&title1);
        self.imp()
            .title_label2
            .set_text(title2.as_deref().unwrap_or_default());
        self.imp().title_label2.set_visible(title2.is_some());

        let media_title = title2
            .map(|t| format!("{title1} - {t}"))
            .unwrap_or_else(|| title1);

        self.imp()
            .video
            .set_property("force-media-title", media_title);

        let id = item.id();
        let series_id = item.series_id();

        // If the video_matcher is None, field wont be updated
        if let Some(video_matcher) = video_matcher {
            self.imp()
                .video_version_matcher
                .replace(Some(video_matcher));
        }

        let cache_key = PlaybackCacheKey::new(
            PlaybackCacheScope::current(),
            id.clone(),
            selected.as_ref(),
            self.imp().video_version_matcher.borrow().clone(),
        );
        let generation = self.begin_play_request();
        let resume_cached = self.imp().cached_playback.borrow().as_ref() == Some(&cache_key);
        let should_refresh_danmaku = should_refresh_danmaku(
            video_changed,
            self.imp().cached_playback.borrow().as_ref(),
            &cache_key,
        );

        if should_refresh_danmaku {
            self.next_danmaku_generation();
            imp.external_danmaku_source.take();
            self.clear_danmaku(DanmakuPopoverStatus::Idle);
        }
        self.imp().video_scale.reset_scale();
        self.imp().last_playback_position.set(start_seconds);
        self.reset_skippable_segments();

        let track_list_changed = self.mpris_track_list_changed(&episode_list);

        self.set_current_video(Some(item.clone()));
        self.imp().current_episode_list.replace(episode_list);

        {
            self.imp().mpris_art_url.take();
            if track_list_changed {
                self.notify_track_list_replaced();
            }
        }
        self.notify_track_changed();
        if should_refresh_danmaku && let Some(client) = self.new_danmaku_client() {
            if SETTINGS.mpv_danmaku_enabled() {
                self.auto_search_danmaku(client, &item);
            } else {
                self.next_danmaku_generation();
                self.imp().external_danmaku_source.take();
                self.clear_danmaku(DanmakuPopoverStatus::Idle);
            }
        }
        self.load_skippable_segments(id.to_owned());

        if resume_cached {
            let imp = self.imp();
            imp.playback_loads
                .borrow_mut()
                .resume_cached(PlaybackRequest {
                    generation,
                    cache_key: cache_key.clone(),
                });
            imp.file_loaded.set(true);
            imp.spinner.set_visible(false);
            imp.loading_box.set_visible(false);
            imp.video.resume_cached(start_seconds);
            self.update_danmaku_rendering(imp.danmaku_popover_content.enabled());

            if !should_refresh_danmaku
                && SETTINGS.mpv_danmaku_enabled()
                && !self.has_danmaku()
                && let Some(client) = self.new_danmaku_client()
            {
                self.auto_search_danmaku(client, &item);
            }

            self.notify_playing();
            self.update_timeout();
            self.handle_callback(BackType::Start);
            return;
        }

        self.imp().cached_playback.take();
        self.imp().suburl.take();
        self.imp().video.player().push_an_empty_texture();
        self.imp().video.stop();

        spawn_g_timeout(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                if !obj.request_is_current(generation) {
                    return;
                }
                let imp = obj.imp();
                imp.spinner.set_visible(true);
                imp.loading_box.set_visible(true);
                imp.network_speed_label
                    .set_text(&gettext("Initializing..."));

                let sub_stream_index = selected.as_ref().map(|s| s.sub_index);
                let media_source_id = selected.as_ref().map(|s| s.media_source_id.clone());
                let id_clone = id.to_owned();
                let playback_info = spawn_tokio(async move {
                    JELLYFIN_CLIENT
                        .get_playbackinfo(&id_clone, sub_stream_index, media_source_id, true)
                        .await
                })
                .await;
                if !obj.request_is_current(generation) {
                    return;
                }
                let playback_info = match playback_info {
                    Ok(playback_info) => playback_info,
                    Err(e) => {
                        obj.fail_playback(e.to_user_facing());
                        return;
                    }
                };

                let media_source =
                    if let Some(video_stream_index) = selected.as_ref().map(|s| s.video_index) {
                        playback_info.media_sources.get(video_stream_index as usize)
                    } else {
                        let video_version_list: Vec<_> = playback_info
                            .media_sources
                            .iter()
                            .map(|media_source| media_source.name.to_owned())
                            .collect();

                        if let Some(matcher) = imp.video_version_matcher.borrow().as_ref() {
                            make_video_version_choice_from_matcher(video_version_list, matcher)
                                .and_then(|index| playback_info.media_sources.get(index))
                        } else {
                            playback_info.media_sources.first()
                        }
                    };

                let Some(media_source) = media_source else {
                    obj.fail_playback(gettext("No media source found"));
                    return;
                };

                let back = Back {
                    id: id.to_owned(),
                    series_id,
                    playsessionid: playback_info.play_session_id.to_owned(),
                    mediasourceid: media_source.id.to_owned(),
                    livestreamid: media_source.live_stream_id.to_owned(),
                    playmethod: "DirectPlay",
                    tick: media_source.run_time_ticks.unwrap_or(0),
                    start_tick: glib::real_time() as u64 * 10,
                };

                let media_stream =
                    if let Some(sub_stream_index) = selected.as_ref().map(|s| s.sub_index) {
                        media_source.media_streams.get(sub_stream_index as usize)
                    } else {
                        let sub_version_list: Vec<_> = media_source
                            .media_streams
                            .iter()
                            .filter(|stream| stream.stream_type == "Subtitle")
                            .map(|stream| {
                                (
                                    stream.index,
                                    stream.display_title.to_owned().unwrap_or_default(),
                                )
                            })
                            .collect();

                        make_subtitle_version_choice(sub_version_list)
                            .and_then(|index| media_source.media_streams.get(index.0 as usize))
                    };

                let slang = selected
                    .as_ref()
                    .map(|selected| selected.sub_lang.clone())
                    .unwrap_or_else(|| SETTINGS.mpv_subtitle_preferred_lang_str());

                let sub_url = match media_stream {
                    Some(stream) if stream.is_external => match &stream.delivery_url {
                        Some(url) => Some(JELLYFIN_CLIENT.resolve_url(url)),
                        None => {
                            println!("External Subtitle without selected source");
                            imp.obj()
                                .external_sub_url_without_selected_source(
                                    id.to_owned(),
                                    stream,
                                    media_source.id.to_owned(),
                                )
                                .await
                        }
                    },
                    _ => None,
                };
                if !obj.request_is_current(generation) {
                    return;
                }

                let Some((primary_url, fallback)) =
                    media_source_url(media_source, playback_info.play_session_id.as_deref())
                else {
                    obj.fail_playback(gettext("No media source found"));
                    return;
                };
                if !obj.request_is_current(generation) {
                    return;
                }

                imp.back.replace(Some(back));
                imp.video.set_slang(slang);
                imp.suburl.replace(sub_url);
                imp.media_source_fallback.replace(fallback);
                imp.playback_loads.borrow_mut().issued(PlaybackRequest {
                    generation,
                    cache_key,
                });
                imp.video.play(&primary_url, start_seconds);
            }
        ));
    }

    fn reset_skippable_segments(&self) {
        let imp = self.imp();
        imp.skippable_segments.replace(None);
        imp.current_segment_end.set(None);
        imp.skip_segment_revealer.set_reveal_child(false);
    }

    fn load_skippable_segments(&self, id: String) {
        spawn_g_timeout(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let request_id = id.to_owned();
                let Ok(segments) = spawn_tokio(async move {
                    JELLYFIN_CLIENT.get_skippable_segments(&request_id).await
                })
                .await
                else {
                    return;
                };
                let segments = segments
                    .items
                    .into_iter()
                    .filter(|s| {
                        s.end_ticks > s.start_ticks
                            && matches!(
                                s.segment_type,
                                MediaSegmentType::Intro | MediaSegmentType::Outro
                            )
                    })
                    .sorted_by_key(|s| s.start_ticks)
                    .collect::<Vec<_>>();
                if obj
                    .current_video()
                    .as_ref()
                    .is_none_or(|item| item.id() != id)
                {
                    return;
                }
                let imp = obj.imp();
                imp.skippable_segments.replace(Some(segments));
                obj.update_skip_segment_button(imp.video.position());
            }
        ));
    }

    fn update_skip_segment_button(&self, position: f64) {
        let current_segment =
            self.imp()
                .skippable_segments
                .borrow()
                .as_ref()
                .and_then(|segments| {
                    segments.iter().find_map(|segment| {
                        let start = segment.start_seconds();
                        let end = segment.end_seconds();
                        (position >= start && position + 0.5 < end)
                            .then_some((segment.segment_type, end))
                    })
                });
        let segment_end = current_segment.map(|(_, end)| end);
        self.imp().current_segment_end.set(segment_end);

        if segment_end.is_some() && SETTINGS.auto_skip_intro_outro() {
            return self.on_skip_segment_clicked();
        }

        if let Some((kind, _)) = current_segment {
            let label = match kind {
                MediaSegmentType::Intro => gettext("Skip Intro"),
                MediaSegmentType::Outro => gettext("Skip Outro"),
                _ => unreachable!(),
            };
            self.imp().skip_segment_button.set_label(&label);
        }
        self.imp()
            .skip_segment_revealer
            .set_reveal_child(segment_end.is_some());
    }

    #[template_callback]
    fn on_skip_segment_clicked(&self) {
        let Some(end) = self.imp().current_segment_end.get() else {
            return;
        };

        self.imp().video.set_position(end);
        self.imp().video_scale.set_value(end);
        self.imp().last_playback_position.set(end);
        self.imp().skip_segment_revealer.set_reveal_child(false);
        self.handle_callback(BackType::Back);
    }

    async fn external_sub_url_without_selected_source(
        &self, id: String, media_stream: &MediaStream, media_source_id: String,
    ) -> Option<String> {
        let stream_index = media_stream.index;
        let media_source_id_clone = media_source_id.to_owned();
        let playback_info = spawn_tokio(async move {
            JELLYFIN_CLIENT
                .get_playbackinfo(&id, Some(stream_index), Some(media_source_id), true)
                .await
        })
        .await
        .ok()?;

        let media_source = playback_info
            .media_sources
            .iter()
            .find(|source| source.id == media_source_id_clone);

        let url = media_source?
            .media_streams
            .get(stream_index as usize)?
            .delivery_url
            .to_owned()?;

        Some(JELLYFIN_CLIENT.resolve_url(&url))
    }

    async fn set_audio_and_video_tracks_dropdown(&self, value: MpvTracks) {
        let MpvTracks {
            audio_tracks,
            sub_tracks,
            danmaku_track,
        } = value;
        self.load_external_danmaku_track(danmaku_track);
        let imp = self.imp();
        self.bind_tracks(audio_tracks, &imp.audio_listbox.get(), MpvTrackKind::Audio)
            .await;
        self.bind_tracks(sub_tracks, &imp.sub_listbox.get(), MpvTrackKind::Subtitle)
            .await;
    }

    // TODO: Use GAction instead of listening to each button
    async fn bind_tracks(&self, tracks: Vec<MpvTrack>, listbox: &gtk::ListBox, kind: MpvTrackKind) {
        while let Some(row) = listbox.first_child() {
            listbox.remove(&row);
        }

        let track_id = self.imp().video.get_track_id(kind.track_kind()).await;

        let row = CheckRow::new();
        row.set_title("None");
        if track_id == 0 {
            row.imp().check.get().set_active(true);
        }
        let none_check = &row.imp().check.get();
        row.connect_activated(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            move |_| {
                obj.set_track(kind, 0);
            }
        ));
        listbox.append(&row);

        for track in tracks {
            let row = CheckRow::new();
            row.set_title(&track.title);
            row.set_subtitle(&track.lang);
            row.imp().track_id.replace(track.id);
            let check = &row.imp().check.get();
            check.set_group(Some(none_check));
            if track.id == track_id {
                check.set_active(true);
            }
            row.connect_activated(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                move |_| {
                    obj.set_track(kind, track.id);
                }
            ));
            listbox.append(&row);
        }
    }

    fn set_track(&self, kind: MpvTrackKind, track_id: i64) {
        let track = if track_id == 0 {
            TrackSelection::None
        } else {
            TrackSelection::Track(track_id)
        };

        self.imp()
            .video
            .set_property(kind.property(), track.to_string());
    }

    async fn load_video(&self, offset: isize) {
        if self.paused() {
            self.imp().video.pause();
        }

        let Some(current_video) = self.imp().current_video.borrow().to_owned() else {
            return;
        };

        let next_item = {
            let video_list = self.imp().current_episode_list.borrow();
            video_list.iter().enumerate().find_map(|(i, item)| {
                // Don't use id() here, because the same video maybe have different id
                if item.index_number() == current_video.index_number()
                    && item.parent_index_number() == current_video.parent_index_number()
                {
                    let new_index = (i as isize + offset) as usize;
                    video_list.get(new_index).cloned()
                } else {
                    None
                }
            })
        };

        let Some(next_item) = next_item else {
            self.toast(gettext("No more video found"));
            self.on_stop_clicked();
            return;
        };

        self.in_play_item(next_item).await;
    }

    pub async fn in_play_item(&self, item: TuItem) {
        self.handle_callback(BackType::Stop);
        let episode_list = self.imp().current_episode_list.borrow().clone();
        self.play(None, item, episode_list, None, 0.0);
    }

    pub async fn on_next_video(&self) {
        self.load_video(1).await;
    }

    pub async fn on_previous_video(&self) {
        self.load_video(-1).await;
    }

    #[template_callback]
    fn on_progress_value_changed(&self, progress_scale: &VideoScale) {
        let label = &self.imp().progress_time_label.get();
        let position = progress_scale.value();
        label.set_text(&format_duration(position as i64));
    }

    #[template_callback]
    fn on_info_clicked(&self) {
        let mpv = &self.imp().video;
        mpv.display_stats_toggle();
    }

    fn listen_events(&self) {
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                while let Ok(value) = MPV_EVENT_CHANNEL.rx.recv_async().await {
                    match value {
                        ListenEvent::Duration(value) => {
                            obj.update_duration(value);
                        }
                        ListenEvent::DemuxerCacheIdle(_) => {}
                        ListenEvent::PausedForCache(true, time_millis)
                        | ListenEvent::Seek(time_millis) => {
                            obj.update_seeking(true, time_millis);
                        }
                        ListenEvent::PausedForCache(false, time_millis)
                        | ListenEvent::PlaybackRestart(time_millis) => {
                            let was_seeking = obj.get_seeking();
                            obj.update_seeking(false, time_millis);
                            if was_seeking {
                                obj.handle_callback(BackType::Back);
                                obj.notify_seeked((time_millis / 1000.0) as i64);
                            }
                        }
                        ListenEvent::Eof(value) => {
                            obj.on_end_file(value);
                        }
                        ListenEvent::PlaybackEnded => {
                            obj.on_end_file(0);
                        }
                        ListenEvent::Error(value) => {
                            obj.on_error(&value);
                        }
                        ListenEvent::Pause(value) => {
                            obj.imp().video.update_paused(value);
                            obj.on_pause_update(value);
                        }
                        ListenEvent::CacheSpeed(value) => {
                            obj.on_cache_speed_update(value);
                        }
                        ListenEvent::FileLoaded => {
                            obj.on_file_loaded();
                        }
                        ListenEvent::StartFile => {
                            obj.on_start_file();
                        }
                        ListenEvent::TrackList(value) => {
                            obj.set_audio_and_video_tracks_dropdown(value).await;
                        }
                        ListenEvent::Volume(value) => {
                            obj.volume_cb(value);
                        }
                        ListenEvent::Speed(value) => {
                            obj.speed_cb(value);
                        }
                        ListenEvent::Shutdown => {
                            obj.on_shutdown();
                        }
                        ListenEvent::DemuxerCacheTime(value) => {
                            obj.on_cache_time_update(value);
                        }
                        ListenEvent::TimePos(value) => {
                            obj.scale_cb(value);
                        }
                        ListenEvent::ChapterList(value) => {
                            obj.on_chapter_list(value);
                        }
                        ListenEvent::Playlist(_) => {}
                    }
                }
            }
        ));
    }

    fn on_shutdown(&self) {
        close_on_error!(
            self,
            gettext("MPV has been shutdown, Application will exit.\nTsukimi can't restart MPV.",)
        );
    }

    fn on_cache_time_update(&self, value: i64) {
        self.imp().video_scale.set_cache_end_time(value);
    }

    fn on_chapter_list(&self, value: ChapterList) {
        self.imp().video_scale.set_chapter_list(value);
    }

    fn update_duration(&self, value: f64) {
        let imp = self.imp();
        let duration = format_duration(value as i64);
        let width_chars = duration.chars().count() as i32;
        imp.video_scale.set_range(0.0, value);
        imp.video_scale.refresh_danmaku_distribution();
        imp.progress_time_label.set_width_chars(width_chars);
        imp.duration_label.set_width_chars(width_chars);
        imp.duration_label.set_text(&duration);
    }

    fn speed_cb(&self, value: f64) {
        let imp = self.imp();
        imp.playback_speed_adj.set_value(value);
        imp.playback_speed_button_content
            .set_label(&format!("{value:.2}x"));
        imp.playback_speed_indicator
            .set_visible((value * 100.0).round() as i64 != 100);
        if let Some(window) = self.root().and_downcast_ref::<Window>() {
            window.imp().mpv_control_sidebar.set_playback_speed(value);
        }
    }

    fn volume_cb(&self, value: i64) {
        let imp = self.imp();
        imp.volume_adj.set_value(value as f64);
        imp.volume_bar.set_level(value as f64 / 100.0);
        if value > 0 {
            imp.last_nonzero_volume.set(value);
        }
        self.update_volume_button(value);
        self.notify_volume_changed(value as f64 / 100.0);
    }

    fn scale_cb(&self, value: i64) {
        let imp = self.imp();
        imp.video.update_position(value as f64);
        imp.last_playback_position.set(value as f64);
        if !imp.video_scale.is_dragging() {
            imp.video_scale.set_value(value as f64);
        }
        self.update_skip_segment_button(value as f64);

        if let Some(time_millis) = imp
            .danmaku_sync
            .observe_position(value as f64 * 1000.0, imp.seeking.get())
        {
            self.resume_danmaku_after_seek(time_millis);
        }
    }

    #[template_callback]
    fn video_scroll_cb(&self, _: f64, dy: f64) -> bool {
        self.imp().video.volume_scroll(-dy as i64 * 5);
        true
    }

    #[template_callback]
    fn on_volume_scale_value_changed(&self, btn: &gtk::Scale) {
        let imp = self.imp();
        imp.video.set_volume(btn.value() as i64);
    }

    #[template_callback]
    fn on_volume_button_clicked(&self) {
        let imp = self.imp();
        let volume = imp.volume_adj.value().round() as i64;
        if volume > 0 {
            imp.last_nonzero_volume.set(volume);
            imp.video.set_volume(0);
        } else {
            imp.video.set_volume(imp.last_nonzero_volume.get().max(1));
        }
    }

    #[template_callback]
    fn playback_speed_indicator_cb(&self, _: &gtk::Button) {
        let imp = self.imp();
        imp.video.set_speed(1.0);
    }

    fn update_volume_button(&self, value: i64) {
        let imp = self.imp();
        let icon_name = match value {
            0 => "audio-volume-muted-symbolic",
            value if value < 33 => "audio-volume-low-symbolic",
            value if value < 66 => "audio-volume-medium-symbolic",
            _ => "audio-volume-high-symbolic",
        };
        imp.volume_button.set_icon_name(icon_name);
    }

    fn on_file_loaded(&self) {
        let imp = self.imp();
        let cache_key = imp
            .playback_loads
            .borrow()
            .file_loaded(imp.play_generation.get())
            .cloned();
        let Some(cache_key) = cache_key else {
            tracing::debug!("Ignoring file-loaded event for a stale playback request");
            return;
        };
        imp.cached_playback.replace(Some(cache_key));
        imp.file_loaded.set(true);
        imp.media_source_fallback.take();

        if imp.danmaku_popover_content.enabled() && self.has_danmaku() {
            imp.danmakw.set_visible(true);
        }

        if let Some(suburl) = imp.suburl.borrow().as_ref() {
            imp.video.add_sub(suburl);
        }
        self.notify_playing();
        self.update_timeout();
        self.handle_callback(BackType::Start);
    }

    fn on_start_file(&self) {
        let imp = self.imp();
        let generation = imp
            .playback_loads
            .borrow_mut()
            .start_file()
            .map(|request| request.generation);
        if generation.is_none() {
            tracing::debug!("Ignoring start-file event without an issued playback request");
        }
    }

    fn update_seeking(&self, seeking: bool, time_millis: f64) {
        let imp = self.imp();
        let was_seeking = imp.seeking.replace(seeking);
        imp.loading_box.set_visible(seeking);
        imp.spinner.set_visible(seeking);

        if !imp.file_loaded.get() || !imp.danmaku_popover_content.enabled() {
            return;
        }

        if seeking {
            if !was_seeking {
                imp.danmaku_sync.begin_seek();
            }
            imp.danmakw.set_paused(true);
            return;
        }

        if let Some(position) = imp.danmaku_sync.finish_seek(time_millis) {
            self.resume_danmaku_after_seek(position);
        }
    }

    fn get_seeking(&self) -> bool {
        self.imp().seeking.get()
    }

    fn on_end_file(&self, value: u32) {
        let imp = self.imp();
        let ended = imp.playback_loads.borrow_mut().take_active();
        let ended_is_current = ended
            .as_ref()
            .is_some_and(|ended| ended.generation == imp.play_generation.get());
        if ended_is_current {
            let ended = ended.expect("current playback request is present");
            imp.file_loaded.set(false);
            if imp.cached_playback.borrow().as_ref() == Some(&ended.cache_key) {
                imp.cached_playback.take();
                self.next_danmaku_generation();
                imp.external_danmaku_source.take();
                self.clear_danmaku(DanmakuPopoverStatus::Idle);
            }
        }

        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                if value == 0 && ended_is_current {
                    match SETTINGS.mpv_action_after_video_end() {
                        0 => obj.on_next_video().await,
                        2 => obj.on_stop_clicked(),
                        _ => {}
                    }
                }
            }
        ));
    }

    fn on_error(&self, value: &str) {
        let imp = self.imp();
        let Some(failed) = imp.playback_loads.borrow_mut().take_active() else {
            tracing::debug!("Ignoring playback error without an active playback request");
            return;
        };
        if failed.generation != imp.play_generation.get() {
            tracing::debug!("Ignoring playback error for a stale playback request");
            return;
        }

        if value.contains("-13")
            && let Some(fallback) = imp.media_source_fallback.take()
        {
            tracing::info!("Direct media path failed; retrying through the media server");
            match fallback {
                MediaSourceFallback::DirectPlay(url) => {
                    imp.playback_loads.borrow_mut().issued(failed);
                    imp.video.play(&url, imp.last_playback_position.get());
                }
                MediaSourceFallback::PlaybackInfo => {
                    let error = value.to_owned();
                    spawn(glib::clone!(
                        #[weak(rename_to = obj)]
                        self,
                        async move {
                            obj.retry_fallback(error, failed).await;
                        }
                    ));
                }
            }
            return;
        }
        self.fail_playback(value);
    }

    async fn retry_fallback(&self, original_error: String, request: PlaybackRequest) {
        let generation = request.generation;
        let Some(back) = self.imp().back.borrow().as_ref().cloned() else {
            self.fail_playback(&original_error);
            return;
        };
        let item_id = back.id;
        let requested_item_id = item_id.to_owned();
        let media_source_id = back.mediasourceid;
        let requested_media_source_id = media_source_id.to_owned();
        let live_stream_id = back.livestreamid;
        let playback_info = spawn_tokio(async move {
            JELLYFIN_CLIENT
                .get_fallback_playbackinfo(&item_id, &media_source_id, live_stream_id.as_deref())
                .await
        })
        .await;
        if !self.request_is_current(generation)
            || self
                .current_video()
                .as_ref()
                .is_none_or(|item| item.id() != requested_item_id)
        {
            return;
        }
        let playback_info = match playback_info {
            Ok(playback_info) => playback_info,
            Err(error) => {
                tracing::warn!(%error, "Fallback playback negotiation failed");
                self.fail_playback(error.to_user_facing());
                return;
            }
        };
        let Some(media_source) = playback_info
            .media_sources
            .into_iter()
            .find(|source| source.id == requested_media_source_id)
        else {
            self.fail_playback(&original_error);
            return;
        };
        let fallback = media_source
            .transcoding_url
            .as_deref()
            .map(|url| (url, "Transcode"))
            .or_else(|| {
                media_source
                    .direct_stream_url
                    .as_deref()
                    .map(|url| (url, "DirectStream"))
            });
        let Some((fallback_url, playmethod)) = fallback else {
            self.fail_playback(&original_error);
            return;
        };
        if let Some(back) = self.imp().back.borrow_mut().as_mut() {
            back.playsessionid = playback_info.play_session_id;
            back.mediasourceid = media_source.id;
            back.livestreamid = media_source.live_stream_id;
            back.playmethod = playmethod;
        }
        self.imp().playback_loads.borrow_mut().issued(request);
        self.imp()
            .video
            .play(fallback_url, self.imp().last_playback_position.get());
    }

    pub fn on_pause_update(&self, value: bool) {
        if !value {
            self.update_timeout();
            self.notify_playing();
        } else {
            self.remove_timeout();
            self.notify_player_paused();
        }

        self.set_paused(value);
    }

    fn on_cache_speed_update(&self, value: i64) {
        let label = &self.imp().network_speed_label;
        if value >= 2 * 1024 * 1024 {
            label.set_text(&format!("{:.2} MiB/s", value as f64 / (1024.0 * 1024.0)));
        } else {
            label.set_text(&format!("{} KiB/s", value / 1024));
        }
    }

    #[template_callback]
    fn on_motion(&self, x: f64, y: f64) {
        let imp = self.imp();
        let old_x = imp.x.get();
        let old_y = imp.y.get();

        if old_x == x && old_y == y {
            return;
        }

        imp.x.set(x);
        imp.y.set(y);

        let now = glib::monotonic_time();

        if now - imp.last_motion_time.get() < MIN_MOTION_TIME {
            return;
        }

        let is_threshold = (old_x - x).abs() > 3.0 || (old_y - y).abs() > 3.0;

        if is_threshold {
            if !self.toolbar_revealed() {
                self.set_reveal_overlay(true);
            }

            self.reset_fade_timeout();

            imp.last_motion_time.set(now);
        }
    }

    #[template_callback]
    fn on_leave(&self) {
        let imp = self.imp();
        imp.x.set(-1.0);
        imp.y.set(-1.0);

        if self.toolbar_revealed() && imp.timeout.borrow().is_none() {
            self.reset_fade_timeout();
        }
    }

    #[template_callback]
    fn on_enter(&self) {
        if self.toolbar_revealed() {
            self.reset_fade_timeout();
        } else {
            self.set_reveal_overlay(true);
        }
    }

    fn reset_fade_timeout(&self) {
        let imp = self.imp();
        if let Some(timeout) = imp.timeout.take() {
            glib::source::SourceId::remove(timeout);
        }
        let timeout = glib::timeout_add_seconds_local_once(
            3,
            glib::clone!(
                #[weak(rename_to = obj)]
                self,
                move || {
                    obj.fade_overlay_delay_cb();
                }
            ),
        );
        *imp.timeout.borrow_mut() = Some(timeout);
    }

    fn toolbar_revealed(&self) -> bool {
        self.imp().bottom_revealer.is_child_revealed()
    }

    fn fade_overlay_delay_cb(&self) {
        *self.imp().timeout.borrow_mut() = None;

        let binding = self.ancestor(adw::OverlaySplitView::static_type());
        let Some(view) = binding.and_downcast_ref::<adw::OverlaySplitView>() else {
            return;
        };

        if view.shows_sidebar() {
            return;
        }

        if self.toolbar_revealed() && self.can_fade_overlay() {
            self.set_reveal_overlay(false);
        }
    }

    fn can_fade_overlay(&self) -> bool {
        let imp = self.imp();

        if imp.popover_count.get() > 0 {
            return false;
        }

        let x = imp.x.get();
        let y = imp.y.get();

        if let Some(widget) = self.pick(x, y, gtk::PickFlags::DEFAULT)
            && !widget.is::<MPVPlaySink>()
            && !widget.is::<Danmakw>()
            && widget.ancestor(MPVPlaySink::static_type()).is_none()
            && widget.ancestor(Danmakw::static_type()).is_none()
        {
            return false;
        }

        if let Some(window) = self.root().and_downcast::<gtk::Window>()
            && let Some(focus) = gtk::prelude::GtkWindowExt::focus(&window)
            && focus.ancestor(adw::Dialog::static_type()).is_some()
        {
            return false;
        }

        let binding = self.ancestor(gtk::Stack::static_type());
        let Some(view) = binding.and_downcast_ref::<gtk::Stack>() else {
            return false;
        };

        if view.visible_child_name() != Some("mpv".into()) {
            return false;
        }

        true
    }

    fn set_reveal_overlay(&self, reveal: bool) {
        let imp = self.imp();
        imp.bottom_revealer.set_reveal_child(reveal);
        imp.top_revealer.set_reveal_child(reveal);

        let cursor = if reveal {
            gtk::gdk::Cursor::from_name("default", None)
        } else {
            gtk::gdk::Cursor::from_name("none", None)
        };

        self.set_cursor(cursor.as_ref());
    }

    #[template_callback]
    fn on_play_pause_clicked(&self) {
        let video = &self.imp().video;
        video.pause();
    }

    #[template_callback]
    pub fn on_stop_clicked(&self) {
        let imp = self.imp();
        imp.file_loaded.set(false);
        imp.media_source_fallback.take();
        imp.danmakw.set_visible(false);
        self.remove_timeout();
        self.reset_skippable_segments();
        let current_video = self.current_video();

        let video = &imp.video;
        let preserve_cached_playback = imp.cached_playback.borrow().is_some();
        if !preserve_cached_playback {
            self.next_danmaku_generation();
            imp.external_danmaku_source.take();
            self.clear_danmaku(DanmakuPopoverStatus::Idle);
        }
        self.cancel_pending_playback();
        if preserve_cached_playback {
            video.park_cached();
        } else {
            video.stop();
        }
        let root = self.root();
        let window = root
            .and_downcast_ref::<crate::ui::widgets::window::Window>()
            .unwrap();
        window.imp().stack.set_visible_child_name("main");
        window.allow_suspend();
        window.refresh_homepage_if_needed();

        spawn_g_timeout(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            #[weak]
            window,
            async move {
                if let Some(timeout) = obj.imp().timeout.take() {
                    glib::source::SourceId::remove(timeout);
                }
                obj.set_reveal_overlay(true);
                // Wait for stop progress to land before refreshing item state
                obj.handle_callback_sync(BackType::Stop).await;
                if let Some(current_video) = current_video {
                    window.update_item_page(current_video).await;
                }
            }
        ));
        self.notify_stopped();
    }

    pub fn update_position_callback(&self) -> glib::ControlFlow {
        self.handle_callback(BackType::Back);
        glib::ControlFlow::Continue
    }

    fn position_back(&self) -> Option<Back> {
        let position = self.imp().last_playback_position.get();
        let mut back = self.imp().back.borrow().as_ref()?.to_owned();
        back.tick = position as u64 * 10000000;
        Some(back)
    }

    fn handle_callback(&self, backtype: BackType) {
        if let Some(back) = self.position_back() {
            crate::utils::spawn_tokio_without_await(async move {
                let _ = JELLYFIN_CLIENT.position_back(&back, backtype).await;
            });
        }
    }

    async fn handle_callback_sync(&self, backtype: BackType) {
        if let Some(back) = self.position_back() {
            let _ =
                spawn_tokio(async move { JELLYFIN_CLIENT.position_back(&back, backtype).await })
                    .await;
        }
    }

    pub fn update_timeout(&self) {
        self.remove_timeout();
        let closure = glib::clone!(
            #[weak(rename_to = obj)]
            self,
            move || {
                self.imp()
                    .back_timeout
                    .replace(Some(glib::timeout_add_seconds_local(10, move || {
                        obj.update_position_callback()
                    })));
            }
        );
        closure();
    }

    pub fn remove_timeout(&self) {
        if let Some(timeout) = self.imp().back_timeout.take() {
            glib::source::SourceId::remove(timeout);
        }
    }

    #[template_callback]
    fn right_click_cb(&self, _n: i32, x: f64, y: f64) {
        if let Some(popover) = self.imp().popover.borrow().as_ref() {
            popover.set_pointing_to(Some(&Rectangle::new(x as i32, y as i32, 0, 0)));
            popover.popup();
        };
    }

    #[template_callback]
    fn left_click_cb(&self) {
        self.imp().video.pause();
    }

    #[template_callback]
    fn on_playlist_clicked(&self) {
        let binding = self.root();
        let Some(window) = binding.and_downcast_ref::<Window>() else {
            return;
        };
        window.view_playlist();
    }

    fn on_sidebar_clicked(&self) {
        let binding = self.root();
        let Some(window) = binding.and_downcast_ref::<Window>() else {
            return;
        };
        window.view_control_sidebar();
    }

    fn view(&self) -> Option<adw::OverlaySplitView> {
        self.ancestor(adw::OverlaySplitView::static_type())
            .and_downcast()
    }

    fn window(&self) -> Option<Window> {
        self.root().and_downcast()
    }

    fn toggle_fullscreen(&self) {
        if let Some(window) = self.window() {
            window.toggle_fullscreen();
        }
    }

    pub fn key_pressed_cb(&self, key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
        if key.to_lower() == gtk::gdk::Key::f || key == gtk::gdk::Key::Escape {
            return true;
        }

        let Some(view) = self.view() else {
            return false;
        };

        if view.shows_sidebar() {
            return false;
        }

        self.imp().video.press_key(key.into_glib(), state);
        true
    }

    pub fn key_released_cb(&self, key: gtk::gdk::Key, state: gtk::gdk::ModifierType) {
        let Some(view) = self.view() else {
            return;
        };

        if key == gtk::gdk::Key::Escape {
            if view.shows_sidebar() {
                view.set_show_sidebar(false);
            } else if let Some(window) = self.window()
                && window.is_fullscreen()
            {
                window.toggle_fullscreen();
            }
            return;
        }

        if key.to_lower() == gtk::gdk::Key::f {
            self.toggle_fullscreen();
            return;
        }

        if view.shows_sidebar() {
            return;
        }

        self.imp().video.release_key(key.into_glib(), state)
    }

    pub fn set_popover(&self) {
        let imp = self.imp();
        let builder = Builder::from_resource("/moe/tsuna/tsukimi/ui/mpv_menu.ui");
        let menu = builder.object::<gio::MenuModel>("mpv-menu");
        match menu {
            Some(popover) => {
                let popover = PopoverMenu::builder()
                    .menu_model(&popover)
                    .halign(gtk::Align::Start)
                    .has_arrow(false)
                    .build();
                popover.connect_map(glib::clone!(
                    #[weak(rename_to = obj)]
                    self,
                    move |_| obj.on_popover_opened()
                ));
                popover.connect_unmap(glib::clone!(
                    #[weak(rename_to = obj)]
                    self,
                    move |_| obj.on_popover_closed()
                ));
                popover.set_parent(self);
                popover.add_child(&imp.menu_actions, "menu-actions");
                let _ = imp.popover.replace(Some(popover));
            }
            None => eprintln!("Failed to load popover"),
        }
    }

    #[template_callback]
    fn on_popover_opened(&self) {
        let count = self.imp().popover_count.get();
        self.imp().popover_count.set(count.saturating_add(1));
    }

    #[template_callback]
    fn on_popover_closed(&self) {
        let count = self.imp().popover_count.get();
        self.imp().popover_count.set(count.saturating_sub(1));
    }

    pub fn on_backward(&self) {
        let video = &self.imp().video;
        let step = SETTINGS.mpv_seek_backward_step() as i64;
        video.seek_backward(step);
    }

    pub fn on_forward(&self) {
        let video = &self.imp().video;
        let step = SETTINGS.mpv_seek_forward_step() as i64;
        video.seek_forward(step);
    }

    pub fn chapter_prev(&self) {
        self.key_pressed_cb(PREV_CHAPTER_KEY, gtk::gdk::ModifierType::empty());
    }

    pub fn chapter_next(&self) {
        self.key_pressed_cb(NEXT_CHAPTER_KEY, gtk::gdk::ModifierType::empty());
    }

    pub fn mpv(&self) -> &mutsumi::ContextedMPV {
        self.imp().video.mpv()
    }

    pub fn notify_playing(&self) {
        self.notify_mpris_playing();
    }

    pub fn notify_track_changed(&self) {
        self.notify_mpris_track_changed();
    }

    pub fn notify_player_paused(&self) {
        self.notify_mpris_paused();
    }

    pub fn notify_volume_changed(&self, volume: f64) {
        self.notify_mpris_volume(volume);
    }

    pub fn notify_stopped(&self) {
        self.notify_mpris_stopped();
    }

    pub fn notify_seeked(&self, position: i64) {
        self.notify_mpris_seeked(position);
    }

    pub fn notify_track_list_replaced(&self) {
        self.notify_mpris_track_list_replaced();
    }
}

fn direct_play_url(source: &MediaSource, play_session_id: Option<&str>) -> Option<String> {
    let container = source.container.as_deref()?;
    JELLYFIN_CLIENT
        .get_item_direct_play_url(
            container,
            source.item_id.as_ref().unwrap_or(&source.id),
            &source.id,
            play_session_id,
        )
        .ok()
}

fn media_source_url(
    source: &MediaSource, play_session_id: Option<&str>,
) -> Option<(String, Option<MediaSourceFallback>)> {
    if let Some(path) = source.path.as_deref()
        && Url::parse(path).is_ok()
    {
        let fallback = if source.live_stream_id.is_some() {
            Some(MediaSourceFallback::PlaybackInfo)
        } else {
            direct_play_url(source, play_session_id).map(MediaSourceFallback::DirectPlay)
        };
        return Some((path.to_owned(), fallback));
    }
    direct_play_url(source, play_session_id).map(|url| (url, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(account_name: &str, user_id: &str, endpoint: &str) -> PlaybackCacheScope {
        PlaybackCacheScope {
            account_name: account_name.into(),
            user_id: user_id.into(),
            endpoint: Some(endpoint.into()),
        }
    }

    fn cache_key(scope: PlaybackCacheScope, item_id: &str) -> PlaybackCacheKey {
        PlaybackCacheKey::new(scope, item_id.into(), None, None)
    }

    fn selection(
        media_source_id: &str, video_index: i64, subtitle_index: i64,
    ) -> SelectedVideoSubInfo {
        SelectedVideoSubInfo {
            sub_lang: "eng".into(),
            sub_index: subtitle_index,
            video_index,
            media_source_id: media_source_id.into(),
        }
    }

    #[test]
    fn playback_cache_key_includes_item_and_selected_streams() {
        let first = selection("source-a", 0, 1);
        let other_source = selection("source-b", 0, 1);
        let other_video = selection("source-a", 1, 1);
        let other_subtitle = selection("source-a", 0, 2);

        let scope = scope("home", "user", "https://home.example/emby/");
        let key = PlaybackCacheKey::new(scope.clone(), "item-a".into(), Some(&first), None);
        assert_ne!(
            key,
            PlaybackCacheKey::new(scope.clone(), "item-b".into(), Some(&first), None)
        );
        assert_ne!(
            key,
            PlaybackCacheKey::new(scope.clone(), "item-a".into(), Some(&other_source), None)
        );
        assert_ne!(
            key,
            PlaybackCacheKey::new(scope.clone(), "item-a".into(), Some(&other_video), None)
        );
        assert_ne!(
            key,
            PlaybackCacheKey::new(scope, "item-a".into(), Some(&other_subtitle), None)
        );
    }

    #[test]
    fn playback_cache_key_includes_version_matcher() {
        let scope = scope("home", "user", "https://home.example/emby/");
        assert_ne!(
            PlaybackCacheKey::new(scope.clone(), "item".into(), None, Some("1080p".into())),
            PlaybackCacheKey::new(scope, "item".into(), None, Some("4K".into()))
        );
    }

    #[test]
    fn playback_cache_key_includes_account_and_endpoint_scope() {
        let key = cache_key(
            scope("home", "user-a", "https://home.example/emby/"),
            "item",
        );
        assert_ne!(
            key,
            cache_key(
                scope("home", "user-b", "https://home.example/emby/"),
                "item"
            )
        );
        assert_ne!(
            key,
            cache_key(
                scope("home", "user-a", "https://remote.example/emby/"),
                "item"
            )
        );
        assert_ne!(
            key,
            cache_key(
                scope("other-account", "user-a", "https://home.example/emby/"),
                "item"
            )
        );
    }

    #[test]
    fn same_cache_key_preserves_danmaku_but_invalid_cache_refreshes_it() {
        let first = cache_key(scope("home", "user", "https://home/emby/"), "item");
        let other = cache_key(scope("home", "user", "https://home/emby/"), "other");

        assert!(!should_refresh_danmaku(false, Some(&first), &first));
        assert!(should_refresh_danmaku(false, Some(&first), &other));
        assert!(should_refresh_danmaku(true, Some(&first), &first));
    }

    #[test]
    fn playback_events_only_cache_the_current_generation() {
        let scope = scope("home", "user", "https://home/emby/");
        let first = PlaybackRequest {
            generation: 1,
            cache_key: cache_key(scope.clone(), "first"),
        };
        let second = PlaybackRequest {
            generation: 2,
            cache_key: cache_key(scope, "second"),
        };
        let mut state = PlaybackLoadState::default();
        state.issued(first);
        state.issued(second.clone());

        assert_eq!(
            state.start_file().map(|request| request.generation),
            Some(1)
        );
        assert!(state.file_loaded(2).is_none());
        assert_eq!(
            state.start_file().map(|request| request.generation),
            Some(2)
        );
        assert_eq!(state.file_loaded(2), Some(&second.cache_key));
        assert_eq!(
            state.start_file().map(|request| request.generation),
            Some(2)
        );
        assert_eq!(state.file_loaded(2), Some(&second.cache_key));
        assert_eq!(state.take_active(), Some(second));
        assert_eq!(state.take_active(), None);
    }

    #[test]
    fn cached_resume_rebinds_the_active_generation() {
        let request = PlaybackRequest {
            generation: 7,
            cache_key: cache_key(scope("home", "user", "https://home/emby/"), "item"),
        };
        let mut state = PlaybackLoadState::default();
        state.resume_cached(request.clone());

        assert_eq!(state.file_loaded(7), Some(&request.cache_key));
        assert!(state.file_loaded(6).is_none());
    }

    #[test]
    fn manual_danmaku_transaction_preserves_loaded_external_track() {
        let loaded = ManualDanmakuTransaction::new(Some("https://example/track.xml".into()), true);
        assert!(loaded.preserve_renderer());
        assert_eq!(loaded.external_source_to_restore(), None);

        let loading =
            ManualDanmakuTransaction::new(Some("https://example/track.xml".into()), false);
        assert!(!loading.preserve_renderer());
        assert_eq!(
            loading.external_source_to_restore(),
            Some("https://example/track.xml")
        );
    }

    #[test]
    fn external_source_logs_omit_query_and_fragment() {
        assert_eq!(
            external_source_for_log("https://example/track.xml?api_key=secret#fragment"),
            "https://example/track.xml"
        );
        assert_eq!(
            external_source_for_log("https://example/track.xml#fragment?api_key=secret"),
            "https://example/track.xml"
        );
        assert_eq!(
            external_source_for_log(
                "https://danmaku-user:danmaku-password@example/track.xml?api_key=secret"
            ),
            "https://example/track.xml"
        );
        assert_eq!(
            external_source_for_log("not a valid URL?api_key=secret"),
            "<invalid external source>"
        );
    }

    #[test]
    fn danmaku_search_prefers_tmdb_over_title() {
        let params = MPVPage::danmaku_search_params(
            "Fallback title".to_string(),
            Some("3".to_string()),
            Some(42),
            0,
        );

        assert_eq!(params.anime, None);
        assert_eq!(params.tmdb_id, Some(42));
        assert_eq!(params.episode.as_deref(), Some("3"));
    }

    #[test]
    fn danmaku_search_keeps_title_without_tmdb() {
        let params = MPVPage::danmaku_search_params(
            "Fallback title".to_string(),
            Some("3".to_string()),
            None,
            0,
        );

        assert_eq!(params.anime.as_deref(), Some("Fallback title"));
        assert_eq!(params.tmdb_id, None);
    }

    #[test]
    fn danmaku_search_falls_back_after_tmdb_errors_or_empty_results() {
        let empty = Ok(EpisodeSearchResult {
            episode_match: None,
            anime_count: 0,
            episode_count: 0,
        });
        let matched = Ok(EpisodeSearchResult {
            episode_match: Some((1, "Episode 1".to_string())),
            anime_count: 1,
            episode_count: 1,
        });
        let failed = Err(anyhow::anyhow!("TMDB request failed"));

        assert!(MPVPage::should_fallback_danmaku_title(
            Some(42),
            &empty,
            false
        ));
        assert!(MPVPage::should_fallback_danmaku_title(
            Some(42),
            &failed,
            false
        ));
        assert!(!MPVPage::should_fallback_danmaku_title(
            Some(42),
            &matched,
            false
        ));
        assert!(!MPVPage::should_fallback_danmaku_title(
            Some(42),
            &failed,
            true
        ));
        assert!(!MPVPage::should_fallback_danmaku_title(
            None, &failed, false
        ));
    }
}
