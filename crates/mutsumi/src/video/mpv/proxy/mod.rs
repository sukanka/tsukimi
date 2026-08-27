mod dmabuf;
mod drm;
#[cfg(feature = "profiling")]
pub mod profiling;
mod shm;
mod surface;
mod xdg;

#[cfg(feature = "profiling")]
pub use profiling::{
    ProxyProfilingGuard,
    start_proxy_profiling,
};
pub use shm::{
    ShmFrame,
    ShmMemoryFormat,
};

use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    os::fd::OwnedFd,
    rc::Rc,
    sync::Mutex,
    thread::JoinHandle,
};

use once_cell::sync::Lazy;
use wl_proxy::{
    baseline::Baseline,
    client::ClientHandler,
    global_mapper::GlobalMapper,
    object::{
        Object,
        ObjectCoreApi,
        ObjectRcUtils,
    },
    protocols::{
        ObjectInterface,
        drm::wl_drm::WlDrm,
        fractional_scale_v1::{
            wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
            wp_fractional_scale_v1::WpFractionalScaleV1,
        },
        linux_dmabuf_v1::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        viewporter::wp_viewporter::WpViewporter,
        wayland::{
            wl_callback::WlCallback,
            wl_compositor::WlCompositor,
            wl_display::{
                WlDisplay,
                WlDisplayHandler,
            },
            wl_registry::{
                WlRegistry,
                WlRegistryHandler,
            },
            wl_shm::{
                WlShm,
                WlShmFormat,
            },
            wl_subcompositor::WlSubcompositor,
            wl_surface::WlSurface,
        },
        xdg_shell::xdg_wm_base::XdgWmBase,
    },
    state::{
        Destructor,
        State,
    },
};

use self::{
    dmabuf::{
        ALLOWED_FORMAT_PAIRS,
        BufferInfo,
        DmabufHandler,
    },
    drm::DrmHandler,
    shm::ShmHandler,
    surface::{
        CompositorHandler,
        FractionalScaleManagerHandler,
        SubcompositorHandler,
        ViewporterHandler,
    },
    xdg::{
        ToplevelEntry,
        WmBaseHandler,
    },
};

enum ProxyEvent {
    ReleaseBuffer(u64),
    FrameDone {
        callback_batch_id: u64,
        time_ms: u32,
    },
}

fn send_proxy_event(tx: &flume::Sender<ProxyEvent>, event: ProxyEvent) {
    let _ = tx.send(event);
}

pub struct FrameCallbacks {
    callback_batch_id: u64,
    event_tx: flume::Sender<ProxyEvent>,
}

impl FrameCallbacks {
    pub fn done(self, time_ms: u32) {
        send_proxy_event(
            &self.event_tx,
            ProxyEvent::FrameDone {
                callback_batch_id: self.callback_batch_id,
                time_ms,
            },
        );
    }
}

pub struct DmabufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    buffer_id: u64,
    event_tx: flume::Sender<ProxyEvent>,
    #[cfg(feature = "profiling")]
    pub profile_frame_id: Option<u64>,
}

impl Drop for DmabufFrame {
    fn drop(&mut self) {
        send_proxy_event(&self.event_tx, ProxyEvent::ReleaseBuffer(self.buffer_id));
    }
}

pub enum SurfaceContentUpdate {
    Unchanged,
    Frame(DmabufFrame),
    Shm(shm::ShmFrame),
    Clear,
}

pub struct SurfaceUpdate {
    pub content: SurfaceContentUpdate,
    pub frame_callbacks: Option<FrameCallbacks>,
}

pub static FRAME_CHANNEL: Lazy<DmabufFrameChannel> = Lazy::new(|| {
    let (tx, rx) = flume::unbounded::<SurfaceUpdate>();
    DmabufFrameChannel { tx, rx }
});

pub struct DmabufFrameChannel {
    pub tx: flume::Sender<SurfaceUpdate>,
    pub rx: flume::Receiver<SurfaceUpdate>,
}

pub static VIEWPORT_CHANNEL: Lazy<ViewportChannel> = Lazy::new(|| {
    let (tx, rx) = flume::unbounded::<(i32, i32, f64)>();
    ViewportChannel { tx, rx }
});

pub struct ViewportChannel {
    pub tx: flume::Sender<(i32, i32, f64)>,
    pub rx: flume::Receiver<(i32, i32, f64)>,
}

static CURRENT_SCALE: Mutex<f64> = Mutex::new(1.0);

struct SharedState {
    buffer_info: HashMap<u64, BufferInfo>,
    event_tx: flume::Sender<ProxyEvent>,
    frame_callbacks: HashMap<u64, Vec<Rc<WlCallback>>>,
    next_callback_batch_id: u64,
    toplevels: Vec<ToplevelEntry>,
    configure_serial: u32,
    fractional_scales: Vec<Rc<WpFractionalScaleV1>>,
    surfaces: Vec<Rc<WlSurface>>,
    shm_buffer_info: HashMap<u64, shm::BufferInfo>,
}

impl SharedState {
    fn configure_toplevels(&mut self, width: i32, height: i32) {
        for entry in &self.toplevels {
            entry.toplevel.send_configure(width, height, &[]);
            entry.xdg_surface.send_configure(self.configure_serial);
            self.configure_serial = self.configure_serial.wrapping_add(1);
        }
    }

    fn update_fractional_scales(&mut self, scale_120: u32) {
        for scale in &self.fractional_scales {
            scale.send_preferred_scale(scale_120);
        }

        let buffer_scale = (scale_120 as f64 / 120.0).ceil() as i32;
        for surface in &self.surfaces {
            if surface.version() >= 6 {
                surface.send_preferred_buffer_scale(buffer_scale);
            }
        }
    }
}

struct DisplayHandler {
    state: Rc<RefCell<SharedState>>,
    lifecycle_tx: flume::Sender<ProxyLifecycleEvent>,
}

impl WlDisplayHandler for DisplayHandler {
    fn handle_get_registry(&mut self, slf: &Rc<WlDisplay>, registry: &Rc<WlRegistry>) {
        let _ = self.lifecycle_tx.send(ProxyLifecycleEvent::Connected);
        slf.send_get_registry(registry);

        let mut mapper = GlobalMapper::default();
        let xdg_wm_base_client_name =
            mapper.add_synthetic_global(registry, ObjectInterface::XdgWmBase, 4);
        let viewporter_client_name =
            mapper.add_synthetic_global(registry, ObjectInterface::WpViewporter, 1);
        let fractional_scale_manager_client_name =
            mapper.add_synthetic_global(registry, ObjectInterface::WpFractionalScaleManagerV1, 1);
        let shm_client_name = mapper.add_synthetic_global(registry, ObjectInterface::WlShm, 1);

        registry.set_handler(RegistryHandler {
            mapper,
            state: Rc::clone(&self.state),
            xdg_wm_base_client_name,
            viewporter_client_name,
            fractional_scale_manager_client_name,
            shm_client_name,
        });
    }
}

struct RegistryHandler {
    mapper: GlobalMapper,
    state: Rc<RefCell<SharedState>>,
    xdg_wm_base_client_name: u32,
    viewporter_client_name: u32,
    fractional_scale_manager_client_name: u32,
    shm_client_name: u32,
}

impl WlRegistryHandler for RegistryHandler {
    fn handle_global(
        &mut self, slf: &Rc<WlRegistry>, name: u32, interface: ObjectInterface, version: u32,
    ) {
        if interface == ObjectInterface::XdgWmBase
            || interface == ObjectInterface::WpViewporter
            || interface == ObjectInterface::WpFractionalScaleManagerV1
            || interface == ObjectInterface::WlShm
        {
            self.mapper.ignore_global(name);
        } else if interface == ObjectInterface::ZwpLinuxDmabufV1 {
            self.mapper
                .forward_global(slf, name, interface, version.min(4));
        } else {
            self.mapper.forward_global(slf, name, interface, version);
        }
    }

    fn handle_global_remove(&mut self, slf: &Rc<WlRegistry>, name: u32) {
        self.mapper.forward_global_remove(slf, name);
    }

    fn handle_bind(&mut self, slf: &Rc<WlRegistry>, name: u32, id: Rc<dyn Object>) {
        if name == self.xdg_wm_base_client_name {
            let wm_base = id.downcast::<XdgWmBase>();
            wm_base.set_forward_to_server(false);
            wm_base.set_handler(WmBaseHandler {
                state: Rc::clone(&self.state),
            });
        } else if name == self.viewporter_client_name {
            let viewporter = id.downcast::<WpViewporter>();
            viewporter.set_forward_to_server(false);
            viewporter.set_handler(ViewporterHandler);
        } else if name == self.fractional_scale_manager_client_name {
            let manager = id.downcast::<WpFractionalScaleManagerV1>();
            manager.set_forward_to_server(false);
            manager.set_handler(FractionalScaleManagerHandler {
                state: Rc::clone(&self.state),
            });
        } else if name == self.shm_client_name {
            let shm = id.downcast::<WlShm>();
            shm.set_forward_to_server(false);
            shm.set_handler(ShmHandler {
                state: Rc::clone(&self.state),
            });
            shm.send_format(WlShmFormat::ARGB8888);
            shm.send_format(WlShmFormat::XRGB8888);
        } else {
            let compositor = id.try_downcast::<WlCompositor>();
            let subcompositor = id.try_downcast::<WlSubcompositor>();
            let dmabuf = id.try_downcast::<ZwpLinuxDmabufV1>();
            let drm = id.try_downcast::<WlDrm>();

            self.mapper.forward_bind(slf, name, &id);

            if let Some(compositor) = compositor {
                compositor.set_handler(CompositorHandler {
                    state: Rc::clone(&self.state),
                });
            } else if let Some(subcompositor) = subcompositor {
                subcompositor.set_handler(SubcompositorHandler);
            } else if let Some(dmabuf) = dmabuf {
                dmabuf.set_handler(DmabufHandler {
                    state: Rc::clone(&self.state),
                });
            } else if let Some(drm) = drm {
                drm.set_handler(DrmHandler {
                    state: Rc::clone(&self.state),
                });
            }
        }
    }
}

struct ClientHandlerImpl {
    _destructor: Destructor,
}

impl ClientHandler for ClientHandlerImpl {
    fn disconnected(self: Box<Self>) {
        tracing::debug!("wl-proxy-mpv: client disconnected");
    }
}

fn handle_proxy_event(shared: &Rc<RefCell<SharedState>>, event: ProxyEvent) {
    match event {
        ProxyEvent::ReleaseBuffer(buffer_id) => {
            let shared = shared.borrow();
            if let Some(info) = shared.buffer_info.get(&buffer_id) {
                info.buffer.send_release();
            }
        }
        ProxyEvent::FrameDone {
            callback_batch_id,
            time_ms,
        } => {
            if let Some(callbacks) = shared
                .borrow_mut()
                .frame_callbacks
                .remove(&callback_batch_id)
            {
                for callback in callbacks {
                    callback.send_done(time_ms);
                    callback.delete_id();
                }
            }
        }
    }
}

fn handle_viewport_update(shared: &Rc<RefCell<SharedState>>, width: i32, height: i32, scale: f64) {
    shared.borrow_mut().configure_toplevels(width, height);
    *CURRENT_SCALE.lock().unwrap() = scale;
    let scale_120 = (scale * 120.0).round() as u32;
    shared.borrow_mut().update_fractional_scales(scale_120);
}

async fn run_client(
    state: Rc<State>, shared: Rc<RefCell<SharedState>>, event_rx: flume::Receiver<ProxyEvent>,
    stop_rx: flume::Receiver<()>,
) {
    let poll_fd = match tokio::io::unix::AsyncFd::new(Rc::clone(state.poll_fd())) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::error!("wl-proxy-mpv: failed to register poll fd: {e}");
            return;
        }
    };

    while state.is_not_destroyed() {
        if let Err(e) = state.dispatch_available() {
            tracing::error!("wl-proxy-mpv: dispatch failed: {e}");
            return;
        }

        if let Err(e) = state.before_poll() {
            tracing::error!("wl-proxy-mpv: failed to prepare poll: {e}");
            return;
        }

        tokio::select! {
            result = poll_fd.readable() => match result {
                Ok(mut guard) => guard.clear_ready(),
                Err(e) => {
                    tracing::error!("wl-proxy-mpv: failed to poll Wayland fd: {e}");
                    return;
                }
            },
            event = event_rx.recv_async() => match event {
                Ok(event) => handle_proxy_event(&shared, event),
                Err(_) => return,
            },
            viewport = VIEWPORT_CHANNEL.rx.recv_async() => match viewport {
                Ok(mut viewport) => {
                    while let Ok(latest) = VIEWPORT_CHANNEL.rx.try_recv() {
                        viewport = latest;
                    }
                    handle_viewport_update(&shared, viewport.0, viewport.1, viewport.2);
                }
                Err(_) => return,
            },
            _ = stop_rx.recv_async() => return,
        }
    }
}

fn serve_client(
    socket: OwnedFd, upstream: String, lifecycle_tx: flume::Sender<ProxyLifecycleEvent>,
    stop_rx: flume::Receiver<()>, ready_tx: std::sync::mpsc::SyncSender<io::Result<()>>,
) {
    let setup = (|| -> io::Result<_> {
        let state = State::builder(Baseline::ALL_OF_THEM)
            .with_server_display_name(&upstream)
            .build()
            .map_err(|e| io::Error::other(format!("failed to create Wayland state: {e}")))?;
        let client = state
            .add_client(&Rc::new(socket))
            .map_err(|e| io::Error::other(format!("failed to add Wayland client: {e}")))?;
        client.set_handler(ClientHandlerImpl {
            _destructor: state.create_destructor(),
        });

        let (event_tx, event_rx) = flume::unbounded();
        let shared = Rc::new(RefCell::new(SharedState {
            buffer_info: HashMap::new(),
            event_tx,
            frame_callbacks: HashMap::new(),
            next_callback_batch_id: 1,
            toplevels: Vec::new(),
            configure_serial: 1,
            fractional_scales: Vec::new(),
            surfaces: Vec::new(),
            shm_buffer_info: HashMap::new(),
        }));
        client.display().set_handler(DisplayHandler {
            state: Rc::clone(&shared),
            lifecycle_tx,
        });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .map_err(|e| io::Error::other(format!("failed to create runtime: {e}")))?;

        Ok((state, shared, event_rx, runtime))
    })();

    let (state, shared, event_rx, runtime) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    runtime.block_on(run_client(Rc::clone(&state), shared, event_rx, stop_rx));
    state.destroy();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProxyLifecycleEvent {
    Connected,
    Disconnected,
}

struct DisconnectNotifier(flume::Sender<ProxyLifecycleEvent>);

impl Drop for DisconnectNotifier {
    fn drop(&mut self) {
        let _ = self.0.send(ProxyLifecycleEvent::Disconnected);
    }
}

#[must_use = "keep the proxy connection alive while mpv owns its client fd"]
pub(crate) struct ProxyConnection {
    client_fd: Option<OwnedFd>,
    lifecycle_rx: flume::Receiver<ProxyLifecycleEvent>,
    stop_tx: Option<flume::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl ProxyConnection {
    pub(crate) fn take_client_fd(&mut self) -> Option<OwnedFd> {
        self.client_fd.take()
    }

    pub(crate) fn lifecycle_events(&self) -> &flume::Receiver<ProxyLifecycleEvent> {
        &self.lifecycle_rx
    }

    fn stop(&mut self) {
        drop(self.client_fd.take());
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.try_send(());
        }
        if let Some(join) = self.join.take() {
            join_in_background(join);
        }
    }
}

impl Drop for ProxyConnection {
    fn drop(&mut self) {
        self.stop();
    }
}

fn join_in_background(join: JoinHandle<()>) {
    if let Err(error) = std::thread::Builder::new()
        .name("wl-proxy-mpv-cleanup".into())
        .spawn(move || {
            if join.join().is_err() {
                tracing::error!("wl-proxy-mpv: proxy thread panicked while stopping");
            }
        })
    {
        tracing::warn!("wl-proxy-mpv: failed to spawn cleanup thread: {error}");
    }
}

pub fn create_mpv_proxy(format_pairs: Vec<(u32, u64)>) {
    ALLOWED_FORMAT_PAIRS
        .set(format_pairs.into_iter().collect())
        .ok();
}

pub(crate) fn start_mpv_proxy() -> io::Result<ProxyConnection> {
    let upstream = std::env::var("WAYLAND_DISPLAY").map_err(|error| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("WAYLAND_DISPLAY is unavailable: {error}"),
        )
    })?;
    let (client, server) = std::os::unix::net::UnixStream::pair()?;
    let client_fd = OwnedFd::from(client);
    let server_fd = OwnedFd::from(server);
    let (lifecycle_tx, lifecycle_rx) = flume::unbounded();
    let (stop_tx, stop_rx) = flume::bounded(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let join = std::thread::Builder::new()
        .name("wl-proxy-mpv".into())
        .spawn(move || {
            let _disconnect = DisconnectNotifier(lifecycle_tx.clone());
            serve_client(server_fd, upstream, lifecycle_tx, stop_rx, ready_tx);
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(ProxyConnection {
            client_fd: Some(client_fd),
            lifecycle_rx,
            stop_tx: Some(stop_tx),
            join: Some(join),
        }),
        Ok(Err(error)) => {
            let _ = stop_tx.try_send(());
            join_in_background(join);
            Err(error)
        }
        Err(error) => {
            let _ = stop_tx.try_send(());
            join_in_background(join);
            Err(io::Error::other(format!(
                "Wayland proxy thread exited before initialization completed: {error}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DisconnectNotifier,
        ProxyLifecycleEvent,
    };

    #[test]
    fn worker_exit_always_emits_disconnected() {
        let (tx, rx) = flume::unbounded();
        drop(DisconnectNotifier(tx));

        assert_eq!(
            rx.recv().expect("disconnect event"),
            ProxyLifecycleEvent::Disconnected
        );
    }

    #[test]
    fn connected_then_disconnected_events_preserve_order() {
        let (tx, rx) = flume::unbounded();
        tx.send(ProxyLifecycleEvent::Connected).unwrap();
        drop(DisconnectNotifier(tx));

        assert_eq!(rx.recv().unwrap(), ProxyLifecycleEvent::Connected);
        assert_eq!(rx.recv().unwrap(), ProxyLifecycleEvent::Disconnected);
    }
}
