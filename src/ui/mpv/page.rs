#![allow(deprecated)]
// FIXME: replace GtkShortcutsWindow when the replacement is appeared on libadwaita

use std::{
    cell::{
        Cell,
        RefCell,
    },
    rc::Rc,
};

use adw::prelude::*;
use anyhow::Result;
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

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, serde::Serialize)]
struct DanmakuEpisodeSearchParams {
    #[serde(rename = "anime", skip_serializing_if = "Option::is_none")]
    anime: Option<String>,
    #[serde(rename = "tmdbId", skip_serializing_if = "Option::is_none")]
    tmdb_id: Option<i32>,
    #[serde(rename = "episode", skip_serializing_if = "Option::is_none")]
    episode: Option<String>,
}

#[cfg(target_os = "linux")]
impl DanmakuEpisodeSearchParams {
    fn new(anime: String, episode: String, tmdb_id: Option<i32>) -> Self {
        Self {
            anime: Self::non_empty_param(anime),
            tmdb_id,
            episode: Self::non_empty_param(episode),
        }
    }

    fn non_empty_param(value: String) -> Option<String> {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
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

use super::{
    tsukimi_mpv::{
        ChapterList,
        ListenEvent,
        MPV_EVENT_CHANNEL,
        MpvTrack,
        MpvTracks,
        TrackSelection,
    },
    video::MPVVideo,
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
    danmaku_combo_to_server_index,
    danmaku_server_to_combo_index,
    has_complete_danmaku_credentials,
    init_danmaku_client_credentials,
    init_danmaku_client_without_credentials,
    is_default_danmaku_credentials,
};

const MIN_MOTION_TIME: i64 = 100000;
const PREV_CHAPTER_KEYVAL: u32 = 65366;
const NEXT_CHAPTER_KEYVAL: u32 = 65365;
#[cfg(target_os = "linux")]
const DANMAKU_AUTO_ANIME_NAME_LIMIT: usize = 3;
#[cfg(target_os = "linux")]
const DANMAKU_MANUAL_ANIME_NAME_LIMIT: usize = 2;
#[cfg(target_os = "linux")]
const DANMAKU_ANIME_CONTEXT_LIMIT: usize = 80;

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

#[derive(Clone, Copy)]
enum MpvTrackKind {
    Audio,
    Subtitle,
}

impl MpvTrackKind {
    fn property(self) -> &'static str {
        match self {
            Self::Audio => "aid",
            Self::Subtitle => "sid",
        }
    }
}

mod imp {
    use std::{
        cell::{
            Cell,
            RefCell,
        },
        collections::HashMap,
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
                video::MPVVideo,
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
        pub video: TemplateChild<MPVVideo>,
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
        pub danmaku_server_combo: TemplateChild<adw::ComboRow>,
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
        pub seeking: RefCell<bool>,
        pub last_playback_position: Cell<f64>,
        pub x: RefCell<f64>,
        pub y: RefCell<f64>,
        pub last_motion_time: RefCell<i64>,
        pub suburl: RefCell<Option<String>>,
        pub skippable_segments: RefCell<Option<Vec<MediaSegment>>>,
        pub current_segment_end: Cell<Option<f64>>,
        pub popover: RefCell<Option<PopoverMenu>>,
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
        #[template_child]
        pub danmaku_speed_adj: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub danmaku_opacity_adj: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub danmaku_font_size_adj: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub danmaku_intensity_adj: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub danmaku_font_weight_adj: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub danmaku_spacing_factor_adj: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub danmaku_outline_px_adj: TemplateChild<gtk::Adjustment>,
        #[template_child]
        pub danmaku_shadow_offset_adj: TemplateChild<gtk::Adjustment>,

        #[property(get, set, nullable)]
        pub current_video: RefCell<Option<TuItem>>,
        pub current_episode_list: RefCell<Vec<TuItem>>,

        #[property(get, set, default_value = true)]
        pub key_vaild: RefCell<bool>,

        pub video_version_matcher: RefCell<Option<String>>,
        pub fallback_context: RefCell<Option<super::FallbackContext>>,
        pub playback_direct_mode: RefCell<super::PlaybackDirectMode>,
        pub queued_playback_direct_mode: RefCell<Option<super::PlaybackDirectMode>>,
        pub retrying_playback: Cell<bool>,
        pub allow_fallback: Cell<bool>,
        pub last_nonzero_volume: Cell<i64>,
        #[cfg(target_os = "linux")]
        pub danmaku_client: OnceCell<dandanapi::DanDanClient>,
        #[cfg(target_os = "linux")]
        pub danmaku_list: RefCell<Option<Vec<mutsumi::Danmaku>>>,
        #[cfg(target_os = "linux")]
        pub danmaku_search_generation: Cell<u64>,
        #[cfg(target_os = "linux")]
        pub danmaku_anime_candidates: RefCell<Vec<String>>,
        #[cfg(target_os = "linux")]
        pub danmaku_anime_candidate_button: RefCell<Option<gtk::MenuButton>>,
        #[cfg(target_os = "linux")]
        pub danmaku_anime_candidate_popover: RefCell<Option<gtk::Popover>>,
        #[cfg(target_os = "linux")]
        pub danmaku_anime_candidate_list: RefCell<Option<gtk::ListBox>>,
        #[cfg(target_os = "linux")]
        pub danmaku_auto_anime_names: RefCell<HashMap<String, Vec<String>>>,
        #[cfg(target_os = "linux")]
        pub danmaku_manual_anime_names: RefCell<HashMap<String, Vec<String>>>,
        #[cfg(target_os = "linux")]
        pub danmaku_anime_context_history: RefCell<Vec<String>>,

        #[property(get, set, default_value = true)]
        pub can_fade_cursor_set: Cell<bool>,

        /// Tracks the ID of the video currently loaded in mpv.
        /// When `Some(id)`, a video file is loaded and its demuxer cache is preserved.
        /// Used to skip re-fetching the playback URL when resuming the same video.
        pub cached_video_id: RefCell<Option<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MPVPage {
        const NAME: &'static str = "MPVPage";
        type Type = super::MPVPage;
        type ParentType = adw::NavigationPage;

        fn class_init(klass: &mut Self::Class) {
            MPVVideo::ensure_type();
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
            self.can_fade_cursor_set.set(true);

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

            #[cfg(target_os = "linux")]
            {
                SETTINGS
                    .bind("is-danmaku-enabled", &self.danmaku_switch.get(), "active")
                    .build();

                SETTINGS
                    .bind("danmaku-opacity", &self.danmaku_opacity_adj.get(), "value")
                    .build();
                SETTINGS
                    .bind("danmaku-speed", &self.danmaku_speed_adj.get(), "value")
                    .build();
                SETTINGS
                    .bind(
                        "danmaku-font-size",
                        &self.danmaku_font_size_adj.get(),
                        "value",
                    )
                    .build();
                SETTINGS
                    .bind(
                        "danmaku-intensity",
                        &self.danmaku_intensity_adj.get(),
                        "value",
                    )
                    .build();
                SETTINGS
                    .bind(
                        "danmaku-font-weight",
                        &self.danmaku_font_weight_adj.get(),
                        "value",
                    )
                    .build();
                SETTINGS
                    .bind(
                        "danmaku-spacing-factor",
                        &self.danmaku_spacing_factor_adj.get(),
                        "value",
                    )
                    .build();
                SETTINGS
                    .bind(
                        "danmaku-outline-px",
                        &self.danmaku_outline_px_adj.get(),
                        "value",
                    )
                    .build();
                SETTINGS
                    .bind(
                        "danmaku-shadow-offset",
                        &self.danmaku_shadow_offset_adj.get(),
                        "value",
                    )
                    .build();

                let video = self.video.get();
                self.danmaku_opacity_adj.connect_value_changed(glib::clone!(
                    #[weak]
                    video,
                    move |adj| {
                        video.set_danmaku_opacity(adj.value());
                    }
                ));
                video.set_danmaku_opacity(self.danmaku_opacity_adj.get().value());
                self.danmaku_speed_adj.connect_value_changed(glib::clone!(
                    #[weak]
                    video,
                    move |adj| {
                        video.set_danmaku_speed(adj.value());
                    }
                ));
                video.set_danmaku_speed(self.danmaku_speed_adj.get().value());
                self.danmaku_font_size_adj
                    .connect_value_changed(glib::clone!(
                        #[weak]
                        video,
                        move |adj| {
                            video.set_danmaku_font_size(adj.value());
                        }
                    ));
                video.set_danmaku_font_size(self.danmaku_font_size_adj.get().value());
                self.danmaku_intensity_adj
                    .connect_value_changed(glib::clone!(
                        #[weak]
                        video,
                        move |adj| {
                            video.set_danmaku_intensity(adj.value());
                        }
                    ));
                video.set_danmaku_intensity(self.danmaku_intensity_adj.get().value());
                self.danmaku_font_weight_adj
                    .connect_value_changed(glib::clone!(
                        #[weak]
                        video,
                        move |adj| {
                            video.set_danmaku_font_weight(adj.value());
                        }
                    ));
                video.set_danmaku_font_weight(self.danmaku_font_weight_adj.get().value());
                self.danmaku_spacing_factor_adj
                    .connect_value_changed(glib::clone!(
                        #[weak]
                        video,
                        move |adj| {
                            video.set_danmaku_spacing_factor(adj.value());
                        }
                    ));
                video.set_danmaku_spacing_factor(self.danmaku_spacing_factor_adj.get().value());
                self.danmaku_outline_px_adj
                    .connect_value_changed(glib::clone!(
                        #[weak]
                        video,
                        move |adj| {
                            video.set_danmaku_outline_px(adj.value());
                        }
                    ));
                video.set_danmaku_outline_px(self.danmaku_outline_px_adj.get().value());
                self.danmaku_shadow_offset_adj
                    .connect_value_changed(glib::clone!(
                        #[weak]
                        video,
                        move |adj| {
                            video.set_danmaku_shadow_offset(adj.value());
                        }
                    ));
                video.set_danmaku_shadow_offset(self.danmaku_shadow_offset_adj.get().value());
            }

            #[cfg(not(target_os = "linux"))]
            self.danmaku_button.set_visible(false);

            self.video_scale.set_player(Some(&self.video.get()));

            let obj = self.obj();

            #[cfg(target_os = "linux")]
            {
                obj.init_dandanapi_client();
                obj.setup_danmaku_anime_candidate_picker();
                obj.rebuild_danmaku_server_list();

                self.danmaku_popover.connect_show(glib::clone!(
                    #[weak]
                    obj,
                    move |_| {
                        obj.prepare_danmaku_popover();
                    }
                ));
                self.danmaku_popover.connect_closed(glib::clone!(
                    #[weak]
                    obj,
                    move |_| {
                        obj.cancel_danmaku_search();
                    }
                ));
            }

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
    fn init_dandanapi_client(&self) {
        self.apply_active_danmaku_server();

        let appid = SETTINGS.danmaku_appid().trim().to_string();
        let appsecret = SETTINGS.danmaku_appsecret().trim().to_string();
        let using_dandanplay = SETTINGS.danmaku_active_server() < 0;
        let has_default_credentials = is_default_danmaku_credentials(&appid, &appsecret);
        let has_custom_credentials = has_complete_danmaku_credentials(&appid, &appsecret);

        if using_dandanplay && !has_default_credentials && !has_custom_credentials {
            self.set_key_vaild(false);
            self.imp().danmaku_page.set_description(&gettext(
                "Please fill App Secret when using a custom App ID.",
            ));
            return;
        }

        let init_result = if !using_dandanplay && !has_custom_credentials {
            init_danmaku_client_without_credentials()
        } else if has_custom_credentials {
            init_danmaku_client_credentials(&appid, &appsecret)
        } else {
            init_danmaku_client_credentials("", "")
        };

        match init_result {
            Ok(()) => {
                self.set_key_vaild(true);
                self.imp()
                    .danmaku_client
                    .get_or_init(dandanapi::DanDanClient::instance);
            }
            Err(e) => {
                self.set_key_vaild(false);
                self.imp().danmaku_page.set_description(&format!(
                    "{}: {e}",
                    gettext("Danmaku credential is invalid")
                ));
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn dandan_client(&self) -> Result<dandanapi::DanDanClient> {
        if !self.ensure_dandanapi_client() {
            return Err(anyhow::anyhow!("Danmaku client is not initialized"));
        }

        self.imp()
            .danmaku_client
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Danmaku client is not initialized"))
    }

    #[cfg(target_os = "linux")]
    fn ensure_dandanapi_client(&self) -> bool {
        if !self.key_vaild() {
            self.init_dandanapi_client();
        }

        self.key_vaild()
    }

    #[cfg(target_os = "linux")]
    fn apply_active_danmaku_server(&self) {
        apply_danmaku_active_server(
            SETTINGS.danmaku_active_server(),
            &SETTINGS.danmaku_servers(),
        );
    }

    #[cfg(target_os = "linux")]
    fn rebuild_danmaku_server_list(&self) {
        let servers = SETTINGS.danmaku_servers();
        let active = SETTINGS.danmaku_active_server();
        let model =
            gtk::StringList::new(&[gettext(crate::client::DEFAULT_DANMAKU_SERVER_LABEL).as_str()]);

        for server in &servers {
            model.append(&server.name);
        }

        let combo = &self.imp().danmaku_server_combo;
        combo.set_model(Some(&model));
        combo.set_selected(danmaku_server_to_combo_index(active));
    }

    #[cfg(target_os = "linux")]
    fn danmaku_context_key_for_item(item: &TuItem) -> String {
        item.series_id().unwrap_or_else(|| item.id())
    }

    #[cfg(target_os = "linux")]
    fn danmaku_context_key(&self) -> Option<String> {
        self.current_video()
            .map(|item| Self::danmaku_context_key_for_item(&item))
    }

    #[cfg(target_os = "linux")]
    fn push_unique_danmaku_anime_candidate(candidates: &mut Vec<String>, value: &str) {
        let value = value.trim();
        if value.is_empty() || candidates.iter().any(|candidate| candidate == value) {
            return;
        }

        candidates.push(value.to_string());
    }

    #[cfg(target_os = "linux")]
    fn base_danmaku_anime_candidates(&self, item: &TuItem) -> Vec<String> {
        let mut candidates = Vec::new();

        if item.item_type() == "Episode" {
            if let Some(series_name) = item.series_name() {
                Self::push_unique_danmaku_anime_candidate(&mut candidates, &series_name);
            }
        } else {
            Self::push_unique_danmaku_anime_candidate(&mut candidates, &item.name());
        }

        candidates
    }

    #[cfg(target_os = "linux")]
    fn setup_danmaku_anime_candidate_picker(&self) {
        let imp = self.imp();
        if imp.danmaku_anime_candidate_button.borrow().is_some() {
            return;
        }

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
    fn rebuild_danmaku_anime_candidate_picker(&self, candidates: &[String]) {
        let imp = self.imp();
        if let Some(button) = imp.danmaku_anime_candidate_button.borrow().as_ref() {
            button.set_visible(candidates.len() > 1);
        }

        let Some(list) = imp.danmaku_anime_candidate_list.borrow().as_ref().cloned() else {
            return;
        };

        while let Some(child) = list.first_child() {
            list.remove(&child);
        }

        let current_anime = imp.danmaku_anime_row.text().trim().to_string();
        for candidate in candidates {
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(candidate));
            row.set_activatable(true);

            let selected_icon = gtk::Image::from_icon_name("object-select-symbolic");
            selected_icon.set_visible(candidate == &current_anime);
            row.add_suffix(&selected_icon);

            let candidate = candidate.clone();
            row.connect_activated(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                move |_| {
                    obj.imp().danmaku_anime_row.set_text(&candidate);
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
    fn set_danmaku_anime_candidates(&self, candidates: Vec<String>) {
        self.rebuild_danmaku_anime_candidate_picker(&candidates);
        let imp = self.imp();
        imp.danmaku_anime_candidates.replace(candidates);
    }

    #[cfg(target_os = "linux")]
    fn touch_danmaku_anime_context(&self, context_key: &str) {
        let mut context_history = self.imp().danmaku_anime_context_history.borrow_mut();
        context_history.retain(|stored_key| stored_key != context_key);
        context_history.insert(0, context_key.to_string());
        while context_history.len() > DANMAKU_ANIME_CONTEXT_LIMIT {
            if let Some(old_key) = context_history.pop() {
                self.imp()
                    .danmaku_auto_anime_names
                    .borrow_mut()
                    .remove(&old_key);
                self.imp()
                    .danmaku_manual_anime_names
                    .borrow_mut()
                    .remove(&old_key);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn stored_danmaku_anime_candidates_for_context(&self, context_key: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        if let Some(auto_names) = self
            .imp()
            .danmaku_auto_anime_names
            .borrow()
            .get(context_key)
        {
            for name in auto_names {
                Self::push_unique_danmaku_anime_candidate(&mut candidates, name);
            }
        }
        if let Some(manual_names) = self
            .imp()
            .danmaku_manual_anime_names
            .borrow()
            .get(context_key)
        {
            for name in manual_names {
                Self::push_unique_danmaku_anime_candidate(&mut candidates, name);
            }
        }
        candidates
    }

    #[cfg(target_os = "linux")]
    fn remember_danmaku_auto_anime_candidates_for_context(
        &self, context_key: &str, names: impl IntoIterator<Item = String>,
    ) -> Vec<String> {
        {
            let mut auto_names = self.imp().danmaku_auto_anime_names.borrow_mut();
            let history = auto_names.entry(context_key.to_string()).or_default();
            for name in names {
                Self::push_unique_danmaku_anime_candidate(history, &name);
            }
            history.truncate(DANMAKU_AUTO_ANIME_NAME_LIMIT);
        }

        self.touch_danmaku_anime_context(context_key);
        self.stored_danmaku_anime_candidates_for_context(context_key)
    }

    #[cfg(target_os = "linux")]
    fn remember_danmaku_manual_anime_candidate(&self, name: String) -> Vec<String> {
        let Some(context_key) = self.danmaku_context_key() else {
            return Vec::new();
        };

        {
            let mut manual_names = self.imp().danmaku_manual_anime_names.borrow_mut();
            let history = manual_names.entry(context_key.clone()).or_default();
            let name = name.trim();
            if !name.is_empty() {
                history.retain(|stored_name| stored_name != name);
                history.insert(0, name.to_string());
                history.truncate(DANMAKU_MANUAL_ANIME_NAME_LIMIT);
            }
        }

        self.touch_danmaku_anime_context(&context_key);
        self.stored_danmaku_anime_candidates_for_context(&context_key)
    }

    #[cfg(target_os = "linux")]
    fn prepare_danmaku_popover(&self) {
        self.rebuild_danmaku_server_list();

        let Some(item) = self.current_video() else {
            return;
        };
        let context_key = Self::danmaku_context_key_for_item(&item);

        let is_movie = item.item_type() == "Movie";
        let episode = if is_movie {
            String::new()
        } else if item.index_number() > 0 {
            item.index_number().to_string()
        } else {
            item.name()
        };
        self.imp().danmaku_episode_row.set_text(&episode);

        let candidates = self.remember_danmaku_auto_anime_candidates_for_context(
            &context_key,
            self.base_danmaku_anime_candidates(&item),
        );
        self.imp()
            .danmaku_anime_row
            .set_text(candidates.first().map(String::as_str).unwrap_or_default());
        self.set_danmaku_anime_candidates(candidates);

        self.imp().danmaku_tmdb_row.set_text("");

        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                if let Some(tmdb_id) = obj.resolve_tmdb_id(item).await {
                    if obj.danmaku_context_key().as_deref() != Some(context_key.as_str()) {
                        return;
                    }

                    obj.imp().danmaku_tmdb_row.set_text(&tmdb_id.to_string());
                    let titles = obj.fetch_dandanplay_anime_title_candidates(tmdb_id).await;
                    if obj.danmaku_context_key().as_deref() != Some(context_key.as_str()) {
                        return;
                    }

                    let candidates = obj
                        .remember_danmaku_auto_anime_candidates_for_context(&context_key, titles);
                    obj.set_danmaku_anime_candidates(candidates);
                }
            }
        ));
    }

    #[cfg(target_os = "linux")]
    async fn resolve_tmdb_id(&self, item: TuItem) -> Option<i32> {
        let mut tmdb_id = None;
        let is_episode = item.item_type() == "Episode";

        if tmdb_id.is_none() && !is_episode {
            let id = item.id();
            if let Ok(info) =
                spawn_tokio(async move { JELLYFIN_CLIENT.get_item_info(&id).await }).await
            {
                tmdb_id = info
                    .provider_ids
                    .and_then(|ids| ids.tmdb)
                    .and_then(|id| id.parse::<i32>().ok());
            }
        }

        if tmdb_id.is_none()
            && let Some(season_id) = item.season_id()
        {
            let season_id = season_id.clone();
            if let Ok(info) =
                spawn_tokio(async move { JELLYFIN_CLIENT.get_item_info(&season_id).await }).await
            {
                tmdb_id = info
                    .provider_ids
                    .and_then(|ids| ids.tmdb)
                    .and_then(|id| id.parse::<i32>().ok());
            }
        }

        if tmdb_id.is_none()
            && let Some(series_id) = item.series_id()
        {
            let series_id = series_id.clone();
            if let Ok(info) =
                spawn_tokio(async move { JELLYFIN_CLIENT.get_item_info(&series_id).await }).await
            {
                tmdb_id = info
                    .provider_ids
                    .and_then(|ids| ids.tmdb)
                    .and_then(|id| id.parse::<i32>().ok());
            }
        }

        tmdb_id
    }

    #[cfg(target_os = "linux")]
    pub async fn load_danmaku(&self) {
        if !SETTINGS.is_danmaku_enabled() {
            return;
        }

        if !self.ensure_dandanapi_client() {
            self.imp().danmaku_page.set_description(&gettext(
                "Danmaku credential is invalid. Set App ID and App Secret to continue.",
            ));
            return;
        }

        let Some(item) = self.current_video() else {
            return;
        };

        let is_movie = item.item_type() == "Movie";
        let anime = item.series_name().unwrap_or_else(|| item.name());
        let episode = if is_movie {
            String::new()
        } else if item.index_number() > 0 {
            item.index_number().to_string()
        } else {
            item.name()
        };
        let video_id = item.id();
        let tmdb_id = self.resolve_tmdb_id(item).await;
        let search_anime = if tmdb_id.is_some() {
            String::new()
        } else {
            anime
        };
        let request = DanmakuEpisodeSearchParams::new(search_anime, episode, tmdb_id);

        tracing::info!(
            anime = ?request.anime.as_deref(),
            episode = ?request.episode.as_deref(),
            tmdb_id = ?request.tmdb_id,
            "Auto danmaku search"
        );

        match self.search_danmaku_episodes(request).await {
            Ok(response) => {
                if self.danmaku_request_is_stale(Some(&video_id)) {
                    return;
                }

                let episode_id = response
                    .animes
                    .and_then(|animes| animes.first()?.episodes.first().map(|ep| ep.episode_id));

                if let Some(episode_id) = episode_id {
                    self.load_danmaku_episode_for_video(episode_id, Some(video_id))
                        .await;
                } else {
                    self.clear_danmaku_items(&gettext("No Danmaku Loaded"));
                }
            }
            Err(e) => {
                tracing::error!("Loading danmaku error: {e}");
                if self.danmaku_request_is_stale(Some(&video_id)) {
                    return;
                }

                self.clear_danmaku_items(&gettext("No Danmaku Loaded"));
            }
        }
    }

    #[cfg(target_os = "linux")]
    async fn search_danmaku_episodes(
        &self, request: DanmakuEpisodeSearchParams,
    ) -> Result<dandanapi::SearchEpisodesResponse> {
        let client = self.dandan_client()?;
        let route = client.route(DanmakuEpisodes(request));
        spawn_tokio(async move { Ok(route.await?) }).await
    }

    #[cfg(target_os = "linux")]
    async fn request_danmaku(&self, episode_id: i64) -> Result<Vec<mutsumi::Danmaku>> {
        let client = self.dandan_client()?;
        let route = client.route(dandanapi::Comments {
            episode_id,
            request_comments: dandanapi::RequestComments {
                from: 0,
                with_related: true,
                ch_convert: dandanapi::ChConvert::NONE,
            },
        });

        spawn_tokio(async move {
            let comments = route
                .await?
                .comments
                .ok_or_else(|| anyhow::anyhow!("No comment found"))?;
            Ok::<_, anyhow::Error>(
                comments
                    .into_iter()
                    .map(|comment| comment.into_danmaku())
                    .collect(),
            )
        })
        .await
    }

    #[cfg(target_os = "linux")]
    async fn load_danmaku_episode(&self, episode_id: i64) {
        self.load_danmaku_episode_for_video(episode_id, None).await;
    }

    #[cfg(target_os = "linux")]
    async fn load_danmaku_episode_for_video(&self, episode_id: i64, video_id: Option<String>) {
        match self.request_danmaku(episode_id).await {
            Ok(danmaku) => {
                if self.danmaku_request_is_stale(video_id.as_deref()) {
                    return;
                }

                let count = danmaku.len();
                self.imp().danmaku_list.replace(Some(danmaku.clone()));
                self.imp().video.load_danmaku_items(danmaku);
                self.imp().danmaku_page.set_description(&format!(
                    "{} {}",
                    count,
                    gettext("Danmaku Loaded")
                ));
            }
            Err(e) => {
                tracing::error!("Danmaku load error: {e}");
                if self.danmaku_request_is_stale(video_id.as_deref()) {
                    return;
                }

                self.clear_danmaku_items(&gettext("Failed to load danmaku"));
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn danmaku_request_is_stale(&self, video_id: Option<&str>) -> bool {
        video_id.is_some_and(|video_id| {
            self.current_video()
                .is_none_or(|item| item.id() != video_id)
        })
    }

    #[cfg(target_os = "linux")]
    fn clear_danmaku_items(&self, description: &str) {
        self.imp().danmaku_list.replace(None);
        self.imp().video.load_danmaku_items(Vec::new());
        self.imp().danmaku_page.set_description(description);
    }

    #[cfg(target_os = "linux")]
    async fn fetch_dandanplay_anime_title_candidates(&self, tmdb_id: i32) -> Vec<String> {
        let Ok(client) = self.dandan_client() else {
            return Vec::new();
        };
        let route = client.route(DanmakuEpisodes(DanmakuEpisodeSearchParams::new(
            String::new(),
            String::new(),
            Some(tmdb_id),
        )));

        spawn_tokio(async move {
            let response = route.await?;
            Ok::<_, anyhow::Error>(
                response
                    .animes
                    .unwrap_or_default()
                    .into_iter()
                    .map(|anime| anime.anime_title)
                    .unique()
                    .collect(),
            )
        })
        .await
        .unwrap_or_default()
    }

    #[template_callback]
    #[cfg(target_os = "linux")]
    fn on_danmaku_switch_state_set(&self, state: bool) -> bool {
        let _ = SETTINGS.set_danmaku_enabled(state);

        if state {
            if let Some(danmaku) = self.imp().danmaku_list.borrow().as_ref() {
                self.imp().video.load_danmaku_items(danmaku.clone());
            } else {
                spawn(glib::clone!(
                    #[weak(rename_to = obj)]
                    self,
                    async move {
                        obj.load_danmaku().await;
                    }
                ));
            }
        } else {
            self.imp().video.load_danmaku_items(Vec::new());
        }

        false
    }

    #[template_callback]
    #[cfg(target_os = "linux")]
    fn on_danmaku_anime_candidate_changed(&self, _pspec: glib::ParamSpec) {}

    #[template_callback]
    #[cfg(target_os = "linux")]
    async fn on_danmaku_search_clicked(&self) {
        if !self.ensure_dandanapi_client() {
            self.imp().danmaku_page.set_description(&gettext(
                "Danmaku credential is invalid. Set App ID and App Secret to continue.",
            ));
            return;
        }

        if self.current_video().is_none() {
            self.toast(gettext("No video is currently playing"));
            return;
        }

        let anime = self.imp().danmaku_anime_row.text().trim().to_string();
        let episode = self.imp().danmaku_episode_row.text().trim().to_string();
        let tmdb_id = self
            .imp()
            .danmaku_tmdb_row
            .text()
            .trim()
            .parse::<i32>()
            .ok();
        let request = DanmakuEpisodeSearchParams::new(anime.clone(), episode, tmdb_id);

        tracing::info!(
            anime = ?request.anime.as_deref(),
            episode = ?request.episode.as_deref(),
            tmdb_id = ?request.tmdb_id,
            "Manual danmaku search"
        );
        let search_generation = self.next_danmaku_search_generation();
        let candidates = self.remember_danmaku_manual_anime_candidate(anime.clone());
        self.set_danmaku_anime_candidates(candidates);

        match self.search_danmaku_episodes(request).await {
            Ok(response) => {
                if self.danmaku_search_is_stale(search_generation) {
                    return;
                }

                self.close_danmaku_popover();
                self.show_danmaku_selection_dialog(response);
            }
            Err(e) => {
                tracing::error!("Manual danmaku search error: {e}");
                if self.danmaku_search_is_stale(search_generation) {
                    return;
                }

                self.close_danmaku_popover();
                self.clear_danmaku_items(&gettext("No Danmaku Loaded"));
                self.toast(gettext("No Danmaku Loaded"));
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn next_danmaku_search_generation(&self) -> u64 {
        let generation = self.imp().danmaku_search_generation.get().wrapping_add(1);
        self.imp().danmaku_search_generation.set(generation);
        generation
    }

    #[cfg(target_os = "linux")]
    fn cancel_danmaku_search(&self) {
        let generation = self.imp().danmaku_search_generation.get().wrapping_add(1);
        self.imp().danmaku_search_generation.set(generation);
    }

    #[cfg(target_os = "linux")]
    fn danmaku_search_is_stale(&self, generation: u64) -> bool {
        self.imp().danmaku_search_generation.get() != generation
    }

    #[cfg(target_os = "linux")]
    fn close_danmaku_popover(&self) {
        let imp = self.imp();
        imp.danmaku_popover.popdown();
        imp.danmaku_button.set_active(false);
    }

    #[cfg(target_os = "linux")]
    fn show_danmaku_selection_dialog(&self, response: dandanapi::SearchEpisodesResponse) {
        let animes = match response.animes {
            Some(animes) => animes,
            None => {
                self.toast(gettext("No results found"));
                return;
            }
        };

        let total_episodes = animes
            .iter()
            .map(|anime| anime.episodes.len())
            .sum::<usize>();
        if total_episodes == 0 {
            self.toast(gettext("No episodes found"));
            return;
        }

        if animes.len() == 1 && animes[0].episodes.len() == 1 {
            let episode_id = animes[0].episodes[0].episode_id;
            spawn(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                async move {
                    obj.load_danmaku_episode(episode_id).await;
                }
            ));
            return;
        }

        let dialog = adw::Dialog::new();
        dialog.set_title(&gettext("Select Danmaku Source"));
        dialog.set_content_width(480);
        dialog.set_content_height(600);
        self.set_can_fade_cursor_set(false);
        self.set_reveal_overlay(true);
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            move |_| {
                obj.set_can_fade_cursor_set(true);
                obj.reset_fade_timeout();
            }
        ));

        let toolbar_view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        header.set_show_end_title_buttons(false);
        header.set_show_start_title_buttons(false);
        toolbar_view.add_top_bar(&header);

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);

        let preferences_page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();

        let single_season = animes.len() == 1;
        let selected_episode_id: Rc<Cell<Option<i64>>> = Rc::new(Cell::new(None));
        let selected_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

        for anime in &animes {
            let expander = adw::ExpanderRow::new();
            expander.set_expanded(single_season);
            expander.set_title(&glib::markup_escape_text(&anime.anime_title));

            let type_desc =
                glib::markup_escape_text(anime.type_description.as_deref().unwrap_or("Season"));
            let count = anime.episodes.len();
            expander.set_subtitle(&format!(
                "{} - {} {}",
                type_desc,
                count,
                if count > 1 { "episodes" } else { "episode" }
            ));

            for episode in &anime.episodes {
                let row = adw::ActionRow::new();
                let episode_title = episode
                    .episode_title
                    .clone()
                    .unwrap_or_else(|| format!("Episode {}", episode.episode_id));
                row.set_title(&glib::markup_escape_text(&episode_title));
                row.set_activatable(true);

                let episode_id = episode.episode_id;
                let selected_episode_id = selected_episode_id.clone();
                let all_rows = selected_rows.clone();
                row.connect_activated(move |row| {
                    selected_episode_id.set(Some(episode_id));
                    for selected_row in all_rows.borrow().iter() {
                        selected_row.remove_css_class("danmaku-episode-selected");
                    }
                    row.add_css_class("danmaku-episode-selected");
                });

                selected_rows.borrow_mut().push(row.clone());
                expander.add_row(&row);
            }

            group.add(&expander);
        }

        preferences_page.add(&group);
        scrolled.set_child(Some(&preferences_page));
        toolbar_view.set_content(Some(&scrolled));

        let load_button = gtk::Button::builder()
            .label(gettext("Load Danmaku"))
            .css_classes(["suggested-action"])
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();

        let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        button_box.set_halign(gtk::Align::Center);
        button_box.append(&load_button);
        toolbar_view.add_bottom_bar(&button_box);
        dialog.set_child(Some(&toolbar_view));

        let css_provider = gtk::CssProvider::new();
        css_provider.load_from_string(
            ".danmaku-episode-selected { background-color: alpha(@accent_bg_color, 0.15); }",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css_provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        load_button.connect_clicked(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            #[weak]
            dialog,
            move |_| {
                if let Some(episode_id) = selected_episode_id.get() {
                    dialog.close();
                    spawn(glib::clone!(
                        #[weak(rename_to = obj)]
                        obj,
                        async move {
                            obj.load_danmaku_episode(episode_id).await;
                        }
                    ));
                }
            }
        ));

        if let Some(window) = self.root().and_downcast::<gtk::Window>() {
            dialog.present(Some(&window));
        }
    }

    #[template_callback]
    #[cfg(target_os = "linux")]
    fn on_danmaku_server_combo_changed(&self, _pspec: glib::ParamSpec) {
        let selected = self.imp().danmaku_server_combo.selected();
        let active = danmaku_combo_to_server_index(selected);
        if SETTINGS.danmaku_active_server() == active {
            return;
        }

        let _ = SETTINGS.set_danmaku_active_server(active);
        self.init_dandanapi_client();
    }

    #[template_callback]
    #[cfg(not(target_os = "linux"))]
    fn on_danmaku_switch_state_set(&self, _state: bool) -> bool {
        false
    }

    #[template_callback]
    #[cfg(not(target_os = "linux"))]
    fn on_danmaku_anime_candidate_changed(&self, _pspec: glib::ParamSpec) {}

    #[template_callback]
    #[cfg(not(target_os = "linux"))]
    async fn on_danmaku_search_clicked(&self) {}

    #[template_callback]
    #[cfg(not(target_os = "linux"))]
    fn on_danmaku_server_combo_changed(&self, _pspec: glib::ParamSpec) {}

    fn mark_stream_failed(&self) {
        let imp = self.imp();
        imp.allow_fallback.set(false);
        imp.loading_box.set_visible(false);
        imp.spinner.set_visible(false);
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
        // Clear the cached video ID since we're about to unload the file.
        // This ensures play() takes the full-reload path rather than the
        // fast resume path (which would fail because mpv.stop() unloaded it).
        self.imp().cached_video_id.replace(None);
        self.imp().video.stop();

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
        #[cfg(target_os = "linux")]
        let should_clear_danmaku = self.current_video().is_none_or(|video| video.id() != id);
        #[cfg(target_os = "linux")]
        if should_clear_danmaku {
            self.imp().danmaku_list.replace(None);
            self.imp().video.clear_danmaku();
            self.imp()
                .danmaku_page
                .set_description(&gettext("No danmakus loaded"));
        }

        self.imp().video_scale.reset_scale();
        self.imp().last_playback_position.set(start_seconds);
        self.reset_skippable_segments();

        // If the video_matcher is None, field wont be updated
        if let Some(video_matcher) = video_matcher {
            self.imp()
                .video_version_matcher
                .replace(Some(video_matcher));
        }

        #[cfg(target_os = "linux")]
        let track_list_changed = self.mpris_track_list_changed(&episode_list);

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

        spawn_g_timeout(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let imp = obj.imp();

                // Fast path: if the same video is already loaded in mpv (cache preserved),
                // skip the network round-trips and just seek to the desired position.
                {
                    let cached = imp.cached_video_id.borrow();
                    if cached.as_deref() == Some(&id) && imp.video.has_loaded_video() {
                        imp.spinner.set_visible(false);
                        imp.loading_box.set_visible(false);
                        imp.video.resume_cached(start_seconds);
                        imp.video.refresh_danmaku_timeline();
                        return;
                    }
                }
                // Different video (or first playback): clear cached ID, full load.
                #[cfg(target_os = "linux")]
                {
                    imp.danmaku_list.replace(None);
                    imp.video.clear_danmaku();
                    imp.danmaku_page
                        .set_description(&gettext("No danmakus loaded"));
                }
                imp.cached_video_id.replace(None);

                imp.spinner.set_visible(true);
                imp.loading_box.set_visible(true);
                imp.network_speed_label
                    .set_text(&gettext("Initializing..."));

                let sub_stream_index = selected.as_ref().map(|s| s.sub_index);
                let media_source_id = selected.as_ref().map(|s| s.media_source_id.clone());
                let id_clone = id.to_owned();
                let playback_info = match spawn_tokio(async move {
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
                .await
                {
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

    fn set_audio_and_video_tracks_dropdown(&self, value: MpvTracks) {
        let imp = self.imp();
        imp.video.load_danmaku_track(value.danmaku_track);
        self.bind_tracks(
            value.audio_tracks,
            &imp.audio_listbox.get(),
            MpvTrackKind::Audio,
        );
        self.bind_tracks(
            value.sub_tracks,
            &imp.sub_listbox.get(),
            MpvTrackKind::Subtitle,
        );
    }

    // TODO: Use GAction instead of listening to each button
    fn bind_tracks(&self, tracks: Vec<MpvTrack>, listbox: &gtk::ListBox, kind: MpvTrackKind) {
        while let Some(row) = listbox.first_child() {
            listbox.remove(&row);
        }

        let track_id = self.imp().video.get_track_id(kind.property());

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
                        ListenEvent::PausedForCache(true) | ListenEvent::Seek => {
                            obj.update_seeking(true);
                        }
                        ListenEvent::PausedForCache(false) | ListenEvent::PlaybackRestart => {
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
                            obj.on_pause_update(value);
                        }
                        ListenEvent::CacheSpeed(value) => {
                            obj.on_cache_speed_update(value);
                        }
                        ListenEvent::FileLoaded => {
                            obj.on_file_loaded();
                        }
                        ListenEvent::TrackList(value) => {
                            obj.set_audio_and_video_tracks_dropdown(value);
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
                        ListenEvent::DanmakuTimeline(value) => {
                            obj.imp().video_scale.set_danmaku_timeline(value);
                        }
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
        imp.video.mark_loaded();
        imp.allow_fallback.set(false);
        // Mark the newly loaded video as cached so we can resume it later
        // without re-fetching from the network.
        imp.cached_video_id
            .replace(self.current_video().map(|v| v.id()));
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
                async move {
                    obj.load_danmaku().await;
                }
            ));
        }
    }

    fn update_seeking(&self, seeking: bool) {
        self.imp().seeking.replace(seeking);
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
        *self.imp().seeking.borrow()
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
        let old_x = *self.x();
        let old_y = *self.y();

        if old_x == x && old_y == y {
            return;
        }

        let imp = self.imp();

        *imp.x.borrow_mut() = x;
        *imp.y.borrow_mut() = y;

        let now = glib::monotonic_time();

        if now - *self.last_motion_time() < MIN_MOTION_TIME {
            return;
        }

        let is_threshold = (old_x - x).abs() > 3.0 || (old_y - y).abs() > 3.0;

        if is_threshold {
            if !self.toolbar_revealed() {
                self.set_reveal_overlay(true);
            }

            self.reset_fade_timeout();

            *imp.last_motion_time.borrow_mut() = now;
        }
    }

    #[template_callback]
    fn on_leave(&self) {
        let imp = self.imp();
        *imp.x.borrow_mut() = -1.0;
        *imp.y.borrow_mut() = -1.0;

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

    fn x(&self) -> impl std::ops::Deref<Target = f64> + '_ {
        self.imp().x.borrow()
    }

    fn y(&self) -> impl std::ops::Deref<Target = f64> + '_ {
        self.imp().y.borrow()
    }

    fn last_motion_time(&self) -> impl std::ops::Deref<Target = i64> + '_ {
        self.imp().last_motion_time.borrow()
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
        let x = *self.x();
        let y = *self.y();
        if x >= 0.0 && y >= 0.0 {
            let widget = self.pick(x, y, gtk::PickFlags::DEFAULT);
            if let Some(widget) = widget
                && widget
                    .ancestor(MPVVideo::static_type())
                    .and_downcast::<MPVVideo>()
                    .is_none()
            {
                return false;
            }
        }

        let imp = self.imp();
        if imp.audio_tracks_menu_button.is_active()
            || imp.subtitle_tracks_menu_button.is_active()
            || imp.volume_button.is_active()
            || imp.danmaku_button.is_active()
            || !self.can_fade_cursor_set()
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
            let Some(pixbuf) =
                gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 1, 1)
            else {
                return;
            };
            pixbuf.fill(0);
            let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
            Some(gtk::gdk::Cursor::from_texture(&texture, 0, 0, None))
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

        self.imp().video.set_pause(true);
        self.imp().video.pause_cache_fill();
        // Keep the video loaded in mpv to preserve the demuxer cache.
        // Previously mpv.stop() was called here, which unloaded the file
        // and discarded the entire buffer, forcing a full re-buffer on resume.
        // Now we pause playback and suspend forward cache fill; the existing
        // cache is reused when playback resumes.
        let imp = self.imp();
        imp.cached_video_id
            .replace(self.current_video().map(|v| v.id()));
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
                popover.set_parent(self);
                popover.add_child(&imp.menu_actions, "menu-actions");
                let _ = imp.popover.replace(Some(popover));
            }
            None => eprintln!("Failed to load popover"),
        }
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
