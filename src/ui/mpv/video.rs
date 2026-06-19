use std::cell::{
    Cell,
    OnceCell,
};

use adw::{
    prelude::*,
    subclass::prelude::*,
};
use glib::Object;
use gtk::glib;
use libmpv2::SetData;
use tracing::info;

use super::{
    mpvglarea::MPVGLArea,
    options_matcher::{
        match_audio_channels,
        match_hwdec_interop,
        match_video_upscale,
    },
    tsukimi_mpv::{
        Chapter,
        ChapterList,
        ListenEvent,
        MPV_EVENT_CHANNEL,
        MpvTrack,
        MpvTracks,
        TrackSelection,
        should_use_mutsumi_embed,
    },
};
use crate::{
    USER_AGENT,
    client::{
        jellyfin_client::JELLYFIN_CLIENT,
        proxy::get_proxy_settings,
    },
    ui::models::SETTINGS,
    utils::spawn,
};

#[cfg(target_os = "linux")]
use mutsumi::MutsumiVideoPlayer;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct MPVVideo {
        pub libmpv: OnceCell<MPVGLArea>,
        #[cfg(target_os = "linux")]
        pub mutsumi: OnceCell<MutsumiVideoPlayer>,
        pub last_position: Cell<f64>,
        pub paused: Cell<bool>,
        pub current_audio_id: Cell<i64>,
        pub current_subtitle_id: Cell<i64>,
        pub pending_file_loaded: Cell<bool>,
        pub loaded: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MPVVideo {
        const NAME: &'static str = "MPVVideo";
        type Type = super::MPVVideo;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for MPVVideo {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            obj.set_hexpand(true);
            obj.set_vexpand(true);

            if obj.uses_mutsumi_embed() {
                #[cfg(target_os = "linux")]
                {
                    let player = MutsumiVideoPlayer::new();
                    configure_mutsumi_player(&player);
                    obj.set_child(Some(&player));
                    tracing::info!("Using mutsumi embedded video backend");
                    self.mutsumi
                        .set(player).expect("mutsumi backend already set");
                    obj.listen_mutsumi_events();
                    return;
                }
            }

            let player = MPVGLArea::new();
            obj.set_child(Some(&player));
            tracing::info!("Using libmpv GLArea video backend");
            self.libmpv.set(player).expect("libmpv backend already set");
        }

        fn dispose(&self) {
            #[cfg(target_os = "linux")]
            if let Some(player) = self.mutsumi.get() {
                player.shutdown();
            }
        }
    }

    impl WidgetImpl for MPVVideo {}
    impl BinImpl for MPVVideo {}
}

glib::wrapper! {
    pub struct MPVVideo(ObjectSubclass<imp::MPVVideo>)
        @extends gtk::Widget, adw::Bin,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for MPVVideo {
    fn default() -> Self {
        Self::new()
    }
}

impl MPVVideo {
    pub fn new() -> Self {
        Object::builder().build()
    }

    pub fn ensure_type() {
        let _ = Self::static_type();
    }

    pub fn uses_mutsumi_embed(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            should_use_mutsumi_embed()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn libmpv(&self) -> Option<&MPVGLArea> {
        self.imp().libmpv.get()
    }

    #[cfg(target_os = "linux")]
    fn mutsumi(&self) -> Option<&MutsumiVideoPlayer> {
        self.imp().mutsumi.get()
    }

    pub fn play(&self, url: &str, start_seconds: f64) {
        self.imp().last_position.set(start_seconds);
        self.imp().pending_file_loaded.set(true);
        self.imp().loaded.set(false);

        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            let url = url.to_owned();
            spawn(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                #[weak]
                player,
                async move {
                    let url = JELLYFIN_CLIENT.get_streaming_url(&url).await;
                    info!("Now Playing (mutsumi embed): {}", url);
                    player.set_start_time(start_seconds as u64);
                    player.load_video(&url);
                    player.pause(false);
                    obj.imp().paused.set(false);
                }
            ));
            return;
        }

        if let Some(player) = self.libmpv() {
            player.play(url, start_seconds);
        }
    }

    pub fn resume_cached(&self, start_seconds: f64) {
        if !self.has_loaded_video() {
            tracing::warn!("Ignoring cached resume without a loaded video");
            return;
        }

        self.imp().last_position.set(start_seconds);

        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.set_position(start_seconds);
            player.pause(false);
            self.imp().paused.set(false);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.resume_cached(start_seconds);
        }
    }

    pub fn stop(&self) {
        self.imp().loaded.set(false);

        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.stop();
            return;
        }

        if let Some(player) = self.libmpv() {
            player.stop();
        }
    }

    pub fn add_sub(&self, url: &str) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.add_sub(url);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.add_sub(url);
        }
    }

    pub fn seek_forward(&self, value: i64) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.seek_forward(value);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.seek_forward(value);
        }
    }

    pub fn seek_backward(&self, value: i64) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.seek_backward(value);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.seek_backward(value);
        }
    }

    pub fn set_position(&self, value: f64) {
        if !self.has_loaded_video() {
            tracing::debug!("Ignoring seek before video is loaded");
            return;
        }

        self.imp().last_position.set(value);

        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.set_position(value);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.set_position(value);
        }
    }

    pub fn position(&self) -> f64 {
        if self.mutsumi_active() {
            return self.imp().last_position.get();
        }

        self.libmpv().map(MPVGLArea::position).unwrap_or_default()
    }

    pub fn set_aid(&self, value: TrackSelection) {
        self.imp().current_audio_id.set(selection_id(value));

        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.set_aid(to_mutsumi_selection(value));
            return;
        }

        if let Some(player) = self.libmpv() {
            player.set_aid(value);
        }
    }

    pub fn get_track_id(&self, property: &str) -> i64 {
        if self.mutsumi_active() {
            return match property {
                "aid" => self.imp().current_audio_id.get(),
                "sid" => self.imp().current_subtitle_id.get(),
                _ => 0,
            };
        }

        self.libmpv()
            .map(|player| player.get_track_id(property))
            .unwrap_or_default()
    }

    pub fn set_sid(&self, value: TrackSelection) {
        self.imp().current_subtitle_id.set(selection_id(value));

        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.set_sid(to_mutsumi_selection(value));
            return;
        }

        if let Some(player) = self.libmpv() {
            player.set_sid(value);
        }
    }

    pub fn press_key(&self, key: u32, state: gtk::gdk::ModifierType) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.press_key(key, state);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.press_key(key, state);
        }
    }

    pub fn release_key(&self, key: u32, state: gtk::gdk::ModifierType) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.release_key(key, state);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.release_key(key, state);
        }
    }

    pub fn set_speed(&self, value: f64) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.set_speed(value);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.set_speed(value);
        }
    }

    pub fn set_volume(&self, value: i64) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.set_volume(value);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.set_volume(value);
        }
    }

    pub fn display_stats_toggle(&self) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.display_stats_toggle();
            return;
        }

        if let Some(player) = self.libmpv() {
            player.display_stats_toggle();
        }
    }

    pub fn paused(&self) -> bool {
        if self.mutsumi_active() {
            return self.imp().paused.get();
        }

        self.libmpv().is_none_or(MPVGLArea::paused)
    }

    pub fn pause(&self) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.command_pause();
            self.imp().paused.set(!self.imp().paused.get());
            return;
        }

        if let Some(player) = self.libmpv() {
            player.pause();
        }
    }

    pub fn set_pause(&self, value: bool) {
        self.imp().paused.set(value);

        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.pause(value);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.set_pause(value);
        }
    }

    pub fn volume_scroll(&self, value: i64) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.volume_scroll(value);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.volume_scroll(value);
        }
    }

    pub fn set_slang(&self, value: String) {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            player.set_slang(value);
            return;
        }

        if let Some(player) = self.libmpv() {
            player.set_slang(value);
        }
    }

    pub fn set_property<V>(&self, property: &str, value: V)
    where
        V: SetData + Send + ToString + 'static,
    {
        #[cfg(target_os = "linux")]
        if let Some(player) = self.mutsumi() {
            match property {
                "aid" => self.set_aid(track_selection_from_string(&value.to_string())),
                "sid" => self.set_sid(track_selection_from_string(&value.to_string())),
                _ => player
                    .backend_ref()
                    .mpv()
                    .mpv
                    .set_property(property, value.to_string()),
            }
            return;
        }

        if let Some(player) = self.libmpv() {
            player.set_property(property, value);
        }
    }

    pub fn mark_loaded(&self) {
        self.imp().loaded.set(true);
    }

    pub fn has_loaded_video(&self) -> bool {
        self.imp().loaded.get()
    }

    fn mutsumi_active(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.imp().mutsumi.get().is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    #[cfg(target_os = "linux")]
    fn listen_mutsumi_events(&self) {
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                while let Ok(event) = mutsumi::MPV_EVENT_CHANNEL.rx.recv_async().await {
                    obj.handle_mutsumi_event(event);
                }
            }
        ));
    }

    #[cfg(target_os = "linux")]
    fn handle_mutsumi_event(&self, event: mutsumi::ListenEvent) {
        match event {
            mutsumi::ListenEvent::Seek(_) => {
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Seek);
            }
            mutsumi::ListenEvent::PlaybackRestart(position_ms) => {
                self.imp().last_position.set(position_ms / 1000.0);
                if self.imp().pending_file_loaded.replace(false) {
                    self.mark_loaded();
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::FileLoaded);
                }
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::PlaybackRestart);
            }
            mutsumi::ListenEvent::Eof(reason) => {
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Eof(reason));
            }
            mutsumi::ListenEvent::StartFile => {}
            mutsumi::ListenEvent::Duration(duration) => {
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Duration(duration));
            }
            mutsumi::ListenEvent::Pause(paused) => {
                self.imp().paused.set(paused);
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Pause(paused));
            }
            mutsumi::ListenEvent::CacheSpeed(speed) => {
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::CacheSpeed(speed));
            }
            mutsumi::ListenEvent::Error(error) => {
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Error(error));
            }
            mutsumi::ListenEvent::TrackList(tracks) => {
                let _ = MPV_EVENT_CHANNEL
                    .tx
                    .send(ListenEvent::TrackList(convert_mutsumi_tracks(tracks)));
            }
            mutsumi::ListenEvent::Volume(volume) => {
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Volume(volume));
            }
            mutsumi::ListenEvent::Speed(speed) => {
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Speed(speed));
            }
            mutsumi::ListenEvent::Shutdown => {
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Shutdown);
            }
            mutsumi::ListenEvent::DemuxerCacheTime(time) => {
                let _ = MPV_EVENT_CHANNEL
                    .tx
                    .send(ListenEvent::DemuxerCacheTime(time));
            }
            mutsumi::ListenEvent::TimePos(position) => {
                self.imp().last_position.set(position as f64);
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::TimePos(position));
            }
            mutsumi::ListenEvent::PausedForCache(paused, _) => {
                let _ = MPV_EVENT_CHANNEL
                    .tx
                    .send(ListenEvent::PausedForCache(paused));
            }
            mutsumi::ListenEvent::ChapterList(chapters) => {
                let _ = MPV_EVENT_CHANNEL
                    .tx
                    .send(ListenEvent::ChapterList(convert_mutsumi_chapters(chapters)));
            }
            // Tsukimi does not expose playlist updates through its internal MPV event channel.
            mutsumi::ListenEvent::Playlist(_) => {}
        }
    }
}

#[cfg(target_os = "linux")]
fn to_mutsumi_selection(value: TrackSelection) -> mutsumi::TrackSelection {
    match value {
        TrackSelection::Track(id) => mutsumi::TrackSelection::Track(id),
        TrackSelection::None => mutsumi::TrackSelection::None,
    }
}

fn selection_id(value: TrackSelection) -> i64 {
    match value {
        TrackSelection::Track(id) => id,
        TrackSelection::None => 0,
    }
}

fn track_selection_from_string(value: &str) -> TrackSelection {
    if value == "no" {
        TrackSelection::None
    } else {
        value
            .parse()
            .map(TrackSelection::Track)
            .unwrap_or(TrackSelection::None)
    }
}

#[cfg(target_os = "linux")]
fn configure_mutsumi_player(player: &MutsumiVideoPlayer) {
    let vo = match SETTINGS.mpv_video_output() {
        2 => "dmabuf-wayland",
        _ => "gpu-next",
    };

    let mpv = &player.backend_ref().mpv().mpv;
    mpv.set_property("vo", vo.to_string());
    mpv.set_property("user-agent", USER_AGENT.to_string());
    mpv.set_property("video-timing-offset", "0".to_string());
    mpv.set_property("video-sync", "audio".to_string());
    mpv.set_property("volume-max", "100".to_string());
    mpv.set_property("volume", SETTINGS.mpv_default_volume().to_string());
    mpv.set_property("sub-font-size", SETTINGS.mpv_subtitle_size().to_string());
    mpv.set_property("sub-font", SETTINGS.mpv_subtitle_font());
    mpv.set_property("sub-scale", SETTINGS.mpv_subtitle_scale().to_string());
    mpv.set_property(
        "hwdec",
        match_hwdec_interop(SETTINGS.mpv_hwdec()).to_string(),
    );
    mpv.set_property(
        "scale",
        match_video_upscale(SETTINGS.mpv_video_scale()).to_string(),
    );
    mpv.set_property(
        "demuxer-max-bytes",
        format!("{}MiB", SETTINGS.mpv_cache_size()),
    );
    mpv.set_property("cache-secs", SETTINGS.mpv_cache_time().to_string());
    mpv.set_property(
        "loop",
        if SETTINGS.mpv_action_after_video_end() == 1 {
            "inf".to_string()
        } else {
            "no".to_string()
        },
    );
    mpv.set_property(
        "audio-channels",
        match_audio_channels(SETTINGS.mpv_audio_channel()).to_string(),
    );

    if let Some(uri) = get_proxy_settings() {
        let url = url::Url::parse(&uri).map_or_else(|_| format!("http://{uri}"), |_| uri.clone());
        mpv.set_property("http-proxy", url);
    }
}

#[cfg(target_os = "linux")]
fn convert_mutsumi_tracks(value: mutsumi::MpvTracks) -> MpvTracks {
    MpvTracks {
        audio_tracks: value
            .audio_tracks
            .into_iter()
            .map(convert_mutsumi_track)
            .collect(),
        sub_tracks: value
            .sub_tracks
            .into_iter()
            .map(convert_mutsumi_track)
            .collect(),
    }
}

#[cfg(target_os = "linux")]
fn convert_mutsumi_track(value: mutsumi::MpvTrack) -> MpvTrack {
    MpvTrack {
        id: value.id,
        title: value.title,
        lang: value.lang,
        type_: value.type_,
    }
}

#[cfg(target_os = "linux")]
fn convert_mutsumi_chapters(value: mutsumi::ChapterList) -> ChapterList {
    ChapterList(
        value
            .into_iter()
            .map(|chapter| Chapter {
                title: chapter.title,
                time: chapter.time,
            })
            .collect(),
    )
}
