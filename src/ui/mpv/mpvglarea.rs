use glib::Object;
use gtk::{
    gio,
    glib,
    subclass::prelude::*,
};
use libmpv2::SetData;
use tracing::info;

use super::tsukimi_mpv::{
    ACTIVE,
    TrackSelection,
};
use crate::{
    client::jellyfin_client::JELLYFIN_CLIENT,
    ui::models::SETTINGS,
    utils::spawn,
};

mod imp {
    use std::cell::RefCell;
    use std::ffi::c_void;

    #[cfg(target_os = "linux")]
    use gdk_wayland::{
        WaylandDisplay,
        wayland_client::Proxy,
    };

    #[cfg(target_os = "linux")]
    use gdk_x11::X11Display;

    use gettextrs::gettext;
    use glow::HasContext;
    use gtk::{
        gdk::{
            Display,
            GLContext,
        },
        glib,
        prelude::*,
        subclass::prelude::*,
    };
    use libmpv2::render::{
        OpenGLInitParams,
        RenderContext,
        RenderParam,
        RenderParamApiType,
    };
    use once_cell::sync::OnceCell;

    use crate::{
        close_on_error,
        ui::mpv::tsukimi_mpv::{
            RENDER_UPDATE,
            TsukimiMPV,
        },
    };

    #[derive(Default)]
    pub struct MPVGLArea {
        pub mpv: TsukimiMPV,
        pub ipc_client: RefCell<Option<crate::ui::mpv::mpv_ipc::MpvIpcClient>>,

        pub ctx: OnceCell<glow::Context>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MPVGLArea {
        const NAME: &'static str = "MPVGLArea";
        type Type = super::MPVGLArea;
        type ParentType = gtk::GLArea;
    }

    impl ObjectImpl for MPVGLArea {
        fn constructed(&self) {
            self.parent_constructed();
        }

        fn dispose(&self) {
            self.mpv().shutdown_event_thread();
        }
    }

    impl WidgetImpl for MPVGLArea {
        fn realize(&self) {
            self.parent_realize();
            if self.obj().is_ipc() {
                return;
            }
            let obj = self.obj();

            if obj.error().is_some() {
                close_on_error!(obj, gettext("Failed to realize GLArea"));
                return;
            }

            obj.make_current();
            let Some(gl_context) = obj.context() else {
                close_on_error!(obj, gettext("Failed to get GLContext"));
                return;
            };

            self.setup_mpv(gl_context, obj.display());

            glib::spawn_future_local(glib::clone!(
                #[weak]
                obj,
                async move {
                    while RENDER_UPDATE.rx.recv_async().await.is_ok() {
                        obj.queue_render();
                    }
                }
            ));
        }

        fn unrealize(&self) {
            self.parent_unrealize();
        }
    }

    impl GLAreaImpl for MPVGLArea {
        fn render(&self, _context: &GLContext) -> glib::Propagation {
            if self.obj().is_ipc() {
                return glib::Propagation::Stop;
            }
            let binding = self.mpv().ctx.borrow();
            let Some(ctx) = binding.as_ref() else {
                return glib::Propagation::Stop;
            };

            let factor = self.obj().scale_factor();
            let width = self.obj().width() * factor;
            let height = self.obj().height() * factor;

            unsafe {
                let fbo = self.glow_cxt().get_parameter_i32(glow::FRAMEBUFFER_BINDING);
                ctx.render::<GLContext>(fbo, width, height, true).unwrap();
            }
            glib::Propagation::Stop
        }
    }

    impl MPVGLArea {
        pub fn mpv(&self) -> &TsukimiMPV {
            &self.mpv
        }

        fn setup_mpv(&self, gl_context: GLContext, display: Display) {
            let mut render_params = vec![
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address,
                    ctx: gl_context,
                }),
            ];

            // MPV render params to enable hardware decoding on X11 and Wayland
            // displays.
            //
            // https://github.com/mpv-player/mpv/blob/86e12929aa0bbc61946d3804982acf887786a7cb/include/mpv/render_gl.h#L91
            #[cfg(target_os = "linux")]
            if let Some(display_wrapper) = display.clone().downcast::<X11Display>().ok() {
                render_params.push(RenderParam::X11Display(
                    unsafe { display_wrapper.xdisplay() } as *const c_void,
                ));
            } else if let Some(display_wrapper) = display.clone().downcast::<WaylandDisplay>().ok()
                && let Some(wl_display) = display_wrapper.wl_display()
            {
                render_params.push(RenderParam::WaylandDisplay(
                    wl_display.id().as_ptr() as *const c_void
                ));
            }

            let tmpv = self.mpv();
            let mut handle = tmpv.mpv.ctx;
            let mut ctx = RenderContext::new(unsafe { handle.as_mut() }, render_params)
                .expect("Failed creating render context");

            ctx.set_update_callback(|| {
                let _ = RENDER_UPDATE.tx.send(true);
            });

            tmpv.ctx.replace(Some(ctx));

            tmpv.process_events();
        }

        fn glow_cxt(&self) -> &glow::Context {
            self.ctx.get_or_init(|| unsafe {
                glow::Context::from_loader_function(epoxy::get_proc_addr)
            })
        }
    }

    fn get_proc_address(_ctx: &GLContext, name: &str) -> *mut c_void {
        epoxy::get_proc_addr(name) as *mut c_void
    }
}

glib::wrapper! {
    pub struct MPVGLArea(ObjectSubclass<imp::MPVGLArea>)
        @extends gtk::Widget ,gtk::GLArea,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::ShortcutManager;
}

impl Default for MPVGLArea {
    fn default() -> Self {
        Self::new()
    }
}

impl MPVGLArea {
    pub fn new() -> Self {
        Object::builder().build()
    }

    pub fn is_ipc(&self) -> bool {
        // IPC mode requires a Unix domain socket; on Windows the IPC
        // backend is unavailable and we always fall back to the
        // in-process libmpv path.
        #[cfg(unix)]
        {
            !super::page::is_libmpv()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    #[cfg(unix)]
    fn ipc(&self) -> Option<std::cell::Ref<'_, super::mpv_ipc::MpvIpcClient>> {
        if !self.is_ipc() {
            return None;
        }
        let r = self.imp().ipc_client.borrow();
        if r.is_some() {
            Some(std::cell::Ref::map(r, |r| r.as_ref().unwrap()))
        } else {
            None
        }
    }

    #[cfg(not(unix))]
    fn ipc(&self) -> Option<std::cell::Ref<'_, super::mpv_ipc::MpvIpcClient>> {
        None
    }

    pub fn play(&self, url: &str, percentage: f64) {
        if self.is_ipc() {
            let url = url.to_owned();
            let vo = match SETTINGS.mpv_video_output() {
                2 => "dmabuf-wayland",
                _ => "gpu-next",
            };
            spawn(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                async move {
                    let url = JELLYFIN_CLIENT.get_streaming_url(&url).await;
                    info!("Now Playing (mpv IPC, vo={}): {}", vo, url);
                    let mut guard = obj.imp().ipc_client.borrow_mut();
                    let client = guard
                        .get_or_insert_with(super::mpv_ipc::MpvIpcClient::new);
                    client.play(&url, percentage, vo);
                }
            ));
            return;
        }
        let url = url.to_owned();

        spawn(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let mpv = &obj.imp().mpv();

                mpv.event_thread_alive
                    .store(ACTIVE, std::sync::atomic::Ordering::SeqCst);
                atomic_wait::wake_all(&*mpv.event_thread_alive);

                let url = JELLYFIN_CLIENT.get_streaming_url(&url).await;

                info!("Now Playing: {}", url);
                mpv.load_video(&url);

                mpv.set_start(percentage);

                mpv.pause(false);
            }
        ));
    }

    pub fn stop(&self) {
        if let Some(client) = self.ipc() {
            return client.command("stop", &[]);
        }
        self.imp().mpv().stop();
    }

    pub fn add_sub(&self, url: &str) {
        if let Some(client) = self.ipc() {
            return client.command("sub-add", &[url, "select"]);
        }
        self.imp().mpv().add_sub(url)
    }

    pub fn seek_forward(&self, value: i64) {
        if let Some(client) = self.ipc() {
            return client.command("seek", &[&value.to_string()]);
        }
        self.imp().mpv().seek_forward(value)
    }

    pub fn seek_backward(&self, value: i64) {
        if let Some(client) = self.ipc() {
            return client.command("seek", &[&(-value).to_string()]);
        }
        self.imp().mpv().seek_backward(value)
    }

    pub fn set_position(&self, value: f64) {
        if let Some(client) = self.ipc() {
            return client.set_property_f64("time-pos", value);
        }
        self.imp().mpv().set_position(value)
    }

    pub fn position(&self) -> f64 {
        if let Some(client) = self.ipc() {
            return client.position();
        }
        self.imp().mpv().position()
    }

    pub fn set_aid(&self, value: TrackSelection) {
        if let Some(client) = self.ipc() {
            return client.set_property_string("aid", &value.to_string());
        }
        self.imp().mpv().set_aid(value)
    }

    pub fn get_track_id(&self, type_: &str) -> i64 {
        if let Some(client) = self.ipc() {
            return client.get_track_id(type_);
        }
        self.imp().mpv().get_track_id(type_)
    }

    pub fn set_sid(&self, value: TrackSelection) {
        if let Some(client) = self.ipc() {
            return client.set_property_string("sid", &value.to_string());
        }
        self.imp().mpv().set_sid(value)
    }

    pub fn press_key(&self, key: u32, state: gtk::gdk::ModifierType) {
        if self.is_ipc() {
            return; // keyboard handled by mpv window itself
        }
        self.imp().mpv().press_key(key, state)
    }

    pub fn release_key(&self, key: u32, state: gtk::gdk::ModifierType) {
        if self.is_ipc() {
            return; // keyboard handled by mpv window itself
        }
        self.imp().mpv().release_key(key, state)
    }

    pub fn set_speed(&self, value: f64) {
        if let Some(client) = self.ipc() {
            return client.set_property_f64("speed", value);
        }
        self.imp().mpv().set_speed(value)
    }

    pub fn set_volume(&self, value: i64) {
        if let Some(client) = self.ipc() {
            return client.set_property_i64("volume", value);
        }
        self.imp().mpv().set_volume(value)
    }

    pub fn display_stats_toggle(&self) {
        if let Some(client) = self.ipc() {
            return client.command("script-binding", &["stats/display-stats-toggle"]);
        }
        self.imp().mpv().display_stats_toggle()
    }

    pub fn paused(&self) -> bool {
        if self.is_ipc() {
            return true; // pause state tracked via ListenEvent::Pause
        }
        self.imp().mpv().paused()
    }

    pub fn pause(&self) {
        if let Some(client) = self.ipc() {
            return client.command("cycle", &["pause"]);
        }
        self.imp().mpv().command_pause();
    }

    pub fn volume_scroll(&self, value: i64) {
        if let Some(client) = self.ipc() {
            return client.command("add", &["volume", &value.to_string()]);
        }
        self.imp().mpv().volume_scroll(value)
    }

    pub fn set_slang(&self, value: String) {
        if let Some(client) = self.ipc() {
            return client.set_property_string("slang", &value);
        }
        self.imp().mpv().set_slang(value)
    }

    pub fn set_property<V>(&self, property: &str, value: V)
    where
        V: SetData + Send + 'static,
    {
        if self.is_ipc() {
            return;
        }
        self.imp().mpv().set_property(property, value)
    }

    pub fn stop_ipc(&self) {
        if let Some(client) = self.imp().ipc_client.borrow_mut().take() {
            client.stop();
        }
    }

    pub fn clear_danmaku_overlay(&self) {
        if let Some(client) = self.ipc() {
            client.clear_danmaku_overlay();
        }
    }
}
