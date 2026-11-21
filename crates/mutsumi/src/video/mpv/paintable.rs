use glib::Object;
use gtk::{
    gdk::prelude::PaintableExt,
    glib,
    subclass::prelude::*,
};

use crate::{
    PlayParams,
    video::{
        backend::{
            TrackKind,
            TrackSelection,
        },
        mpv::contexted::ContextedMPV,
    },
};

use gtk::gdk;

mod imp {
    use crate::{
        DmabufFrame,
        FRAME_CHANNEL,
        FrameCallbacks,
        ShmMemoryFormat,
        SurfaceContentUpdate,
        SurfaceUpdate,
        create_mpv_proxy,
        video::{
            MutsumiMpvError,
            mpv::{
                color::ColorDescription,
                contexted::ContextedMPV,
            },
        },
    };
    #[cfg(feature = "profiling")]
    use std::cell::Cell;
    use std::{
        cell::RefCell,
        os::fd::AsRawFd,
        sync::{
            Arc,
            OnceLock,
        },
    };

    #[cfg(feature = "profiling")]
    use crate::video::mpv::proxy::profiling::{
        self,
        Stage,
    };

    use super::*;

    use glib::subclass::Signal;
    use gtk::{
        glib,
        prelude::*,
    };

    enum FramePixels {
        Dmabuf(Arc<DmabufFrame>),
        Shm {
            width: i32,
            height: i32,
            stride: usize,
            format: gdk::MemoryFormat,
            bytes: glib::Bytes,
        },
    }

    struct VideoFrame {
        surface_id: u64,
        color: ColorDescription,
        pixels: FramePixels,
    }

    impl VideoFrame {
        fn texture(&self) -> Result<gdk::Texture, glib::Error> {
            match &self.pixels {
                FramePixels::Dmabuf(frame) => {
                    #[cfg(feature = "profiling")]
                    profiling::mark(frame.profile_frame_id, Stage::TextureBuildStarted);
                    let color = self.color.to_gdk_dmabuf_color_state(frame.format);
                    let mut builder = gdk::DmabufTextureBuilder::new()
                        .set_display(
                            &gdk::Display::default().expect("Could not connect to display"),
                        )
                        .set_width(frame.width)
                        .set_height(frame.height)
                        .set_fourcc(frame.format)
                        .set_modifier(frame.modifier)
                        .set_color_state(color.as_ref())
                        .set_n_planes(frame.planes.len() as u32);
                    for (i, plane) in frame.planes.iter().enumerate() {
                        builder = unsafe { builder.set_fd(i as u32, plane.fd.as_raw_fd()) }
                            .set_offset(i as u32, plane.offset)
                            .set_stride(i as u32, plane.stride);
                    }
                    let texture = unsafe { builder.build() }?;
                    // Textures built from the same frame share its buffer lease.
                    unsafe { texture.set_data("mutsumi-dmabuf-frame", Arc::clone(frame)) };
                    #[cfg(feature = "profiling")]
                    profiling::mark(frame.profile_frame_id, Stage::TextureBuilt);
                    Ok(texture)
                }
                FramePixels::Shm {
                    width,
                    height,
                    stride,
                    format,
                    bytes,
                } => {
                    let color = self
                        .color
                        .to_gdk_color_state()
                        .unwrap_or_else(|_| gdk::ColorState::srgb());
                    Ok(gdk::MemoryTextureBuilder::new()
                        .set_width(*width)
                        .set_height(*height)
                        .set_stride(*stride)
                        .set_format(*format)
                        .set_bytes(Some(bytes))
                        .set_color_state(&color)
                        .build())
                }
            }
        }
    }

    #[derive(Default)]
    pub struct MutsumiVideoSink {
        pub mpv: ContextedMPV,
        pub texture: RefCell<Option<gdk::Texture>>,
        frame: RefCell<Option<VideoFrame>>,
        pub frame_callbacks: RefCell<Vec<FrameCallbacks>>,
        #[cfg(feature = "profiling")]
        pub pending_snapshot_frame_id: Cell<Option<u64>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MutsumiVideoSink {
        const NAME: &'static str = "MutsumiVideoSink";
        type Type = super::MutsumiVideoSink;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for MutsumiVideoSink {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();

            self.setup_mpv();

            glib::spawn_future_local(glib::clone!(
                #[weak]
                obj,
                async move {
                    while let Ok(update) = FRAME_CHANNEL.rx.recv_async().await {
                        obj.imp().update_frame(update);
                    }
                }
            ));
        }

        fn dispose(&self) {
            self.frame.take();
            self.texture.take();
            self.finish_frame_callbacks();
            self.mpv.shutdown();
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("mutsumi-error")
                        .param_types([glib::Type::I32])
                        .build(),
                ]
            })
        }
    }

    impl PaintableImpl for MutsumiVideoSink {
        fn intrinsic_width(&self) -> i32 {
            self.texture.borrow().as_ref().map_or(0, |t| t.width())
        }

        fn intrinsic_height(&self) -> i32 {
            self.texture.borrow().as_ref().map_or(0, |t| t.height())
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            if let Some(texture) = self.texture.borrow().as_ref() {
                snapshot.append_texture(
                    texture,
                    &gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32),
                );

                #[cfg(feature = "profiling")]
                profiling::mark(self.pending_snapshot_frame_id.take(), Stage::Snapshot);
            }

            self.finish_frame_callbacks();
        }
    }

    impl MutsumiVideoSink {
        fn update_frame(&self, update: SurfaceUpdate) {
            let SurfaceUpdate {
                content,
                frame_callbacks,
                surface_id,
                color,
            } = update;
            let mut frame = self.frame.borrow_mut();
            let current = frame
                .as_mut()
                .filter(|frame| frame.surface_id == surface_id);
            let color_changed = current.as_ref().is_none_or(|frame| frame.color != color);
            let rebuild = match content {
                SurfaceContentUpdate::Frame(pixels) => {
                    *frame = Some(VideoFrame {
                        surface_id,
                        color,
                        pixels: FramePixels::Dmabuf(Arc::new(pixels)),
                    });
                    true
                }
                SurfaceContentUpdate::Shm(pixels) => {
                    let format = match pixels.format {
                        ShmMemoryFormat::Argb8888 | ShmMemoryFormat::Xrgb8888 => {
                            gdk::MemoryFormat::B8g8r8a8Premultiplied
                        }
                    };
                    *frame = Some(VideoFrame {
                        surface_id,
                        color,
                        pixels: FramePixels::Shm {
                            width: pixels.width,
                            height: pixels.height,
                            stride: pixels.stride,
                            format,
                            bytes: glib::Bytes::from_owned(pixels.data),
                        },
                    });
                    true
                }
                SurfaceContentUpdate::Clear if current.is_some() => {
                    *frame = None;
                    true
                }
                SurfaceContentUpdate::Unchanged if color_changed => {
                    if let Some(current) = current {
                        current.color = color;
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            };

            let obj = self.obj();
            if rebuild {
                let texture = frame.as_ref().and_then(|frame| match frame.texture() {
                    Ok(texture) => {
                        if color_changed {
                            tracing::debug!(color = ?frame.color, "Imported video texture color state");
                        }
                        Some(texture)
                    }
                    Err(error) => {
                        tracing::error!(%error, "Failed to import video texture");
                        None
                    }
                });
                #[cfg(feature = "profiling")]
                self.pending_snapshot_frame_id
                    .set(frame.as_ref().and_then(|frame| match &frame.pixels {
                        FramePixels::Dmabuf(pixels) if texture.is_some() => pixels.profile_frame_id,
                        _ => None,
                    }));
                drop(frame);
                obj.replace_texture(texture);
            }
            if let Some(frame_callbacks) = frame_callbacks {
                self.frame_callbacks.borrow_mut().push(frame_callbacks);
            }
            if self.texture.borrow().is_none() {
                self.finish_frame_callbacks();
            }
            obj.invalidate_contents();
        }

        fn finish_frame_callbacks(&self) {
            let time_ms = (glib::monotonic_time() / 1_000) as u32;
            for callbacks in self.frame_callbacks.take() {
                callbacks.done(time_ms);
            }
        }

        pub(super) fn discard_frame(&self) {
            self.frame.take();
            #[cfg(feature = "profiling")]
            self.pending_snapshot_frame_id.set(None);
            self.finish_frame_callbacks();
        }

        fn setup_mpv(&self) {
            let display = gdk::Display::default().expect("Could not connect to display");
            let formats = display.dmabuf_formats();

            // GTK < 4.24 has no memory-format mapping for 10-bit packed RGB
            // formats (AR30/XR30/AB30/XB30 = *RGB/*BGR 2101010), but the host
            // compositor may still advertise them in dmabuf_formats().
            // See https://gitlab.gnome.org/GNOME/gtk/-/issues/8148
            const PACKED_10BIT: &[u32] = &[
                0x30335241, // DRM_FORMAT_ARGB2101010 (AR30)
                0x30335258, // DRM_FORMAT_XRGB2101010 (XR30)
                0x30334241, // DRM_FORMAT_ABGR2101010 (AB30)
                0x30334258, // DRM_FORMAT_XBGR2101010 (XB30)
            ];
            let skip_packed_10bit = gtk::minor_version() < 24;

            let format_pairs: Vec<(u32, u64)> = (0..formats.n_formats())
                .map(|i| formats.format(i))
                .filter(|(fourcc, _)| !skip_packed_10bit || !PACKED_10BIT.contains(fourcc))
                .collect();

            create_mpv_proxy(format_pairs);
        }

        pub fn throw_error(&self, code: MutsumiMpvError) {
            self.obj().emit_by_name::<()>("mutsumi-error", &[&code]);
        }
    }
}

glib::wrapper! {
    pub struct MutsumiVideoSink(ObjectSubclass<imp::MutsumiVideoSink>)
        @implements gdk::Paintable;
}

impl Default for MutsumiVideoSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MutsumiVideoSink {
    pub fn new() -> Self {
        Object::builder().build()
    }

    pub fn mpv(&self) -> &ContextedMPV {
        &self.imp().mpv
    }

    pub fn play(&self, source: &PlayParams) {
        let url = source.url().into_owned();
        let start_time = source.start_time;

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let mpv = obj.mpv();

                tracing::info!(target: "mutsumi::mpv", source = url, "loading media");
                mpv.load_video(&url);

                if let Some(start_time) = start_time {
                    mpv.set_start_time(start_time);
                }

                mpv.pause(false);
            }
        ));
    }

    fn replace_texture(&self, texture: Option<gdk::Texture>) {
        let old_size = (self.intrinsic_width(), self.intrinsic_height());
        self.imp().texture.replace(texture);
        let new_size = (self.intrinsic_width(), self.intrinsic_height());

        // Picture caches its layout even when paintable contents are invalidated.
        if old_size != new_size {
            self.invalidate_size();
        }
    }

    // This would be useful for clearing the video area when stopping playback, to avoid showing the last frame.
    pub fn push_an_empty_texture(&self) {
        self.imp().discard_frame();
        let bytes = glib::Bytes::from_static(&[0, 0, 0, 0]);

        let texture = gdk::MemoryTexture::new(1, 1, gdk::MemoryFormat::R8g8b8a8, &bytes, 4);

        let texture = gdk::Texture::from(texture);

        self.replace_texture(Some(texture));
        self.invalidate_contents();
    }

    pub fn set_loop_playlist(&self, loop_: &str) {
        self.mpv().set_loop_playlist(loop_);
    }

    pub fn set_loop_file(&self, loop_: &str) {
        self.mpv().set_loop_file(loop_);
    }

    pub fn playlist_shuffle(&self) {
        self.mpv().playlist_shuffle();
    }

    pub fn playlist_unshuffle(&self) {
        self.mpv().playlist_unshuffle();
    }

    pub fn set_playlist(&self, urls: &[String]) {
        self.mpv().set_playlist(urls);
    }

    pub fn set_playlist_pos(&self, pos: i64) {
        self.mpv().set_playlist_pos(pos);
    }

    pub fn playlist_add(&self, url: &str, index: i64) {
        self.mpv().playlist_add(url, index);
    }

    pub fn playlist_remove(&self, index: i64) {
        self.mpv().playlist_remove(index);
    }

    pub fn playlist_move(&self, from: i64, to: i64) {
        self.mpv().playlist_move(from, to);
    }

    pub fn press_key(&self, key: u32, state: gtk::gdk::ModifierType) {
        self.mpv().press_key(key, state)
    }

    pub fn release_key(&self, key: u32, state: gtk::gdk::ModifierType) {
        self.mpv().release_key(key, state)
    }

    pub fn volume_scroll(&self, value: f64) {
        self.mpv().volume_scroll(value)
    }

    pub fn set_slang(&self, value: String) {
        self.mpv().set_slang(value)
    }

    pub fn shutdown(&self) {
        self.imp().discard_frame();
        self.replace_texture(None);
        self.invalidate_contents();
        self.mpv().shutdown();
    }

    pub fn stop(&self) {
        self.mpv().stop();
    }

    pub fn load_video(&self, url: &str) {
        self.mpv().load_video(url);
    }

    pub fn pause(&self, pause: bool) {
        self.mpv().pause(pause);
    }

    pub fn command_pause(&self) {
        self.mpv().command_pause();
    }

    pub fn set_percent_position(&self, value: f64) {
        self.mpv().set_percent_position(value);
    }

    pub fn set_start_time(&self, second: u64) {
        self.mpv().set_start_time(second);
    }

    pub fn set_start(&self, second: f64) {
        self.mpv().set_start(second);
    }

    pub fn set_aid(&self, value: TrackSelection) {
        self.mpv().set_aid(value);
    }

    pub fn set_sid(&self, value: TrackSelection) {
        self.mpv().set_sid(value);
    }

    pub fn disable_aid(&self) {
        self.mpv().set_aid(TrackSelection::None);
    }

    pub fn disable_sid(&self) {
        self.mpv().set_sid(TrackSelection::None);
    }

    pub fn set_brightness(&self, value: f64) {
        self.mpv().mpv.set_property("brightness", value);
    }

    pub fn set_contrast(&self, value: f64) {
        self.mpv().mpv.set_property("contrast", value);
    }

    pub fn set_gamma(&self, value: f64) {
        self.mpv().mpv.set_property("gamma", value);
    }

    pub fn set_hue(&self, value: f64) {
        self.mpv().mpv.set_property("hue", value);
    }

    pub fn set_saturation(&self, value: f64) {
        self.mpv().mpv.set_property("saturation", value);
    }

    pub fn set_sub_pos(&self, value: f64) {
        self.mpv().mpv.set_property("sub-pos", value);
    }

    pub fn set_sub_font_size(&self, value: f64) {
        self.mpv().mpv.set_property("sub-font-size", value);
    }

    pub fn set_sub_scale(&self, value: f64) {
        self.mpv().mpv.set_property("sub-scale", value);
    }

    pub fn set_sub_speed(&self, value: f64) {
        self.mpv().mpv.set_property("sub-speed", value);
    }

    pub fn set_sub_delay(&self, value: f64) {
        self.mpv().mpv.set_property("sub-delay", value);
    }

    pub fn set_sub_justify(&self, value: &str) {
        self.mpv().mpv.set_property("sub-justify", value.to_owned());
    }

    pub fn set_sub_bold(&self, value: bool) {
        self.mpv().mpv.set_property("sub-bold", value);
    }

    pub fn set_sub_italic(&self, value: bool) {
        self.mpv().mpv.set_property("sub-italic", value);
    }

    pub fn set_sub_font(&self, value: &str) {
        self.mpv().mpv.set_property("sub-font", value.to_owned());
    }

    pub fn set_sub_color(&self, value: &str) {
        self.mpv().mpv.set_property("sub-color", value.to_owned());
    }

    pub fn set_sub_border_color(&self, value: &str) {
        self.mpv()
            .mpv
            .set_property("sub-border-color", value.to_owned());
    }

    pub fn set_sub_back_color(&self, value: &str) {
        self.mpv()
            .mpv
            .set_property("sub-back-color", value.to_owned());
    }

    pub fn set_sub_border_style(&self, value: &str) {
        self.mpv()
            .mpv
            .set_property("sub-border-style", value.to_owned());
    }

    pub fn set_sub_border_size(&self, value: f64) {
        self.mpv().mpv.set_property("sub-border-size", value);
    }

    pub fn set_sub_shadow_offset(&self, value: f64) {
        self.mpv().mpv.set_property("sub-shadow-offset", value);
    }

    pub fn set_audio_delay(&self, value: f64) {
        self.mpv().mpv.set_property("audio-delay", value);
    }

    pub fn set_audio_channels(&self, value: &str) {
        self.mpv()
            .mpv
            .set_property("audio-channels", value.to_owned());
    }

    pub fn set_audio_pan(&self, value: &str) {
        self.mpv().mpv.set_property("af", value.to_owned());
    }

    pub fn clear_audio_pan(&self) {
        self.mpv().mpv.set_property("af", String::new());
    }

    pub fn set_scale(&self, value: &str) {
        self.mpv().mpv.set_property("scale", value.to_owned());
    }

    pub fn set_deband(&self, value: bool) {
        self.mpv().mpv.set_property("deband", value);
    }

    pub fn set_deband_iterations(&self, value: i64) {
        self.mpv().mpv.set_property("deband-iterations", value);
    }

    pub fn set_deband_threshold(&self, value: i64) {
        self.mpv().mpv.set_property("deband-threshold", value);
    }

    pub fn set_deband_range(&self, value: i64) {
        self.mpv().mpv.set_property("deband-range", value);
    }

    pub fn set_deband_grain(&self, value: i64) {
        self.mpv().mpv.set_property("deband-grain", value);
    }

    pub fn set_deinterlace(&self, value: bool) {
        self.mpv().mpv.set_property("deinterlace", value);
    }

    pub fn set_hwdec(&self, value: &str) {
        self.mpv().mpv.set_property("hwdec", value.to_owned());
    }

    pub fn set_panscan(&self, value: f64) {
        self.mpv().mpv.set_property("panscan", value);
    }

    pub fn set_stretch_image_subs_to_screen(&self, value: bool) {
        self.mpv()
            .mpv
            .set_property("stretch-image-subs-to-screen", value);
    }

    pub fn set_demuxer_max_bytes(&self, value: &str) {
        self.mpv()
            .mpv
            .set_property("demuxer-max-bytes", value.to_owned());
    }

    pub fn set_cache_secs(&self, value: f64) {
        self.mpv().mpv.set_property("cache-secs", value);
    }

    pub fn set_vo(&self, value: &str) {
        self.mpv().mpv.set_property("vo", value.to_owned());
    }

    pub fn set_property<V>(&self, property: &str, value: V)
    where
        V: Into<crate::video::MpvValue>,
    {
        self.mpv().mpv.set_property(property, value);
    }

    pub fn display_stats_toggle(&self) {
        self.mpv().display_stats_toggle();
    }

    pub fn add_sub(&self, url: &str) {
        self.mpv().add_sub(url);
    }

    pub fn set_position(&self, position: f64) {
        self.mpv().set_position(position);
    }

    pub fn set_volume(&self, volume: f64) {
        self.mpv().set_volume(volume);
    }

    pub fn seek_forward(&self, seconds: i64) {
        self.mpv().seek_forward(seconds);
    }

    pub fn seek_backward(&self, seconds: i64) {
        self.mpv().seek_backward(seconds);
    }

    pub fn set_speed(&self, speed: f64) {
        self.mpv().set_speed(speed);
    }

    pub fn set_keep_aspect_ratio(&self, value: bool) {
        self.mpv().mpv.set_property("keepaspect", value);
    }

    pub async fn position(&self) -> f64 {
        self.mpv().position().await
    }

    pub async fn paused(&self) -> bool {
        self.mpv().paused().await
    }

    pub async fn duration(&self) -> f64 {
        self.mpv().duration().await
    }

    pub async fn get_track_id(&self, kind: TrackKind) -> i64 {
        let type_ = match kind {
            TrackKind::Video => "vid",
            TrackKind::Audio => "aid",
            TrackKind::Subtitle => "sid",
        };

        self.mpv().get_track_id(type_).await
    }
}
