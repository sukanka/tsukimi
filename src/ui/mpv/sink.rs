use std::cell::{
    Cell,
    RefCell,
};

use crate::{
    ui::models::SETTINGS,
    utils::spawn,
};
use adw::{
    prelude::*,
    subclass::prelude::*,
};
use glib::Object;
use gtk::{
    gio,
    glib,
};
use mutsumi::{
    ContextedMPV,
    Danmaku,
    DanmakuTrack,
    Danmakw,
    MpvValue,
    MutsumiVideoPlayer,
    TrackKind,
    TrackSelection,
};

type DanmakuTimelineHandler = Box<dyn Fn(&MPVPlaySink, Vec<f64>)>;

mod imp {
    use super::*;

    pub struct MPVPlaySink {
        pub player: MutsumiVideoPlayer,
        pub danmaku: Danmakw,
        pub position: Cell<f64>,
        pub paused: Cell<bool>,
        pub video_loaded: Cell<bool>,
        pub buffering: Cell<bool>,
        pub danmaku_loaded: Cell<bool>,
        pub danmaku_running: Cell<bool>,
        pub danmaku_source: RefCell<Option<String>>,
        pub danmaku_generation: Cell<u64>,
        pub playback_speed: Cell<f64>,
        pub danmaku_timeline_handlers: RefCell<Vec<DanmakuTimelineHandler>>,
    }

    impl Default for MPVPlaySink {
        fn default() -> Self {
            Self {
                player: MutsumiVideoPlayer::new(),
                danmaku: Danmakw::new(),
                position: Cell::new(0.0),
                paused: Cell::new(true),
                video_loaded: Cell::new(false),
                buffering: Cell::new(false),
                danmaku_loaded: Cell::new(false),
                danmaku_running: Cell::new(false),
                danmaku_source: RefCell::new(None),
                danmaku_generation: Cell::new(0),
                playback_speed: Cell::new(1.0),
                danmaku_timeline_handlers: RefCell::new(Vec::new()),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MPVPlaySink {
        const NAME: &'static str = "MPVGLArea";
        type Type = super::MPVPlaySink;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for MPVPlaySink {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            self.player.set_hexpand(true);
            self.player.set_vexpand(true);

            let overlay = gtk::Overlay::new();
            overlay.set_hexpand(true);
            overlay.set_vexpand(true);
            overlay.set_child(Some(&self.player));

            self.danmaku.set_hexpand(true);
            self.danmaku.set_vexpand(true);
            self.danmaku.set_can_target(false);
            self.danmaku.set_opacity(SETTINGS.danmaku_opacity());
            self.danmaku.set_visible(SETTINGS.is_danmaku_enabled());
            overlay.add_overlay(&self.danmaku);
            obj.set_child(Some(&overlay));

            SETTINGS
                .bind("is-danmaku-enabled", &self.danmaku, "visible")
                .build();
            SETTINGS
                .bind("danmaku-opacity", &self.danmaku, "opacity")
                .build();
            obj.apply_danmaku_appearance();
            for key in [
                "danmaku-speed",
                "danmaku-font-size",
                "danmaku-font-weight",
                "danmaku-intensity",
                "danmaku-spacing-factor",
                "danmaku-outline-px",
                "danmaku-shadow-offset",
            ] {
                SETTINGS.connect_changed(
                    Some(key),
                    glib::clone!(
                        #[weak]
                        obj,
                        move |_, _| obj.apply_danmaku_appearance()
                    ),
                );
            }
            SETTINGS.connect_changed(
                Some("is-danmaku-enabled"),
                glib::clone!(
                    #[weak]
                    obj,
                    move |_, _| obj.sync_danmaku_playback()
                ),
            );
        }

        fn dispose(&self) {
            self.obj().set_child(None::<&gtk::Widget>);
        }
    }

    impl WidgetImpl for MPVPlaySink {}
    impl BinImpl for MPVPlaySink {}
}

glib::wrapper! {
    pub struct MPVPlaySink(ObjectSubclass<imp::MPVPlaySink>)
        @extends adw::Bin, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget;
}

impl Default for MPVPlaySink {
    fn default() -> Self {
        Self::new()
    }
}

impl MPVPlaySink {
    pub fn new() -> Self {
        Object::builder().build()
    }

    pub fn mpv(&self) -> &ContextedMPV {
        self.imp().player.mpv()
    }

    pub fn player(&self) -> &MutsumiVideoPlayer {
        &self.imp().player
    }

    pub fn play(&self, url: &str, start_seconds: f64) {
        tracing::info!("Now Playing: {url}");
        self.imp().position.set(start_seconds);
        self.imp().paused.set(false);
        self.imp().video_loaded.set(false);
        self.clear_danmaku();
        self.resume_cache_fill();

        self.player().set_start(start_seconds);
        self.player().push_an_empty_texture();
        self.player().load_video(url);
        self.player().pause(false);
    }

    pub fn resume_cached(&self, start_seconds: f64) {
        self.imp().position.set(start_seconds);
        self.imp().paused.set(false);
        self.resume_cache_fill();
        self.preroll_danmaku(start_seconds * 1000.0);
        self.player().set_position(start_seconds);
        self.player().pause(false);
        self.sync_danmaku_playback();
    }

    pub fn park_cached(&self) {
        self.imp().paused.set(true);
        self.player().pause(true);
        self.player().set_cache_secs(0.0);
        self.sync_danmaku_playback();
    }

    fn resume_cache_fill(&self) {
        self.player()
            .set_cache_secs(SETTINGS.mpv_cache_time() as f64);
    }

    pub fn add_sub(&self, url: &str) {
        self.player().add_sub(url)
    }

    pub fn seek_forward(&self, value: i64) {
        self.player().seek_forward(value)
    }

    pub fn seek_backward(&self, value: i64) {
        self.player().seek_backward(value)
    }

    pub fn set_position(&self, value: f64) {
        self.imp().position.set(value);
        self.preroll_danmaku(value * 1000.0);
        self.player().set_position(value)
    }

    pub fn position(&self) -> f64 {
        self.imp().position.get()
    }

    pub fn update_position(&self, value: f64) {
        self.imp().position.set(value);
        if self.imp().danmaku_loaded.get() && !self.imp().danmaku_running.get() {
            self.imp().danmaku.update(value * 1000.0);
        }
    }

    pub fn set_aid(&self, value: TrackSelection) {
        self.player().set_aid(value)
    }

    pub async fn get_track_id(&self, kind: TrackKind) -> i64 {
        self.player().get_track_id(kind).await
    }

    pub fn set_sid(&self, value: TrackSelection) {
        self.player().set_sid(value)
    }

    pub fn press_key(&self, key: u32, state: gtk::gdk::ModifierType) {
        self.player().press_key(key, state)
    }

    pub fn release_key(&self, key: u32, state: gtk::gdk::ModifierType) {
        self.player().release_key(key, state)
    }

    pub fn set_speed(&self, value: f64) {
        self.imp().playback_speed.set(value);
        self.apply_danmaku_speed();
        self.player().set_speed(value)
    }

    pub fn set_volume(&self, value: i64) {
        self.player().set_volume(value)
    }

    pub fn display_stats_toggle(&self) {
        self.player().display_stats_toggle()
    }

    pub fn paused(&self) -> bool {
        self.imp().paused.get()
    }

    pub fn update_paused(&self, value: bool) {
        self.imp().paused.set(value);
        self.sync_danmaku_playback();
    }

    pub fn pause(&self) {
        self.update_paused(!self.paused());
        self.player().command_pause();
    }

    pub fn volume_scroll(&self, value: i64) {
        self.player().volume_scroll(value)
    }

    pub fn set_slang(&self, value: String) {
        self.player().set_slang(value)
    }

    pub fn stop(&self) {
        self.imp().paused.set(true);
        self.imp().video_loaded.set(false);
        self.clear_danmaku();
        self.player().stop();
    }

    pub fn mark_loaded(&self) {
        self.imp().video_loaded.set(true);
        self.sync_danmaku_playback();
    }

    pub fn set_buffering(&self, buffering: bool) {
        self.imp().buffering.set(buffering);
        self.sync_danmaku_playback();
    }

    pub fn seek_danmaku(&self, position_millis: f64) {
        self.imp().position.set(position_millis / 1000.0);
        self.preroll_danmaku(position_millis);
    }

    pub fn load_danmaku_track(&self, track: Option<DanmakuTrack>) {
        let source = track.map(|track| track.external_url);
        if self.imp().danmaku_source.borrow().as_ref() == source.as_ref() {
            return;
        }

        let generation = self.next_danmaku_generation();
        self.imp().danmaku_source.replace(source.clone());
        self.reset_danmaku_renderer();

        let Some(source) = source else {
            return;
        };
        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let result = fetch_url_content(&source).await.and_then(|content| {
                    mutsumi::parse_bilibili_xml(&content)
                        .map_err(Box::<dyn std::error::Error>::from)
                });
                if obj.imp().danmaku_generation.get() != generation {
                    return;
                }

                match result {
                    Ok(items) => {
                        let count = items.len();
                        obj.apply_danmaku_items(items);
                        tracing::info!(
                            "Loaded external danmaku track from {source} ({count} items)"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            "Failed to load external danmaku track from {source}: {error}"
                        );
                    }
                }
            }
        ));
    }

    pub fn load_danmaku_items(&self, items: Vec<Danmaku>) {
        self.next_danmaku_generation();
        self.imp().danmaku_source.take();
        self.apply_danmaku_items(items);
    }

    pub fn has_external_danmaku_source(&self) -> bool {
        self.imp().danmaku_source.borrow().is_some()
    }

    fn apply_danmaku_items(&self, items: Vec<Danmaku>) {
        let loaded = !items.is_empty();
        let timeline = danmaku_timeline(&items);
        self.imp().danmaku.load_danmaku(items);
        self.imp().danmaku_loaded.set(loaded);
        self.preroll_danmaku(self.position() * 1000.0);
        self.sync_danmaku_playback();
        self.emit_danmaku_timeline(timeline);
    }

    fn apply_danmaku_appearance(&self) {
        let danmaku = &self.imp().danmaku;
        self.apply_danmaku_speed();
        danmaku.set_font_size(SETTINGS.danmaku_font_size());
        danmaku.set_font_weight(SETTINGS.danmaku_font_weight().round() as u32);
        danmaku.set_intensity(SETTINGS.danmaku_intensity().round().clamp(0.0, 3.0) as u32);
        danmaku.set_spacing_factor(SETTINGS.danmaku_spacing_factor());
        danmaku.set_outline_px(SETTINGS.danmaku_outline_px());
        danmaku.set_shadow_offset(SETTINGS.danmaku_shadow_offset());
        self.preroll_danmaku(self.position() * 1000.0);
    }

    fn apply_danmaku_speed(&self) {
        self.imp()
            .danmaku
            .set_speed_factor(SETTINGS.danmaku_speed() * self.imp().playback_speed.get());
    }

    pub fn clear_danmaku(&self) {
        self.next_danmaku_generation();
        self.imp().danmaku_source.take();
        self.reset_danmaku_renderer();
    }

    fn next_danmaku_generation(&self) -> u64 {
        let generation = self.imp().danmaku_generation.get().wrapping_add(1);
        self.imp().danmaku_generation.set(generation);
        generation
    }

    fn reset_danmaku_renderer(&self) {
        self.imp().danmaku_loaded.set(false);
        self.imp().danmaku_running.set(false);
        self.imp().danmaku.set_paused(true);
        self.imp().danmaku.load_danmaku(Vec::new());
        self.emit_danmaku_timeline(Vec::new());
    }

    pub fn connect_danmaku_timeline_changed(&self, callback: impl Fn(&Self, Vec<f64>) + 'static) {
        self.imp()
            .danmaku_timeline_handlers
            .borrow_mut()
            .push(Box::new(callback));
    }

    fn emit_danmaku_timeline(&self, timeline: Vec<f64>) {
        for callback in self.imp().danmaku_timeline_handlers.borrow().iter() {
            callback(self, timeline.clone());
        }
    }

    fn preroll_danmaku(&self, position_millis: f64) {
        if self.imp().danmaku_loaded.get() {
            self.imp().danmaku.preroll_seek(position_millis);
        }
    }

    fn sync_danmaku_playback(&self) {
        let imp = self.imp();
        let should_run = SETTINGS.is_danmaku_enabled()
            && imp.video_loaded.get()
            && imp.danmaku_loaded.get()
            && !imp.paused.get()
            && !imp.buffering.get();
        if should_run == imp.danmaku_running.get() {
            return;
        }

        imp.danmaku.set_paused(!should_run);
        imp.danmaku_running.set(should_run);
    }

    pub fn set_property<V>(&self, property: &str, value: V)
    where
        V: Into<MpvValue>,
    {
        self.player().set_property(property, value)
    }
}
async fn fetch_url_content(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let file = gio::File::for_uri(url);
    let (bytes, _) = file.load_contents_future().await?;
    Ok(String::from_utf8(bytes.to_vec())?)
}

fn danmaku_timeline(items: &[Danmaku]) -> Vec<f64> {
    items
        .iter()
        .filter_map(|item| {
            (item.start.is_finite() && item.start >= 0.0).then_some(item.start / 1000.0)
        })
        .collect()
}
