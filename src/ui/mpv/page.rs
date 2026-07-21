use adw::prelude::*;
use gettextrs::gettext;
use glib::Object;
use gtk::{
    Builder,
    PopoverMenu,
    gdk::Rectangle,
    gio,
    glib,
    subclass::prelude::*,
};
use itertools::Itertools;
use mutsumi::*;

#[cfg(target_os = "linux")]
use std::{
    cell::Cell,
    rc::Rc,
    time::Instant,
};

#[cfg(target_os = "linux")]
use anyhow::Result;

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, serde::Serialize)]
struct DanmakuEpisodeSearchParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    anime: Option<String>,
    #[serde(rename = "tmdbId", skip_serializing_if = "Option::is_none")]
    tmdb_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    episode: Option<String>,
}

#[cfg(target_os = "linux")]
impl DanmakuEpisodeSearchParams {
    fn new(anime: &str, episode: &str, tmdb_id: Option<i32>) -> Self {
        let non_empty = |value: &str| {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        };
        Self {
            anime: non_empty(anime),
            tmdb_id,
            episode: non_empty(episode),
        }
    }
}

#[cfg(target_os = "linux")]
struct DanmakuEpisodes(DanmakuEpisodeSearchParams);

#[cfg(target_os = "linux")]
impl dandanapi::Request for DanmakuEpisodes {
    type Response = dandanapi::SearchEpisodesResponse;
    type Body = ();
    type Params = DanmakuEpisodeSearchParams;

    const PATH: &'static str = "/api/v2/search/episodes";

    fn params(&self) -> Option<&Self::Params> {
        Some(&self.0)
    }
}

#[cfg(target_os = "linux")]
struct DanmakuLoadResult {
    items: Vec<Danmaku>,
    raw_count: usize,
    fetch_elapsed_ms: u128,
    convert_elapsed_ms: u128,
}

use super::{
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
        provider::tu_item::TuItem,
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
    },
};

#[cfg(target_os = "linux")]
use crate::client::{
    DanmakuConvert,
    apply_danmaku_active_server,
    has_complete_danmaku_credentials,
    init_danmaku_client_credentials,
    init_danmaku_client_without_credentials,
    is_default_danmaku_credentials,
};

const MIN_MOTION_TIME: i64 = 100000;
const PREV_CHAPTER_KEYVAL: u32 = 65366;
const NEXT_CHAPTER_KEYVAL: u32 = 65365;
#[cfg(target_os = "linux")]
const DANMAKU_CONTEXT_HISTORY_LIMIT: usize = 80;
#[cfg(target_os = "linux")]
const DANMAKU_AUTO_NAME_LIMIT: usize = 3;
#[cfg(target_os = "linux")]
const DANMAKU_MANUAL_NAME_LIMIT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackDirectMode {
    pub enable_direct_play: bool,
    pub enable_direct_stream: bool,
}

impl Default for PlaybackDirectMode {
    fn default() -> Self {
        Self::direct()
    }
}

impl PlaybackDirectMode {
    pub const fn direct() -> Self {
        Self {
            enable_direct_play: true,
            enable_direct_stream: true,
        }
    }

    fn fallback(self) -> Option<Self> {
        match (self.enable_direct_play, self.enable_direct_stream) {
            (true, true) => Some(Self {
                enable_direct_play: false,
                enable_direct_stream: true,
            }),
            (false, true) => Some(Self {
                enable_direct_play: false,
                enable_direct_stream: false,
            }),
            (false, false) => None,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone)]
pub struct FallbackContext {
    selected: Option<SelectedVideoSubInfo>,
    start_seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackCacheKey {
    item_id: String,
    media_source_id: Option<String>,
    video_index: Option<i64>,
    subtitle_index: Option<i64>,
    video_matcher: Option<String>,
}

impl PlaybackCacheKey {
    fn new(
        item_id: String, selected: Option<&SelectedVideoSubInfo>, video_matcher: Option<String>,
    ) -> Self {
        Self {
            item_id,
            media_source_id: selected.map(|selected| selected.media_source_id.clone()),
            video_index: selected.map(|selected| selected.video_index),
            subtitle_index: selected.map(|selected| selected.sub_index),
            video_matcher,
        }
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

mod imp {

    use std::cell::{
        Cell,
        RefCell,
    };

    #[cfg(target_os = "linux")]
    use std::collections::HashMap;

    use adw::prelude::*;
    use gettextrs::gettext;
    use glib::subclass::InitializingObject;
    use gtk::{
        CompositeTemplate,
        PopoverMenu,
        glib,
        subclass::prelude::*,
    };
    #[cfg(target_os = "linux")]
    use mpris_server::LocalServer;
    use once_cell::sync::OnceCell;

    use crate::{
        APP_ID,
        client::structs::{
            Back,
            MediaSegment,
        },
        ui::{
            models::SETTINGS,
            mpv::{
                VolumeBar,
                menu_actions::MenuActions,
                sink::MPVPlaySink,
                video_scale::VideoScale,
            },
            provider::tu_item::TuItem,
            widgets::check_row::CheckRow,
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
        pub danmaku_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub danmaku_popover: TemplateChild<gtk::Popover>,
        #[template_child]
        pub danmaku_page: TemplateChild<adw::PreferencesPage>,
        #[template_child]
        pub danmaku_switch: TemplateChild<gtk::Switch>,
        #[template_child]
        pub danmaku_server_expander: TemplateChild<adw::ExpanderRow>,
        #[template_child]
        pub danmaku_anime_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub danmaku_episode_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub danmaku_tmdb_row: TemplateChild<adw::EntryRow>,
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
        pub last_playback_position: Cell<f64>,
        pub x: Cell<f64>,
        pub y: Cell<f64>,
        pub last_motion_time: Cell<i64>,
        pub suburl: RefCell<Option<String>>,
        pub skippable_segments: RefCell<Option<Vec<MediaSegment>>>,
        pub current_segment_end: Cell<Option<f64>>,
        pub popover: RefCell<Option<PopoverMenu>>,
        pub popover_count: Cell<u32>,
        pub menu_actions: MenuActions,
        #[cfg(target_os = "linux")]
        pub mpris_server: OnceCell<LocalServer<super::MPVPage>>,
        #[cfg(target_os = "linux")]
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
        pub fallback_context: RefCell<Option<super::FallbackContext>>,
        pub playback_direct_mode: RefCell<super::PlaybackDirectMode>,
        pub queued_playback_direct_mode: RefCell<Option<super::PlaybackDirectMode>>,
        pub retrying_playback: Cell<bool>,
        pub allow_fallback: Cell<bool>,
        pub last_nonzero_volume: Cell<i64>,
        pub(super) cached_playback: RefCell<Option<super::PlaybackCacheKey>>,
        pub(super) pending_playback: RefCell<Option<super::PlaybackCacheKey>>,
        pub(super) play_generation: Cell<u64>,
        #[cfg(target_os = "linux")]
        pub danmaku_list: RefCell<Option<Vec<mutsumi::Danmaku>>>,
        #[cfg(target_os = "linux")]
        pub danmaku_search_generation: Cell<u64>,
        #[cfg(target_os = "linux")]
        pub danmaku_server_rows: RefCell<Vec<CheckRow>>,
        #[cfg(target_os = "linux")]
        pub danmaku_anime_candidate_button: RefCell<Option<gtk::MenuButton>>,
        #[cfg(target_os = "linux")]
        pub danmaku_anime_candidate_popover: RefCell<Option<gtk::Popover>>,
        #[cfg(target_os = "linux")]
        pub danmaku_anime_candidate_list: RefCell<Option<gtk::ListBox>>,
        #[cfg(target_os = "linux")]
        pub danmaku_auto_names: RefCell<HashMap<String, Vec<String>>>,
        #[cfg(target_os = "linux")]
        pub danmaku_manual_names: RefCell<HashMap<String, Vec<String>>>,
        #[cfg(target_os = "linux")]
        pub danmaku_context_history: RefCell<Vec<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MPVPage {
        const NAME: &'static str = "MPVPage";
        type Type = super::MPVPage;
        type ParentType = adw::NavigationPage;

        fn class_init(klass: &mut Self::Class) {
            MPVPlaySink::ensure_type();
            VideoScale::ensure_type();
            VolumeBar::ensure_type();
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

            SETTINGS
                .bind("is-danmaku-enabled", &self.danmaku_switch.get(), "active")
                .build();

            self.video_scale.set_player(Some(&self.video.get()));
            self.video.connect_danmaku_timeline_changed(glib::clone!(
                #[weak(rename_to = scale)]
                self.video_scale,
                move |_, timeline| scale.set_danmaku_timeline(timeline)
            ));

            let obj = self.obj();

            #[cfg(target_os = "linux")]
            {
                obj.setup_danmaku_anime_candidate_picker();
                obj.rebuild_danmaku_server_list();
                SETTINGS.connect_changed(
                    Some("danmaku-servers"),
                    glib::clone!(
                        #[weak]
                        obj,
                        move |_, _| obj.rebuild_danmaku_server_list()
                    ),
                );
            }

            #[cfg(not(target_os = "linux"))]
            self.danmaku_button.set_visible(false);

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
            #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    fn dandan_client(&self) -> Result<dandanapi::DanDanClient> {
        let active = SETTINGS.danmaku_active_server();
        apply_danmaku_active_server(active, &SETTINGS.danmaku_servers())
            .map_err(anyhow::Error::msg)?;

        let appid = SETTINGS.danmaku_appid();
        let appsecret = SETTINGS.danmaku_appsecret();
        if active < 0
            && !is_default_danmaku_credentials(&appid, &appsecret)
            && !has_complete_danmaku_credentials(&appid, &appsecret)
        {
            return Err(anyhow::anyhow!(
                "Please fill App Secret when using a custom App ID."
            ));
        }
        let result = if active >= 0 && !has_complete_danmaku_credentials(&appid, &appsecret) {
            init_danmaku_client_without_credentials()
        } else if is_default_danmaku_credentials(&appid, &appsecret) {
            init_danmaku_client_credentials("", "")
        } else {
            init_danmaku_client_credentials(&appid, &appsecret)
        };
        result.map_err(anyhow::Error::msg)?;
        Ok(dandanapi::DanDanClient::instance())
    }

    #[cfg(target_os = "linux")]
    fn danmaku_server_label(active: i32) -> String {
        if active < 0 {
            return gettext(crate::client::DEFAULT_DANMAKU_SERVER_LABEL);
        }
        SETTINGS
            .danmaku_servers()
            .get(active as usize)
            .map(|server| server.name.clone())
            .unwrap_or_else(|| gettext(crate::client::DEFAULT_DANMAKU_SERVER_LABEL))
    }

    #[cfg(target_os = "linux")]
    fn rebuild_danmaku_server_list(&self) {
        let imp = self.imp();
        for row in imp.danmaku_server_rows.borrow_mut().drain(..) {
            imp.danmaku_server_expander.remove(&row);
        }

        let active = SETTINGS.danmaku_active_server();
        let mut group: Option<gtk::CheckButton> = None;
        let mut append = |label: String, server_index: i32| {
            let row = CheckRow::new();
            row.set_title(&glib::markup_escape_text(&label));
            let check = row.imp().check.get();
            if let Some(first) = &group {
                check.set_group(Some(first));
            } else {
                group = Some(check.clone());
            }
            check.set_active(active == server_index);
            row.connect_activated(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                move |_| obj.select_danmaku_server(server_index)
            ));
            imp.danmaku_server_expander.add_row(&row);
            imp.danmaku_server_rows.borrow_mut().push(row);
        };

        append(gettext(crate::client::DEFAULT_DANMAKU_SERVER_LABEL), -1);
        for (index, server) in SETTINGS.danmaku_servers().into_iter().enumerate() {
            append(server.name, index as i32);
        }
        imp.danmaku_server_expander
            .set_subtitle(&glib::markup_escape_text(&Self::danmaku_server_label(
                active,
            )));
    }

    #[cfg(target_os = "linux")]
    fn select_danmaku_server(&self, active: i32) {
        if SETTINGS.set_danmaku_active_server(active).is_err() {
            self.toast(gettext("Failed to save active danmaku server."));
            return;
        }
        self.cancel_danmaku_search();
        self.rebuild_danmaku_server_list();
        self.imp().danmaku_server_expander.set_expanded(false);
    }

    #[cfg(target_os = "linux")]
    fn setup_danmaku_anime_candidate_picker(&self) {
        let imp = self.imp();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        list.add_css_class("boxed-list");

        let popover = gtk::Popover::new();
        popover.set_child(Some(&list));
        let button = gtk::MenuButton::builder()
            .icon_name("pan-down-symbolic")
            .tooltip_text(gettext("Show anime name candidates"))
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        button.add_css_class("flat");
        button.set_popover(Some(&popover));
        imp.danmaku_anime_row.add_suffix(&button);
        imp.danmaku_anime_candidate_button.replace(Some(button));
        imp.danmaku_anime_candidate_popover.replace(Some(popover));
        imp.danmaku_anime_candidate_list.replace(Some(list));
    }

    #[cfg(target_os = "linux")]
    fn push_unique_danmaku_name(names: &mut Vec<String>, value: &str) {
        let value = value.trim();
        if !value.is_empty() && !names.iter().any(|name| name == value) {
            names.push(value.to_string());
        }
    }

    #[cfg(target_os = "linux")]
    fn danmaku_context_key(item: &TuItem) -> String {
        item.series_id().unwrap_or_else(|| item.id())
    }

    #[cfg(target_os = "linux")]
    fn touch_danmaku_context(&self, key: &str) {
        let imp = self.imp();
        let mut history = imp.danmaku_context_history.borrow_mut();
        history.retain(|stored| stored != key);
        history.insert(0, key.to_string());
        while history.len() > DANMAKU_CONTEXT_HISTORY_LIMIT {
            if let Some(expired) = history.pop() {
                imp.danmaku_auto_names.borrow_mut().remove(&expired);
                imp.danmaku_manual_names.borrow_mut().remove(&expired);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn danmaku_names_for_context(&self, key: &str) -> Vec<String> {
        let imp = self.imp();
        let mut names = Vec::new();
        for source in [&imp.danmaku_auto_names, &imp.danmaku_manual_names] {
            if let Some(stored) = source.borrow().get(key) {
                for name in stored {
                    Self::push_unique_danmaku_name(&mut names, name);
                }
            }
        }
        names
    }

    #[cfg(target_os = "linux")]
    fn remember_danmaku_names(
        &self, key: &str, names: impl IntoIterator<Item = String>, manual: bool,
    ) -> Vec<String> {
        let imp = self.imp();
        let (storage, limit) = if manual {
            (&imp.danmaku_manual_names, DANMAKU_MANUAL_NAME_LIMIT)
        } else {
            (&imp.danmaku_auto_names, DANMAKU_AUTO_NAME_LIMIT)
        };
        let mut storage = storage.borrow_mut();
        let stored = storage.entry(key.to_string()).or_default();
        for name in names {
            let name = name.trim();
            if manual {
                stored.retain(|candidate| candidate != name);
                if !name.is_empty() {
                    stored.insert(0, name.to_string());
                }
            } else {
                Self::push_unique_danmaku_name(stored, name);
            }
        }
        stored.truncate(limit);
        drop(storage);
        self.touch_danmaku_context(key);
        self.danmaku_names_for_context(key)
    }

    #[cfg(target_os = "linux")]
    fn rebuild_danmaku_anime_candidate_picker(&self, names: &[String]) {
        let imp = self.imp();
        if let Some(button) = imp.danmaku_anime_candidate_button.borrow().as_ref() {
            button.set_visible(names.len() > 1);
        }
        let Some(list) = imp.danmaku_anime_candidate_list.borrow().as_ref().cloned() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        let current = imp.danmaku_anime_row.text();
        for name in names {
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(name));
            row.set_activatable(true);
            let selected = gtk::Image::from_icon_name("object-select-symbolic");
            selected.set_visible(name == current.as_str());
            row.add_suffix(&selected);
            let name = name.clone();
            row.connect_activated(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                move |_| {
                    obj.imp().danmaku_anime_row.set_text(&name);
                    if let Some(popover) =
                        obj.imp().danmaku_anime_candidate_popover.borrow().as_ref()
                    {
                        popover.popdown();
                    }
                }
            ));
            list.append(&row);
        }
    }

    #[cfg(target_os = "linux")]
    async fn fetch_dandanplay_anime_titles(&self, tmdb_id: i32) -> Vec<String> {
        let Ok(response) = self
            .search_danmaku_episodes(DanmakuEpisodeSearchParams::new("", "", Some(tmdb_id)))
            .await
        else {
            return Vec::new();
        };
        response
            .animes
            .unwrap_or_default()
            .into_iter()
            .map(|anime| anime.anime_title)
            .unique()
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn prepare_danmaku_popover(&self) {
        self.rebuild_danmaku_server_list();
        let Some(item) = self.current_video() else {
            return;
        };
        let video_id = item.id();
        let context_key = Self::danmaku_context_key(&item);
        let anime = item.series_name().unwrap_or_else(|| item.name());
        let episode = if item.item_type() == "Movie" {
            String::new()
        } else if item.index_number() > 0 {
            item.index_number().to_string()
        } else {
            item.name()
        };
        let names = self.remember_danmaku_names(&context_key, [anime], false);
        self.imp()
            .danmaku_anime_row
            .set_text(names.first().map(String::as_str).unwrap_or_default());
        self.rebuild_danmaku_anime_candidate_picker(&names);
        self.imp().danmaku_episode_row.set_text(&episode);
        self.imp().danmaku_tmdb_row.set_text("");

        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let Some(tmdb_id) = obj.resolve_tmdb_id(item).await else {
                    return;
                };
                if obj.current_video().is_none_or(|item| item.id() != video_id) {
                    return;
                }
                obj.imp().danmaku_tmdb_row.set_text(&tmdb_id.to_string());
                let titles = obj.fetch_dandanplay_anime_titles(tmdb_id).await;
                if obj.current_video().is_none_or(|item| item.id() != video_id) {
                    return;
                }
                let names = obj.remember_danmaku_names(&context_key, titles, false);
                obj.rebuild_danmaku_anime_candidate_picker(&names);
            }
        ));
    }

    #[cfg(target_os = "linux")]
    async fn resolve_tmdb_id(&self, item: TuItem) -> Option<i32> {
        let mut ids = Vec::new();
        if item.item_type() != "Episode" {
            ids.push(item.id());
        }
        if let Some(season_id) = item.season_id() {
            ids.push(season_id);
        }
        if let Some(series_id) = item.series_id() {
            ids.push(series_id);
        }

        for id in ids {
            if let Ok(info) =
                spawn_tokio(async move { JELLYFIN_CLIENT.get_item_info(&id).await }).await
                && let Some(tmdb_id) = info
                    .provider_ids
                    .and_then(|provider_ids| provider_ids.tmdb)
                    .and_then(|id| id.parse().ok())
            {
                return Some(tmdb_id);
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    async fn search_danmaku_episodes(
        &self, params: DanmakuEpisodeSearchParams,
    ) -> Result<dandanapi::SearchEpisodesResponse> {
        let route = self.dandan_client()?.route(DanmakuEpisodes(params));
        spawn_tokio(async move { Ok(route.await?) }).await
    }

    #[cfg(target_os = "linux")]
    async fn request_danmaku(&self, episode_id: i64) -> Result<DanmakuLoadResult> {
        let route = self.dandan_client()?.route(dandanapi::Comments {
            episode_id,
            request_comments: dandanapi::RequestComments {
                from: 0,
                with_related: true,
                ch_convert: dandanapi::ChConvert::NONE,
            },
        });
        spawn_tokio(async move {
            let fetch_started = Instant::now();
            let comments = route.await?.comments.unwrap_or_default();
            let fetch_elapsed_ms = fetch_started.elapsed().as_millis();
            let raw_count = comments.len();
            let convert_started = Instant::now();
            let items = comments
                .into_iter()
                .filter_map(DanmakuConvert::into_danmaku)
                .collect();
            Ok(DanmakuLoadResult {
                items,
                raw_count,
                fetch_elapsed_ms,
                convert_elapsed_ms: convert_started.elapsed().as_millis(),
            })
        })
        .await
    }

    #[cfg(target_os = "linux")]
    pub async fn load_danmaku(&self) {
        if !SETTINGS.is_danmaku_enabled() || self.imp().video.has_external_danmaku_source() {
            return;
        }
        let Some(item) = self.current_video() else {
            return;
        };
        let video_id = item.id();
        let generation = self.next_danmaku_search_generation();
        let anime = item.series_name().unwrap_or_else(|| item.name());
        let episode = if item.item_type() == "Movie" {
            String::new()
        } else if item.index_number() > 0 {
            item.index_number().to_string()
        } else {
            item.name()
        };
        let tmdb_id = self.resolve_tmdb_id(item).await;
        if self.danmaku_request_is_stale(generation, &video_id) {
            return;
        }
        let params = DanmakuEpisodeSearchParams::new(
            if tmdb_id.is_some() { "" } else { &anime },
            &episode,
            tmdb_id,
        );
        let server = Self::danmaku_server_label(SETTINGS.danmaku_active_server());
        tracing::info!(?params, %server, "Automatic danmaku search started");

        let search_started = Instant::now();
        match self.search_danmaku_episodes(params).await {
            Ok(response) => {
                if self.danmaku_request_is_stale(generation, &video_id) {
                    return;
                }
                let episode_id = response.animes.and_then(|animes| {
                    animes
                        .into_iter()
                        .find_map(|anime| anime.episodes.first().map(|episode| episode.episode_id))
                });
                if let Some(episode_id) = episode_id {
                    tracing::info!(
                        episode_id,
                        elapsed_ms = search_started.elapsed().as_millis(),
                        %server,
                        "Automatic danmaku search matched"
                    );
                    self.load_danmaku_episode_for_video(episode_id, video_id, generation, true)
                        .await;
                } else {
                    tracing::info!(
                        elapsed_ms = search_started.elapsed().as_millis(),
                        %server,
                        "Automatic danmaku search returned no episodes"
                    );
                    self.clear_danmaku_items(&gettext("No Danmaku Loaded"));
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    elapsed_ms = search_started.elapsed().as_millis(),
                    %server,
                    "Automatic danmaku search failed"
                );
                if !self.danmaku_request_is_stale(generation, &video_id) {
                    self.clear_danmaku_items(&gettext("No Danmaku Loaded"));
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    async fn load_danmaku_episode(&self, episode_id: i64) {
        let Some(video_id) = self.current_video().map(|item| item.id()) else {
            return;
        };
        let generation = self.next_danmaku_search_generation();
        self.load_danmaku_episode_for_video(episode_id, video_id, generation, false)
            .await;
    }

    #[cfg(target_os = "linux")]
    async fn load_danmaku_episode_for_video(
        &self, episode_id: i64, video_id: String, generation: u64, automatic: bool,
    ) {
        let started = Instant::now();
        match self.request_danmaku(episode_id).await {
            Ok(result) => {
                if self.danmaku_request_is_stale(generation, &video_id)
                    || automatic && self.imp().video.has_external_danmaku_source()
                {
                    return;
                }
                let count = result.items.len();
                tracing::debug!(
                    episode_id,
                    raw_count = result.raw_count,
                    count,
                    fetch_elapsed_ms = result.fetch_elapsed_ms,
                    convert_elapsed_ms = result.convert_elapsed_ms,
                    automatic,
                    "Danmaku request completed"
                );
                self.imp().danmaku_list.replace(Some(result.items.clone()));
                self.imp().video.load_danmaku_items(result.items);
                self.imp().danmaku_page.set_description(&format!(
                    "{} {}",
                    count,
                    gettext("Danmaku Loaded")
                ));
                tracing::info!(
                    episode_id,
                    count,
                    automatic,
                    elapsed_ms = started.elapsed().as_millis(),
                    "Danmaku loaded"
                );
            }
            Err(error) => {
                tracing::warn!(
                    episode_id,
                    %error,
                    automatic,
                    elapsed_ms = started.elapsed().as_millis(),
                    "Danmaku load failed"
                );
                if !self.danmaku_request_is_stale(generation, &video_id) {
                    self.clear_danmaku_items(&gettext("Failed to load danmaku"));
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn danmaku_request_is_stale(&self, generation: u64, video_id: &str) -> bool {
        self.imp().danmaku_search_generation.get() != generation
            || self
                .current_video()
                .is_none_or(|item| item.id() != video_id)
    }

    #[cfg(target_os = "linux")]
    fn clear_danmaku_items(&self, description: &str) {
        self.imp().danmaku_list.take();
        if !self.imp().video.has_external_danmaku_source() {
            self.imp().video.load_danmaku_items(Vec::new());
        }
        self.imp().danmaku_page.set_description(description);
    }

    #[cfg(target_os = "linux")]
    fn next_danmaku_search_generation(&self) -> u64 {
        let generation = self.imp().danmaku_search_generation.get().wrapping_add(1);
        self.imp().danmaku_search_generation.set(generation);
        generation
    }

    #[cfg(target_os = "linux")]
    fn cancel_danmaku_search(&self) {
        self.next_danmaku_search_generation();
    }

    #[template_callback]
    #[cfg(target_os = "linux")]
    fn on_danmaku_popover_opened(&self) {
        self.on_popover_opened();
        self.prepare_danmaku_popover();
    }

    #[template_callback]
    #[cfg(target_os = "linux")]
    fn on_danmaku_popover_closed(&self) {
        self.cancel_danmaku_search();
        self.imp().danmaku_server_expander.set_expanded(false);
        self.on_popover_closed();
    }

    #[template_callback]
    #[cfg(target_os = "linux")]
    fn on_danmaku_switch_state_set(&self, state: bool) -> bool {
        let _ = SETTINGS.set_boolean("is-danmaku-enabled", state);
        if state
            && self.imp().danmaku_list.borrow().is_none()
            && !self.imp().video.has_external_danmaku_source()
        {
            spawn(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                async move { obj.load_danmaku().await }
            ));
        } else if !state {
            self.cancel_danmaku_search();
        }
        false
    }

    #[template_callback]
    #[cfg(target_os = "linux")]
    async fn on_danmaku_search_clicked(&self) {
        if self.current_video().is_none() {
            self.toast(gettext("No video is currently playing"));
            return;
        }
        let imp = self.imp();
        let anime = imp.danmaku_anime_row.text();
        let episode = imp.danmaku_episode_row.text();
        let tmdb_id = imp.danmaku_tmdb_row.text().trim().parse().ok();
        let params = DanmakuEpisodeSearchParams::new(&anime, &episode, tmdb_id);
        let generation = self.next_danmaku_search_generation();
        if let Some(item) = self.current_video() {
            let names = self.remember_danmaku_names(
                &Self::danmaku_context_key(&item),
                [anime.to_string()],
                true,
            );
            self.rebuild_danmaku_anime_candidate_picker(&names);
        }

        let server = Self::danmaku_server_label(SETTINGS.danmaku_active_server());
        let search_started = Instant::now();
        tracing::info!(generation, ?params, %server, "Manual danmaku search started");
        match self.search_danmaku_episodes(params).await {
            Ok(response) if self.imp().danmaku_search_generation.get() == generation => {
                let count = response
                    .animes
                    .as_ref()
                    .map(|animes| {
                        animes
                            .iter()
                            .map(|anime| anime.episodes.len())
                            .sum::<usize>()
                    })
                    .unwrap_or_default();
                tracing::info!(
                    generation,
                    count,
                    elapsed_ms = search_started.elapsed().as_millis(),
                    %server,
                    "Manual danmaku search completed"
                );
                self.imp().danmaku_popover.popdown();
                self.show_danmaku_selection_dialog(response);
            }
            Ok(_) => {}
            Err(error) if self.imp().danmaku_search_generation.get() == generation => {
                tracing::warn!(
                    generation,
                    %error,
                    elapsed_ms = search_started.elapsed().as_millis(),
                    %server,
                    "Manual danmaku search failed"
                );
                self.toast(gettext("No Danmaku Loaded"));
            }
            Err(_) => {}
        }
    }

    #[cfg(target_os = "linux")]
    fn show_danmaku_selection_dialog(&self, response: dandanapi::SearchEpisodesResponse) {
        let animes = response.animes.unwrap_or_default();
        let episode_count = animes
            .iter()
            .map(|anime| anime.episodes.len())
            .sum::<usize>();
        if episode_count == 0 {
            self.toast(gettext("No episodes found"));
            return;
        }
        if animes.len() == 1 && episode_count == 1 {
            let episode_id = animes[0].episodes[0].episode_id;
            spawn(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                async move { obj.load_danmaku_episode(episode_id).await }
            ));
            return;
        }

        let dialog = adw::Dialog::builder()
            .title(gettext("Select Danmaku Source"))
            .content_width(480)
            .content_height(600)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let selected = Rc::new(Cell::new(None::<i64>));
        let rows = Rc::new(std::cell::RefCell::new(Vec::<adw::ActionRow>::new()));

        for anime in animes {
            let expander = adw::ExpanderRow::new();
            expander.set_title(&glib::markup_escape_text(&anime.anime_title));
            expander.set_expanded(true);
            for episode in anime.episodes {
                let row = adw::ActionRow::new();
                row.set_title(&glib::markup_escape_text(
                    episode.episode_title.as_deref().unwrap_or("Episode"),
                ));
                row.set_activatable(true);
                let selected = selected.clone();
                let selection_rows = rows.clone();
                row.connect_activated(move |row| {
                    selected.set(Some(episode.episode_id));
                    for candidate in selection_rows.borrow().iter() {
                        candidate.remove_css_class("accent");
                    }
                    row.add_css_class("accent");
                });
                rows.borrow_mut().push(row.clone());
                expander.add_row(&row);
            }
            group.add(&expander);
        }
        page.add(&group);
        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scrolled.set_child(Some(&page));
        toolbar.set_content(Some(&scrolled));

        let load = gtk::Button::builder()
            .label(gettext("Load Danmaku"))
            .css_classes(["suggested-action"])
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        button_box.set_halign(gtk::Align::Center);
        button_box.append(&load);
        toolbar.add_bottom_bar(&button_box);
        dialog.set_child(Some(&toolbar));
        load.connect_clicked(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            #[weak]
            dialog,
            move |_| {
                if let Some(episode_id) = selected.get() {
                    dialog.close();
                    spawn(glib::clone!(
                        #[weak(rename_to = obj)]
                        obj,
                        async move { obj.load_danmaku_episode(episode_id).await }
                    ));
                }
            }
        ));
        dialog.present(Some(self));
    }

    #[template_callback]
    #[cfg(not(target_os = "linux"))]
    fn on_danmaku_popover_opened(&self) {
        self.on_popover_opened();
    }

    #[template_callback]
    #[cfg(not(target_os = "linux"))]
    fn on_danmaku_popover_closed(&self) {
        self.on_popover_closed();
    }

    #[template_callback]
    #[cfg(not(target_os = "linux"))]
    fn on_danmaku_switch_state_set(&self, _state: bool) -> bool {
        false
    }

    #[template_callback]
    #[cfg(not(target_os = "linux"))]
    async fn on_danmaku_search_clicked(&self) {}

    fn mark_stream_failed(&self) {
        let imp = self.imp();
        imp.allow_fallback.set(false);
        imp.pending_playback.take();
        imp.cached_playback.take();
        imp.loading_box.set_visible(false);
        imp.spinner.set_visible(false);
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
        self.imp().pending_playback.take();
    }

    fn is_loading_failed(value: &str) -> bool {
        value.contains("-13")
    }

    fn retry_fallback_playback(&self) -> bool {
        let next_mode = {
            let imp = self.imp();
            if imp.retrying_playback.get() {
                return true;
            }
            let Some(next_mode) = imp.playback_direct_mode.borrow().fallback() else {
                return false;
            };
            imp.retrying_playback.set(true);
            next_mode
        };

        let Some(context) = self.imp().fallback_context.take() else {
            self.imp().retrying_playback.set(false);
            return false;
        };
        let Some(item) = self.current_video() else {
            self.imp().retrying_playback.set(false);
            return false;
        };

        tracing::info!(
            "Retrying playback with EnableDirectPlay={}, EnableDirectStream={}",
            next_mode.enable_direct_play,
            next_mode.enable_direct_stream
        );

        let episode_list = self.imp().current_episode_list.take();
        self.imp()
            .queued_playback_direct_mode
            .replace(Some(next_mode));
        spawn_g_timeout(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                obj.play(
                    context.selected,
                    item,
                    episode_list,
                    None,
                    context.start_seconds,
                );
            }
        ));

        true
    }

    pub fn play(
        &self, selected: Option<SelectedVideoSubInfo>, item: TuItem, episode_list: Vec<TuItem>,
        video_matcher: Option<String>, start_seconds: f64,
    ) {
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
        self.imp().video_scale.reset_scale();
        self.imp().last_playback_position.set(start_seconds);
        self.reset_skippable_segments();

        // If the video_matcher is None, field wont be updated
        if let Some(video_matcher) = video_matcher {
            self.imp()
                .video_version_matcher
                .replace(Some(video_matcher));
        }

        let cache_key = PlaybackCacheKey::new(
            id.clone(),
            selected.as_ref(),
            self.imp().video_version_matcher.borrow().clone(),
        );
        let resume_cached = self.imp().cached_playback.borrow().as_ref() == Some(&cache_key);
        let generation = self.begin_play_request();

        #[cfg(target_os = "linux")]
        let track_list_changed = self.mpris_track_list_changed(&episode_list);

        #[cfg(target_os = "linux")]
        if !resume_cached {
            self.cancel_danmaku_search();
            self.imp().danmaku_list.take();
        }
        self.set_current_video(Some(item));
        self.imp().current_episode_list.replace(episode_list);

        #[cfg(target_os = "linux")]
        {
            self.imp().mpris_art_url.take();
            if track_list_changed {
                self.notify_track_list_replaced();
            }
        }
        self.notify_track_changed();
        self.imp().fallback_context.replace(Some(FallbackContext {
            selected: selected.to_owned(),
            start_seconds,
        }));
        let direct_mode = self
            .imp()
            .queued_playback_direct_mode
            .borrow_mut()
            .take()
            .unwrap_or_else(PlaybackDirectMode::direct);
        self.imp().playback_direct_mode.replace(direct_mode);
        self.imp().retrying_playback.set(false);
        self.imp().allow_fallback.set(true);

        self.load_skippable_segments(id.to_owned());

        if resume_cached {
            let imp = self.imp();
            imp.allow_fallback.set(false);
            imp.spinner.set_visible(false);
            imp.loading_box.set_visible(false);
            imp.video.resume_cached(start_seconds);
            self.notify_playing();
            self.update_timeout();
            self.handle_callback(BackType::Start);
            return;
        }

        self.imp().cached_playback.take();
        self.imp().pending_playback.replace(Some(cache_key));
        self.imp().suburl.take();
        self.imp().video.stop();

        spawn_g_timeout(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
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
                        .get_playbackinfo(
                            &id_clone,
                            sub_stream_index,
                            media_source_id,
                            true,
                            direct_mode,
                        )
                        .await
                })
                .await;
                if !obj.request_is_current(generation) {
                    return;
                }
                let playback_info = match playback_info {
                    Ok(playback_info) => playback_info,
                    Err(e) => {
                        obj.mark_stream_failed();
                        obj.toast(e.to_user_facing());
                        obj.notify_stopped();
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
                    obj.mark_stream_failed();
                    obj.toast(gettext("No media source found"));
                    obj.notify_stopped();
                    return;
                };

                let back = Back {
                    id: id.to_owned(),
                    series_id,
                    playsessionid: playback_info.play_session_id.to_owned(),
                    mediasourceid: media_source.id.to_owned(),
                    livestreamid: media_source.live_stream_id.to_owned(),
                    playmethod: media_source_play_method(media_source),
                    tick: media_source.run_time_ticks.unwrap_or(0),
                    start_tick: glib::real_time() as u64 * 10,
                };

                imp.back.replace(Some(back));

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

                if let Some(slang) = selected.map(|s| s.sub_lang) {
                    imp.video.set_slang(slang);
                } else {
                    imp.video
                        .set_slang(SETTINGS.mpv_subtitle_preferred_lang_str());
                }

                let sub_url = match media_stream {
                    Some(stream) if stream.is_external => match &stream.delivery_url {
                        Some(url) => Some(JELLYFIN_CLIENT.get_streaming_url(url).await),
                        None => {
                            println!("External Subtitle without selected source");
                            imp.obj()
                                .external_sub_url_without_selected_source(
                                    id.to_owned(),
                                    stream,
                                    media_source.id.to_owned(),
                                    direct_mode,
                                )
                                .await
                        }
                    },
                    _ => None,
                };
                if !obj.request_is_current(generation) {
                    return;
                }

                imp.suburl.replace(sub_url);

                let video_url = match media_source_stream_url(media_source).await {
                    Some(video_url) => video_url,
                    None => {
                        obj.mark_stream_failed();
                        obj.toast(gettext("No media source found"));
                        obj.notify_stopped();
                        return;
                    }
                };
                let video_url = JELLYFIN_CLIENT.get_streaming_url(&video_url).await;
                if !obj.request_is_current(generation) {
                    return;
                }

                imp.video.play(&video_url, start_seconds);
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
        direct_mode: PlaybackDirectMode,
    ) -> Option<String> {
        let stream_index = media_stream.index;
        let media_source_id_clone = media_source_id.to_owned();
        let playback_info = spawn_tokio(async move {
            JELLYFIN_CLIENT
                .get_playbackinfo(
                    &id,
                    Some(stream_index),
                    Some(media_source_id),
                    true,
                    direct_mode,
                )
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

        Some(JELLYFIN_CLIENT.get_streaming_url(&url).await)
    }

    async fn set_audio_and_video_tracks_dropdown(&self, value: MpvTracks) {
        let imp = self.imp();
        imp.video.load_danmaku_track(value.danmaku_track);
        self.bind_tracks(
            value.audio_tracks,
            &imp.audio_listbox.get(),
            MpvTrackKind::Audio,
        )
        .await;
        self.bind_tracks(
            value.sub_tracks,
            &imp.sub_listbox.get(),
            MpvTrackKind::Subtitle,
        )
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
                        ListenEvent::PausedForCache(true, _) => {
                            obj.imp().video.set_buffering(true);
                            obj.update_seeking(true);
                        }
                        ListenEvent::Seek(position) => {
                            obj.imp().video.seek_danmaku(position);
                            obj.update_seeking(true);
                        }
                        ListenEvent::PausedForCache(false, _) => {
                            obj.imp().video.set_buffering(false);
                            let was_seeking = obj.get_seeking();
                            obj.update_seeking(false);
                            if was_seeking {
                                obj.handle_callback(BackType::Back);
                                obj.notify_seeked(obj.imp().video.position() as i64);
                            }
                        }
                        ListenEvent::PlaybackRestart(position) => {
                            obj.imp().video.set_buffering(false);
                            obj.imp().video.seek_danmaku(position);
                            let was_seeking = obj.get_seeking();
                            obj.update_seeking(false);
                            if was_seeking {
                                obj.handle_callback(BackType::Back);
                                obj.notify_seeked(obj.imp().video.position() as i64);
                            }
                        }
                        ListenEvent::Eof(value) => {
                            obj.on_end_file(value);
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
                        ListenEvent::StartFile => {}
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
        self.imp().video.update_position(value as f64);
        self.imp().last_playback_position.set(value as f64);
        if !self.imp().video_scale.is_dragging() {
            self.imp().video_scale.set_value(value as f64);
        }
        self.update_skip_segment_button(value as f64);
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
        let Some(cache_key) = imp.pending_playback.take() else {
            tracing::debug!("Ignoring file-loaded event without a pending playback request");
            return;
        };
        imp.cached_playback.replace(Some(cache_key));
        imp.allow_fallback.set(false);
        imp.video.mark_loaded();
        if let Some(suburl) = imp.suburl.borrow().as_ref() {
            imp.video.add_sub(suburl);
        }
        self.notify_playing();
        self.update_timeout();
        self.handle_callback(BackType::Start);

        #[cfg(target_os = "linux")]
        if SETTINGS.is_danmaku_enabled() {
            spawn(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                async move { obj.load_danmaku().await }
            ));
        }
    }

    fn update_seeking(&self, seeking: bool) {
        self.imp().seeking.set(seeking);
        let spinner = &self.imp().spinner;
        let loading_box = &self.imp().loading_box;
        if seeking {
            loading_box.set_visible(true);
            spinner.set_visible(true);
        } else {
            loading_box.set_visible(false);
            spinner.set_visible(false);
        }
    }

    fn get_seeking(&self) -> bool {
        self.imp().seeking.get()
    }

    fn on_end_file(&self, value: u32) {
        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                if value == 0 {
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
        if self.imp().allow_fallback.get()
            && Self::is_loading_failed(value)
            && self.retry_fallback_playback()
        {
            return;
        }

        self.mark_stream_failed();
        self.toast(value);
        self.notify_stopped();
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
            && widget.ancestor(MPVPlaySink::static_type()).is_none()
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

        let Some(surface) = self.native().and_then(|f| f.surface()) else {
            return;
        };
        let cursor = if reveal {
            gtk::gdk::Cursor::from_name("default", None)
        } else {
            gtk::gdk::Cursor::from_name("none", None)
        };

        surface.set_cursor(cursor.as_ref());
    }

    #[template_callback]
    fn on_play_pause_clicked(&self) {
        let video = &self.imp().video;
        video.pause();
    }

    #[template_callback]
    pub fn on_stop_clicked(&self) {
        self.remove_timeout();
        self.reset_skippable_segments();
        let current_video = self.current_video();

        let video = &self.imp().video;
        self.cancel_pending_playback();
        if self.imp().cached_playback.borrow().is_some() {
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

    pub fn key_pressed_cb(&self, key: u32, state: gtk::gdk::ModifierType) {
        let binding = self.ancestor(adw::OverlaySplitView::static_type());
        let Some(view) = binding.and_downcast_ref::<adw::OverlaySplitView>() else {
            return;
        };

        if view.shows_sidebar() {
            return;
        }

        self.imp().video.press_key(key, state)
    }

    pub fn key_released_cb(&self, key: u32, state: gtk::gdk::ModifierType) {
        let binding = self.ancestor(adw::OverlaySplitView::static_type());
        let Some(view) = binding.and_downcast_ref::<adw::OverlaySplitView>() else {
            return;
        };

        if view.shows_sidebar() {
            return;
        }

        self.imp().video.release_key(key, state)
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
        self.key_pressed_cb(PREV_CHAPTER_KEYVAL, gtk::gdk::ModifierType::empty());
    }

    pub fn chapter_next(&self) {
        self.key_pressed_cb(NEXT_CHAPTER_KEYVAL, gtk::gdk::ModifierType::empty());
    }

    pub fn mpv(&self) -> &mutsumi::ContextedMPV {
        self.imp().video.mpv()
    }

    pub fn notify_playing(&self) {
        #[cfg(target_os = "linux")]
        self.notify_mpris_playing();
    }

    pub fn notify_track_changed(&self) {
        #[cfg(target_os = "linux")]
        self.notify_mpris_track_changed();
    }

    pub fn notify_player_paused(&self) {
        #[cfg(target_os = "linux")]
        self.notify_mpris_paused();
    }

    pub fn notify_volume_changed(&self, volume: f64) {
        #[cfg(target_os = "linux")]
        self.notify_mpris_volume(volume);
    }

    pub fn notify_stopped(&self) {
        #[cfg(target_os = "linux")]
        self.notify_mpris_stopped();
    }

    pub fn notify_seeked(&self, position: i64) {
        #[cfg(target_os = "linux")]
        self.notify_mpris_seeked(position);
    }

    pub fn notify_track_list_replaced(&self) {
        #[cfg(target_os = "linux")]
        self.notify_mpris_track_list_replaced();
    }
}

pub async fn direct_stream_url(source: &MediaSource) -> Option<String> {
    let container = source.container.as_deref()?;
    JELLYFIN_CLIENT
        .get_item_stream_url(
            container,
            source.item_id.as_ref().unwrap_or(&source.id),
            &source.id,
        )
        .await
        .ok()
}

fn playable_media_source_path(source: &MediaSource) -> Option<String> {
    let path = source.path.as_deref()?;

    if path.starts_with("http://") || path.starts_with("https://") {
        return Some(path.to_owned());
    }

    None
}

fn media_source_play_method(source: &MediaSource) -> &'static str {
    if source.direct_stream_url.is_some() {
        return "DirectStream";
    }
    if source.transcoding_url.is_some() {
        return "Transcode";
    }
    "DirectPlay"
}

pub async fn media_source_stream_url(source: &MediaSource) -> Option<String> {
    if let Some(direct_url) = source.direct_stream_url.to_owned() {
        return Some(direct_url);
    }

    if let Some(transcoding_url) = source.transcoding_url.to_owned() {
        return Some(transcoding_url);
    }

    if let Some(path) = playable_media_source_path(source) {
        return Some(path);
    }

    direct_stream_url(source).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn playback_cache_key_includes_selected_streams() {
        let first = selection("source-a", 0, 1);
        let second = selection("source-a", 0, 2);

        assert_ne!(
            PlaybackCacheKey::new("item".into(), Some(&first), None),
            PlaybackCacheKey::new("item".into(), Some(&second), None)
        );
    }

    #[test]
    fn playback_cache_key_includes_version_matcher() {
        assert_ne!(
            PlaybackCacheKey::new("item".into(), None, Some("1080p".into())),
            PlaybackCacheKey::new("item".into(), None, Some("4K".into()))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn danmaku_search_omits_empty_parameters() {
        let params = DanmakuEpisodeSearchParams::new("  ", " 1 ", Some(42));
        assert_eq!(
            serde_json::to_value(params).expect("serializable search parameters"),
            serde_json::json!({ "episode": "1", "tmdbId": 42 })
        );
    }
}
