use anyhow::Result;

use adw::subclass::prelude::{
    ObjectSubclassExt,
    ObjectSubclassIsExt,
};
use gtk::{
    self,
    glib,
};
use mpris_server::{
    LocalPlayerInterface,
    LocalRootInterface,
    LocalServer,
    LoopStatus,
    Metadata,
    PlaybackRate,
    PlaybackStatus,
    Property,
    Signal,
    Time,
    TrackId,
    Volume,
    zbus::{
        self,
        fdo,
    },
};

use crate::{
    APP_ID,
    CLIENT_ID,
    gstl::player::imp::ListRepeatMode,
    ui::mpv::page::MPVPage,
    utils::{
        get_image_with_cache,
        spawn,
    },
};
use tracing::warn;

impl MPVPage {
    pub async fn initialize_mpris(&self, app_id: &str) -> Result<()> {
        let server = LocalServer::new(app_id, self.imp().obj().clone()).await?;
        spawn(server.run());
        self.imp()
            .mpris_server
            .set(server)
            .map_err(|_| anyhow::anyhow!("Mpris server already initialized"))?;
        Ok(())
    }

    pub fn mpris_server(&self) -> Option<&LocalServer<MPVPage>> {
        self.imp().mpris_server.get()
    }

    pub fn mpris_properties_changed(&self, property: impl IntoIterator<Item = Property> + 'static) {
        #[cfg(target_os = "linux")]
        let will_init = self.mpris_server().is_none()
            && !self.imp().mpris_initializing.replace(true);
        #[cfg(not(target_os = "linux"))]
        let will_init = false;

        spawn(glib::clone!(
            #[weak(rename_to=imp)]
            self,
            async move {
                if will_init {
                    let app_id = format!("{}.{}", APP_ID, "mpv");
                    if let Err(e) = imp.initialize_mpris(&app_id).await {
                        warn!("Failed to initialize mpris server: {}", e);
                    }
                    #[cfg(target_os = "linux")]
                    imp.imp().mpris_initializing.set(false);
                }
                // If another future is still initializing, wait for it
                // so we don't drop this property change.
                let server = loop {
                    if let Some(server) = imp.mpris_server() {
                        break server;
                    }
                    glib::timeout_future(std::time::Duration::from_millis(10)).await;
                };
                if let Err(err) = server.properties_changed(property).await {
                    warn!("Failed to emit properties changed: {}", err);
                }
            }
        ));
    }

    pub fn notify_mpris_seeked(&self, position: i64) {
        if position <= 0 {
            return;
        }
        spawn(glib::clone!(
            #[weak(rename_to=obj)]
            self,
            async move {
                if let Some(server) = obj.mpris_server() {
                    let signal = Signal::Seeked {
                        position: Time::from_millis(position),
                    };
                    if let Err(err) = server.emit(signal).await {
                        warn!("Failed to emit mpris_seeked: {}", err);
                    }
                }
            }
        ));
    }

    pub fn notify_mpris_playing(&self) {
        self.mpris_properties_changed([
            Property::Metadata(self.metadata().clone()),
            Property::CanPlay(true),
            Property::CanPause(true),
            Property::CanSeek(true),
            Property::PlaybackStatus(PlaybackStatus::Playing),
        ]);
        self.notify_mpris_art_changed();
        let position = self.imp().video.position();
        self.notify_mpris_seeked((position * 1000.0) as i64);
    }

    pub fn notify_mpris_paused(&self) {
        self.mpris_properties_changed([
            Property::Metadata(self.metadata().clone()),
            Property::CanPlay(true),
            Property::CanPause(false),
            Property::CanSeek(true),
            Property::PlaybackStatus(PlaybackStatus::Paused),
        ]);
    }

    pub fn notify_mpris_stopped(&self) {
        self.mpris_properties_changed([
            Property::Metadata(self.metadata().clone()),
            Property::CanPlay(true),
            Property::CanPause(false),
            Property::CanSeek(false),
            Property::PlaybackStatus(PlaybackStatus::Stopped),
        ]);
    }

    pub fn notify_mpris_loop_status(&self, status: ListRepeatMode) {
        self.mpris_properties_changed([Property::LoopStatus(status.into())]);
    }

    pub fn notify_mpris_has_chapters(&self, has_chapters: bool) {
        self.mpris_properties_changed([
            Property::CanGoNext(has_chapters),
            Property::CanGoPrevious(has_chapters),
        ]);
    }

    pub fn notify_mpris_art_changed(&self) {
        let mut metadata = self.metadata().clone();
        spawn(glib::clone!(
            #[weak(rename_to = imp)]
            self.imp(),
            #[weak(rename_to = obj)]
            self,
            async move {
                if let Some(video) = obj.current_video().as_ref() {
                    // Try episode cover → season cover → poster/backdrop
                    let fallbacks: Vec<(String, &str)> = {
                        let mut v = Vec::new();
                        if let Some(id) = video.primary_image_item_id() {
                            v.push((id, "Primary"));
                        }
                        if let Some(id) = video.season_id() {
                            v.push((id, "Primary"));
                        }
                        if let Some(id) = video.series_id() {
                            v.push((id, "Backdrop"));
                        }
                        if let Some(id) = video.parent_backdrop_item_id() {
                            v.push((id, "Backdrop"));
                        }
                        if let Some(id) = video.parent_thumb_item_id() {
                            v.push((id, "Thumb"));
                        }
                        v.push((video.id(), "Backdrop"));
                        v.push((video.id(), "Primary"));
                        v
                    };

                    for (id, img_type) in &fallbacks {
                        if let Ok(path) = get_image_with_cache(id.clone(), img_type.to_string(), None).await {
                            if !path.is_empty() {
                                let url = format!("file://{}", path);
                                imp.cached_art_url.replace(Some(url.clone()));
                                imp.cached_art_id.replace(video.id());
                                metadata.set_art_url(Some(url));
                                obj.mpris_properties_changed([Property::Metadata(metadata)]);
                                return;
                            }
                        }
                    }
                }
            }
        ));
    }

    pub fn metadata(&self) -> Metadata {
        self.imp()
            .obj()
            .current_video()
            .as_ref()
            .map_or_else(Metadata::new, |video| {
                let mut builder = Metadata::builder()
                    .title(video.name())
                    .length(Time::from_secs(
                        (video.run_time_ticks() / 10_000_000) as i64,
                    ));
                if let Some(artists) = video.artists() {
                    builder = builder.artist([artists]);
                }
                if let Some(album) = video.album_id() {
                    builder = builder.album(album);
                }
                // Use cached art URL if it matches the current video
                let imp = self.imp();
                if imp.cached_art_id.borrow().as_str() == video.id().as_str() {
                    if let Some(url) = imp.cached_art_url.borrow().as_ref() {
                        builder = builder.art_url(url.clone());
                    }
                }
                builder.build()
            })
    }
}

impl LocalRootInterface for MPVPage {
    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn raise(&self) -> fdo::Result<()> {
        crate::mpris_common::raise_window().await
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn quit(&self) -> fdo::Result<()> {
        crate::mpris_common::quit_application().await
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(self.fullscreened())
    }

    async fn set_fullscreen(&self, fullscreen: bool) -> zbus::Result<()> {
        self.set_fullscreened(fullscreen);
        Ok(())
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok(CLIENT_ID.to_string())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok(APP_ID.to_string())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }
}

impl LocalPlayerInterface for MPVPage {
    async fn next(&self) -> fdo::Result<()> {
        self.chapter_next();
        Ok(())
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.chapter_next();
        Ok(())
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.on_pause_update(true);
        self.mpv().pause(true);
        Ok(())
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        let paused = self.imp().video.paused();
        self.on_pause_update(!paused);
        self.mpv().pause(!paused);
        Ok(())
    }

    async fn stop(&self) -> fdo::Result<()> {
        // same as pause
        self.on_pause_update(true);
        self.mpv().pause(true);
        Ok(())
    }

    async fn play(&self) -> fdo::Result<()> {
        self.on_pause_update(false);
        self.mpv().pause(false);
        Ok(())
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        self.imp().video.seek_forward(offset.as_secs());
        Ok(())
    }

    async fn set_position(&self, _track_id: TrackId, position: Time) -> fdo::Result<()> {
        self.mpv().set_position(position.as_secs() as f64);
        Ok(())
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("OpenUri is not supported".into()))
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(PlaybackStatus::Stopped)
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(LoopStatus::None)
    }

    async fn set_loop_status(&self, _status: LoopStatus) -> zbus::Result<()> {
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn set_rate(&self, rate: PlaybackRate) -> zbus::Result<()> {
        self.mpv().set_speed(rate);
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_shuffle(&self, _shuffle: bool) -> zbus::Result<()> {
        Err(zbus::Error::from(fdo::Error::NotSupported(
            "SetShuffle is not supported".into(),
        )))
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(self.metadata())
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(1.0)
    }

    async fn set_volume(&self, volume: Volume) -> zbus::Result<()> {
        self.mpv().set_volume(volume as i64);
        Ok(())
    }

    async fn position(&self) -> fdo::Result<Time> {
        let position = Time::from_micros(self.imp().video.position() as i64);
        Ok(position)
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(0.1)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(5.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.current_video().is_some())
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.current_video().is_some())
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(self.current_video().is_some())
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.current_video().is_some())
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.current_video().is_some())
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}
