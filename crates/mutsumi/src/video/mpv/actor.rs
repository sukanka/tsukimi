use std::{
    collections::VecDeque,
    ffi::OsString,
    ops::Deref,
    os::fd::{
        IntoRawFd,
        OwnedFd,
        RawFd,
    },
    sync::{
        Arc,
        Mutex,
        MutexGuard,
        OnceLock,
    },
};

use crate::MutsumiMpvError;

use super::{
    logging,
    proxy::{
        ProxyConnection,
        ProxyLifecycleEvent,
        start_mpv_proxy,
    },
    *,
};
use flume::{
    Receiver,
    Selector,
    Sender,
    unbounded,
};
use libmpv2::{
    Format,
    Mpv,
    events::{
        Event,
        PropertyData,
    },
};
use mutsumi_prelude::spawn_tokio_blocking;
use once_cell::sync::Lazy;
use serde_json::Value;

type MpvInitializer =
    dyn Fn(libmpv2::MpvInitializer) -> libmpv2::Result<()> + Send + Sync + 'static;

static MPV_INITIALIZER: OnceLock<Box<MpvInitializer>> = OnceLock::new();
static WAYLAND_HANDOFF_LOCK: Mutex<()> = Mutex::new(());
// Async property replies share mpv's event FIFO with StartFile/EndFile/Idle,
// so this token acts as a generation fence before an old proxy is discarded.
const IDLE_FENCE_REPLY: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaylandProxyPhase {
    Pending,
    Active,
}

impl WaylandProxyPhase {
    fn transition(self, event: ProxyLifecycleEvent) -> Option<Self> {
        match event {
            ProxyLifecycleEvent::Connected => Some(Self::Active),
            ProxyLifecycleEvent::Disconnected => None,
        }
    }

    fn survives_idle(
        self, has_current_vo: bool, handoff_is_published: bool, has_deferred_start: bool,
    ) -> bool {
        has_current_vo || (self == Self::Pending && handoff_is_published && has_deferred_start)
    }
}

struct WaylandSocketHandoff {
    previous: Option<OsString>,
    installed: OsString,
    raw_fd: Option<RawFd>,
    guard: Option<MutexGuard<'static, ()>>,
}

impl WaylandSocketHandoff {
    fn publish(client_fd: OwnedFd) -> Self {
        // WAYLAND_SOCKET is process-global. Keep this lock until libwayland
        // consumes the descriptor, which is signalled by get_registry.
        let guard = WAYLAND_HANDOFF_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("WAYLAND_SOCKET");
        let raw_fd = client_fd.into_raw_fd();
        let installed = OsString::from(raw_fd.to_string());
        unsafe { std::env::set_var("WAYLAND_SOCKET", &installed) };

        Self {
            previous,
            installed,
            raw_fd: Some(raw_fd),
            guard: Some(guard),
        }
    }

    fn rollback(mut self) {
        self.restore_environment();
        if let Some(raw_fd) = self.raw_fd.take() {
            unsafe { libc::close(raw_fd) };
        }
    }

    fn is_published(&self) -> bool {
        std::env::var_os("WAYLAND_SOCKET").as_ref() == Some(&self.installed)
    }

    fn restore_environment(&mut self) {
        let Some(guard) = self.guard.take() else {
            return;
        };

        let current = std::env::var_os("WAYLAND_SOCKET");
        if current.is_none() || current.as_ref() == Some(&self.installed) {
            match self.previous.take() {
                Some(previous) => unsafe { std::env::set_var("WAYLAND_SOCKET", previous) },
                None => unsafe { std::env::remove_var("WAYLAND_SOCKET") },
            }
        } else {
            tracing::warn!(
                target: "mutsumi::mpv",
                "WAYLAND_SOCKET changed during the mpv proxy handoff; preserving the newer value"
            );
        }

        drop(guard);
    }
}

impl Drop for WaylandSocketHandoff {
    fn drop(&mut self) {
        self.restore_environment();
        // Once mpv accepts the command, ownership of this fd is ambiguous
        // until libwayland consumes it. Closing it here could race with mpv.
    }
}

struct WaylandProxy {
    connection: ProxyConnection,
    phase: WaylandProxyPhase,
    handoff: Option<WaylandSocketHandoff>,
}

impl WaylandProxy {
    fn pending(connection: ProxyConnection, handoff: WaylandSocketHandoff) -> Self {
        Self {
            connection,
            phase: WaylandProxyPhase::Pending,
            handoff: Some(handoff),
        }
    }

    fn event_rx(&self) -> &Receiver<ProxyLifecycleEvent> {
        self.connection.lifecycle_events()
    }

    fn mark_connected(&mut self) {
        self.phase = self
            .phase
            .transition(ProxyLifecycleEvent::Connected)
            .expect("a connection event keeps the proxy alive");
        drop(self.handoff.take());
    }

    fn cancel_if_unclaimed(mut self) {
        if let Some(handoff) = self.handoff.take()
            && handoff.is_published()
        {
            handoff.rollback();
        }
    }
}

enum ActorInput {
    Control(Result<MpvMessage, flume::RecvError>),
    Proxy(Result<ProxyLifecycleEvent, flume::RecvError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackLifecycle {
    Idle,
    StartingFromIdle,
    StartingFromPlayback,
    Active,
    Ended,
}

impl PlaybackLifecycle {
    fn start_requested(self) -> Self {
        match self {
            Self::Idle | Self::StartingFromIdle => Self::StartingFromIdle,
            Self::StartingFromPlayback | Self::Active | Self::Ended => Self::StartingFromPlayback,
        }
    }

    fn can_finish_at_fence_reply(self) -> bool {
        matches!(self, Self::Idle | Self::StartingFromIdle)
    }

    fn is_playing_or_starting(self) -> bool {
        matches!(
            self,
            Self::StartingFromIdle | Self::StartingFromPlayback | Self::Active
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleCleanup {
    Ready,
    AwaitingFence,
    AwaitingIdle,
}

impl IdleCleanup {
    fn begin(&mut self) {
        if *self == Self::Ready {
            *self = Self::AwaitingFence;
        }
    }

    fn fence_reply(&mut self, playback: PlaybackLifecycle) -> bool {
        if *self != Self::AwaitingFence {
            return false;
        }

        if playback.can_finish_at_fence_reply() {
            *self = Self::Ready;
            true
        } else {
            *self = Self::AwaitingIdle;
            false
        }
    }

    fn finish_idle(&mut self, playback: PlaybackLifecycle) -> bool {
        match *self {
            Self::AwaitingIdle => {
                *self = Self::Ready;
                true
            }
            Self::Ready => !playback.is_playing_or_starting(),
            Self::AwaitingFence => false,
        }
    }

    fn should_defer(self, message: &MpvMessage) -> bool {
        if self == Self::Ready {
            return false;
        }

        match message {
            MpvMessage::GetProperty { .. }
            | MpvMessage::InitRenderContext(_)
            | MpvMessage::StartFile
            | MpvMessage::EndFile
            | MpvMessage::Idle
            | MpvMessage::FenceReply(_)
            | MpvMessage::Shutdown => false,
            MpvMessage::Command { cmd, .. } if cmd == "quit" => false,
            _ => true,
        }
    }
}

fn mark_file_start_requested(playback: &mut PlaybackLifecycle) {
    *playback = playback.start_requested();
}

fn mark_file_started(playback: &mut PlaybackLifecycle) {
    *playback = PlaybackLifecycle::Active;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the mpv initializer has already been set")]
pub struct MpvInitializerAlreadySet;

pub fn set_mpv_initializer<F>(initializer: F) -> Result<(), MpvInitializerAlreadySet>
where
    F: Fn(libmpv2::MpvInitializer) -> libmpv2::Result<()> + Send + Sync + 'static,
{
    MPV_INITIALIZER
        .set(Box::new(initializer))
        .map_err(|_| MpvInitializerAlreadySet)
}

struct SendMpv {
    mpv: Arc<Mpv>,
    has_file: std::cell::Cell<bool>,
}
unsafe impl Send for SendMpv {}

#[derive(Debug, Clone)]
pub enum MpvValue {
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
}

#[derive(Debug, Clone)]
pub enum MpvValueType {
    Bool,
    I64,
    F64,
    String,
}

impl From<i32> for MpvValue {
    fn from(val: i32) -> Self {
        MpvValue::I64(val as i64)
    }
}

impl From<u32> for MpvValue {
    fn from(val: u32) -> Self {
        MpvValue::I64(val as i64)
    }
}

impl From<bool> for MpvValue {
    fn from(val: bool) -> Self {
        MpvValue::Bool(val)
    }
}

impl From<i64> for MpvValue {
    fn from(val: i64) -> Self {
        MpvValue::I64(val)
    }
}

impl From<f64> for MpvValue {
    fn from(val: f64) -> Self {
        MpvValue::F64(val)
    }
}

impl From<String> for MpvValue {
    fn from(val: String) -> Self {
        MpvValue::String(val)
    }
}

impl From<&str> for MpvValue {
    fn from(val: &str) -> Self {
        MpvValue::String(val.to_string())
    }
}

impl MpvValue {
    pub fn set_on(&self, mpv: &Mpv, property: &str) -> libmpv2::Result<()> {
        match self {
            MpvValue::Bool(v) => mpv.set_property(property, *v),
            MpvValue::I64(v) => mpv.set_property(property, *v),
            MpvValue::F64(v) => mpv.set_property(property, *v),
            MpvValue::String(v) => mpv.set_property(property, v.as_str()),
        }
    }
}

pub enum MpvMessage {
    Command {
        cmd: String,
        args: Vec<String>,
    },
    WaylandCommand {
        cmd: String,
        args: Vec<String>,
    },
    SetPlaylistPos(i64),
    SetProperty {
        property: String,
        value: MpvValue,
    },
    GetProperty {
        property: String,
        value_type: MpvValueType,
        tx: tokio::sync::oneshot::Sender<MpvValue>,
    },
    InitRenderContext(tokio::sync::oneshot::Sender<Arc<Mpv>>),
    StartFile,
    EndFile,
    Idle,
    FenceReply(u64),
    Shutdown,
}

pub static MPV_CTRL: Lazy<MpvCtrl> = Lazy::new(|| {
    let (tx, rx) = unbounded::<MpvMessage>();

    MpvCtrl { tx, rx }
});

pub struct MpvCtrl {
    pub tx: Sender<MpvMessage>,
    pub rx: Receiver<MpvMessage>,
}

#[derive(Clone, Copy)]
pub struct MpvActor {
    _phantom: std::marker::PhantomData<()>,
}

impl Deref for SendMpv {
    type Target = Mpv;

    fn deref(&self) -> &Self::Target {
        &self.mpv
    }
}

impl Default for MpvActor {
    fn default() -> Self {
        Self::new()
    }
}

impl MpvActor {
    pub fn new() -> Self {
        Self::with_initializer(|mpv| {
            mpv.set_option("input-default-bindings", "yes")?;
            mpv.set_option("hwdec", "auto-safe")?;
            mpv.set_option("keep-open", "yes")?;
            mpv.set_option("vo", "gpu-next")?;

            if let Some(initializer) = MPV_INITIALIZER.get() {
                initializer(mpv)?;
            }

            Ok(())
        })
        .expect("Failed to create mpv instance")
    }

    pub fn with_initializer<F>(initializer: F) -> libmpv2::Result<Self>
    where
        F: FnOnce(libmpv2::MpvInitializer) -> libmpv2::Result<()>,
    {
        let mpv = Mpv::with_initializer(initializer)?;
        logging::request_logs(&mpv);
        tracing::debug!(target: "mutsumi::mpv", "mpv instance initialized");

        mpv.disable_deprecated_events()?;
        // MPV_EVENT_IDLE is emitted after stop has finished deciding whether
        // to retain or destroy the VO, making it a lifecycle barrier for the
        // Wayland proxy. libmpv2 exposes this deprecated event as raw data.
        mpv.enable_event(libmpv2_sys::mpv_event_id_MPV_EVENT_IDLE)?;

        mpv.observe_property("duration", Format::Double, 0)?;
        mpv.observe_property("pause", Format::Flag, 1)?;
        mpv.observe_property("cache-speed", Format::Int64, 2)?;
        mpv.observe_property("track-list", Format::String, 3)?;
        mpv.observe_property("paused-for-cache", Format::Flag, 4)?;
        mpv.observe_property("demuxer-cache-time", Format::Int64, 5)?;
        mpv.observe_property("time-pos", Format::Int64, 6)?;
        mpv.observe_property("volume", Format::Int64, 7)?;
        mpv.observe_property("chapter-list", Format::String, 8)?;
        mpv.observe_property("speed", Format::Double, 9)?;
        mpv.observe_property("playlist", Format::String, 10)?;
        mpv.observe_property("eof-reached", Format::Flag, 11)?;

        let mpv = SendMpv {
            mpv: Arc::new(mpv),
            has_file: std::cell::Cell::new(false),
        };

        let event_mpv = SendMpv {
            mpv: Arc::clone(&mpv.mpv),
            has_file: std::cell::Cell::new(false),
        };
        std::thread::Builder::new()
            .name("mpv event loop".into())
            .spawn(move || while event_mpv.handle_event() {})
            .expect("Failed to spawn mpv event thread");

        spawn_tokio_blocking(move || {
            let mut proxy: Option<WaylandProxy> = None;
            let mut playback_lifecycle = PlaybackLifecycle::Idle;
            let mut idle_cleanup = IdleCleanup::Ready;
            let mut deferred_messages = VecDeque::new();
            let mut replay = VecDeque::new();

            loop {
                let msg = match replay.pop_front() {
                    Some(msg) => {
                        drain_proxy_events(&mut proxy);
                        msg
                    }
                    None => match receive_actor_input(proxy.as_ref()) {
                        ActorInput::Control(Ok(msg)) => {
                            // Prefer a queued disconnect over reusing a dead proxy.
                            drain_proxy_events(&mut proxy);
                            msg
                        }
                        ActorInput::Control(Err(_)) => break,
                        ActorInput::Proxy(event) => {
                            handle_proxy_event(&mut proxy, event);
                            continue;
                        }
                    },
                };

                if should_defer_start_until_idle(playback_lifecycle, idle_cleanup, &msg)
                    || idle_cleanup.should_defer(&msg)
                {
                    deferred_messages.push_back(msg);
                    continue;
                }

                match msg {
                    MpvMessage::Command { cmd, args } => {
                        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
                        let needs_idle_barrier = cmd == "stop"
                            && proxy.is_some()
                            && playback_lifecycle != PlaybackLifecycle::Idle;
                        let result = mpv.command(&cmd, &args_ref);
                        if needs_idle_barrier && result.is_ok() {
                            begin_idle_cleanup(&mpv, &mut idle_cleanup);
                        }
                    }
                    MpvMessage::WaylandCommand { cmd, args } => {
                        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
                        if run_wayland_command(&mpv, &mut proxy, &cmd, &args_ref)
                            && cmd == "loadfile"
                        {
                            mark_file_start_requested(&mut playback_lifecycle);
                        }
                    }
                    MpvMessage::SetPlaylistPos(pos) => {
                        let playlist_count = mpv
                            .get_property::<i64>("playlist-count")
                            .unwrap_or(i64::MAX);
                        let current_pos = mpv.get_property::<i64>("playlist-pos").unwrap_or(-1);
                        let starts_file = pos >= 0 && pos < playlist_count && pos != current_pos;

                        if starts_file {
                            let pos = pos.to_string();
                            if run_wayland_command(
                                &mpv,
                                &mut proxy,
                                "set",
                                &["playlist-pos", pos.as_str()],
                            ) {
                                mark_file_start_requested(&mut playback_lifecycle);
                            }
                        } else {
                            let needs_idle_barrier = (pos < 0 || pos >= playlist_count)
                                && proxy.is_some()
                                && playback_lifecycle != PlaybackLifecycle::Idle;
                            let result = mpv.set_property("playlist-pos", pos);
                            if needs_idle_barrier && result.is_ok() {
                                begin_idle_cleanup(&mpv, &mut idle_cleanup);
                            }
                        }
                    }
                    MpvMessage::SetProperty { property, value } => {
                        let _ = value.set_on(&mpv, &property);
                    }
                    MpvMessage::GetProperty {
                        property,
                        value_type,
                        tx,
                    } => {
                        if let Ok(result) = mpv.get_property_value(&property, value_type) {
                            let _ = tx.send(result);
                        }
                    }
                    MpvMessage::InitRenderContext(tx) => {
                        let _ = tx.send(Arc::clone(&mpv.mpv));
                    }
                    MpvMessage::StartFile => {
                        mark_file_started(&mut playback_lifecycle);
                        if idle_cleanup == IdleCleanup::Ready {
                            replay.append(&mut deferred_messages);
                        }
                    }
                    MpvMessage::EndFile => {
                        playback_lifecycle = PlaybackLifecycle::Ended;
                    }
                    MpvMessage::Idle => {
                        if idle_cleanup == IdleCleanup::AwaitingFence {
                            // Record the latest lifecycle state, but wait for
                            // the FIFO lifecycle fence before releasing loads.
                            playback_lifecycle = PlaybackLifecycle::Idle;
                        } else if idle_cleanup.finish_idle(playback_lifecycle) {
                            complete_idle_cleanup(
                                &mpv,
                                &mut proxy,
                                &mut playback_lifecycle,
                                &mut deferred_messages,
                                &mut replay,
                            );
                        }
                    }
                    MpvMessage::FenceReply(reply) => {
                        if reply == IDLE_FENCE_REPLY && idle_cleanup.fence_reply(playback_lifecycle)
                        {
                            complete_idle_cleanup(
                                &mpv,
                                &mut proxy,
                                &mut playback_lifecycle,
                                &mut deferred_messages,
                                &mut replay,
                            );
                        }
                    }
                    MpvMessage::Shutdown => {
                        if let Some(proxy) = proxy.take() {
                            proxy.cancel_if_unclaimed();
                        }
                        break;
                    }
                }
            }
        });

        Ok(Self {
            _phantom: std::marker::PhantomData,
        })
    }

    pub fn set_property<V>(&self, property: &str, value: V)
    where
        V: Into<MpvValue>,
    {
        if let Err(error) = MPV_CTRL.tx.send(MpvMessage::SetProperty {
            property: property.to_owned(),
            value: value.into(),
        }) {
            tracing::warn!(target: "mutsumi::mpv", %error, "mpv actor is unavailable");
        }
    }

    pub async fn get_property(
        &self, property: &str, value_type: MpvValueType,
    ) -> Result<MpvValue, tokio::sync::oneshot::error::RecvError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<MpvValue>();
        if let Err(error) = MPV_CTRL.tx.send(MpvMessage::GetProperty {
            property: property.to_owned(),
            value_type,
            tx,
        }) {
            tracing::warn!(target: "mutsumi::mpv", %error, "mpv actor is unavailable");
        }

        rx.await
    }

    pub fn command(&self, cmd: &str, args: &[&str]) {
        let cmd_owned = cmd.to_string();
        let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        if let Err(error) = MPV_CTRL.tx.send(MpvMessage::Command {
            cmd: cmd_owned,
            args: args_owned,
        }) {
            tracing::warn!(target: "mutsumi::mpv", %error, "mpv actor is unavailable");
        }
    }

    pub fn wayland_command(&self, cmd: &str, args: &[&str]) {
        let cmd_owned = cmd.to_string();
        let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        if let Err(error) = MPV_CTRL.tx.send(MpvMessage::WaylandCommand {
            cmd: cmd_owned,
            args: args_owned,
        }) {
            tracing::warn!(target: "mutsumi::mpv", %error, "mpv actor is unavailable");
        }
    }

    pub fn set_playlist_pos(&self, pos: i64) {
        if let Err(error) = MPV_CTRL.tx.send(MpvMessage::SetPlaylistPos(pos)) {
            tracing::warn!(target: "mutsumi::mpv", %error, "mpv actor is unavailable");
        }
    }
}

fn receive_actor_input(proxy: Option<&WaylandProxy>) -> ActorInput {
    match proxy {
        Some(proxy) => Selector::new()
            .recv(&MPV_CTRL.rx, ActorInput::Control)
            .recv(proxy.event_rx(), ActorInput::Proxy)
            .wait(),
        None => ActorInput::Control(MPV_CTRL.rx.recv()),
    }
}

fn requests_file_start(message: &MpvMessage) -> bool {
    matches!(
        message,
        MpvMessage::WaylandCommand { cmd, .. } if cmd == "loadfile"
    ) || matches!(message, MpvMessage::SetPlaylistPos(0..))
}

fn has_deferred_file_start(mpv: &Mpv, messages: &VecDeque<MpvMessage>) -> bool {
    let playlist_count = mpv
        .get_property::<i64>("playlist-count")
        .unwrap_or(i64::MAX);

    messages.iter().any(|message| match message {
        MpvMessage::WaylandCommand { cmd, .. } => cmd == "loadfile",
        MpvMessage::SetPlaylistPos(pos) => *pos >= 0 && *pos < playlist_count,
        _ => false,
    })
}

fn should_defer_start_until_idle(
    playback: PlaybackLifecycle, cleanup: IdleCleanup, message: &MpvMessage,
) -> bool {
    cleanup == IdleCleanup::Ready
        && playback == PlaybackLifecycle::Ended
        && requests_file_start(message)
}

fn complete_idle_cleanup(
    mpv: &Mpv, proxy: &mut Option<WaylandProxy>, playback: &mut PlaybackLifecycle,
    deferred_messages: &mut VecDeque<MpvMessage>, replay: &mut VecDeque<MpvMessage>,
) {
    *playback = PlaybackLifecycle::Idle;
    let has_deferred_start = has_deferred_file_start(mpv, deferred_messages);
    reconcile_proxy_after_idle(mpv, proxy, has_deferred_start);
    replay.append(deferred_messages);
}

fn begin_idle_cleanup(mpv: &Mpv, cleanup: &mut IdleCleanup) {
    match queue_idle_fence(mpv) {
        Ok(()) => cleanup.begin(),
        Err(error) => tracing::error!(
            target: "mutsumi::mpv",
            %error,
            "failed to queue the mpv idle lifecycle fence"
        ),
    }
}

fn queue_idle_fence(mpv: &Mpv) -> libmpv2::Result<()> {
    // This always-available property gives us a side-effect-free reply in the
    // same event FIFO after the preceding stop-like operation.
    let result = unsafe {
        libmpv2_sys::mpv_get_property_async(
            mpv.ctx.as_ptr(),
            IDLE_FENCE_REPLY,
            c"idle-active".as_ptr(),
            libmpv2_sys::mpv_format_MPV_FORMAT_FLAG,
        )
    };

    if result < 0 {
        Err(libmpv2::Error::Raw(result))
    } else {
        Ok(())
    }
}

fn reconcile_proxy_after_idle(
    mpv: &Mpv, proxy: &mut Option<WaylandProxy>, has_deferred_start: bool,
) {
    let Some(current) = proxy.as_ref() else {
        return;
    };
    let has_current_vo = mpv
        .get_property::<String>("current-vo")
        .is_ok_and(|vo| !vo.is_empty());
    let handoff_is_published = current
        .handoff
        .as_ref()
        .is_some_and(WaylandSocketHandoff::is_published);

    if !current
        .phase
        .survives_idle(has_current_vo, handoff_is_published, has_deferred_start)
    {
        tracing::debug!(
            target: "mutsumi::mpv",
            "mpv destroyed its VO while stopping; rebuilding the Wayland proxy"
        );
        if let Some(proxy) = proxy.take() {
            proxy.cancel_if_unclaimed();
        }
    }
}

fn drain_proxy_events(proxy: &mut Option<WaylandProxy>) {
    while let Some(rx) = proxy.as_ref().map(WaylandProxy::event_rx) {
        let event = match rx.try_recv() {
            Ok(event) => Ok(event),
            Err(flume::TryRecvError::Disconnected) => Err(flume::RecvError::Disconnected),
            Err(flume::TryRecvError::Empty) => break,
        };
        handle_proxy_event(proxy, event);
    }
}

fn handle_proxy_event(
    proxy: &mut Option<WaylandProxy>, event: Result<ProxyLifecycleEvent, flume::RecvError>,
) {
    let event = event.unwrap_or(ProxyLifecycleEvent::Disconnected);
    match event {
        ProxyLifecycleEvent::Connected => {
            if let Some(proxy) = proxy.as_mut() {
                let was_pending = proxy.phase == WaylandProxyPhase::Pending;
                proxy.mark_connected();
                if was_pending {
                    tracing::debug!(
                        target: "mutsumi::mpv",
                        "mpv connected to the Wayland proxy"
                    );
                }
            }
        }
        ProxyLifecycleEvent::Disconnected => {
            let Some(disconnected) = proxy.take() else {
                return;
            };
            let was_pending = disconnected.phase == WaylandProxyPhase::Pending;
            debug_assert_eq!(
                disconnected
                    .phase
                    .transition(ProxyLifecycleEvent::Disconnected),
                None
            );
            if was_pending {
                // The worker can exit just before mpv consumes the submitted
                // descriptor. Closing it here could race with that transfer.
                drop(disconnected);
                // mpv reports media failures with the matching playback
                // request. A proxy-level error here has no generation and
                // could otherwise fail a newer load after stop/switch.
                tracing::warn!(
                    target: "mutsumi::mpv",
                    "Wayland proxy exited before mpv connected"
                );
            } else {
                drop(disconnected);
                tracing::debug!(
                    target: "mutsumi::mpv",
                    "mpv disconnected from the Wayland proxy"
                );
            }
        }
    }
}

fn run_wayland_command(
    mpv: &Mpv, proxy: &mut Option<WaylandProxy>, command: &str, args: &[&str],
) -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return mpv.command(command, args).is_ok();
    }

    if proxy.is_some() {
        return match mpv.command(command, args) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(
                    target: "mutsumi::mpv",
                    %error,
                    "Wayland MPV command failed"
                );
                false
            }
        };
    }

    let mut connection = match start_mpv_proxy() {
        Ok(connection) => connection,
        Err(error) => {
            report_wayland_handoff_failure(error.to_string());
            return false;
        }
    };
    let Some(client_fd) = connection.take_client_fd() else {
        report_wayland_handoff_failure(
            "Wayland proxy did not return a client file descriptor".to_string(),
        );
        return false;
    };
    let handoff = WaylandSocketHandoff::publish(client_fd);

    match mpv.command(command, args) {
        Ok(()) => {
            *proxy = Some(WaylandProxy::pending(connection, handoff));
            true
        }
        Err(error) => {
            handoff.rollback();
            report_wayland_handoff_failure(error.to_string());
            false
        }
    }
}

fn report_wayland_handoff_failure(error: String) {
    tracing::error!(
        target: "mutsumi::mpv",
        %error,
        "Wayland proxy handoff failed"
    );
    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Error(error));
}

#[cfg(test)]
mod tests {
    use super::{
        IdleCleanup,
        MpvMessage,
        PlaybackLifecycle,
        ProxyLifecycleEvent,
        WaylandProxyPhase,
        mark_file_start_requested,
        mark_file_started,
        should_defer_start_until_idle,
    };

    #[test]
    fn pending_proxy_becomes_active_only_after_connection() {
        assert_eq!(
            WaylandProxyPhase::Pending.transition(ProxyLifecycleEvent::Connected),
            Some(WaylandProxyPhase::Active)
        );
    }

    #[test]
    fn disconnected_proxy_must_be_rebuilt() {
        for phase in [WaylandProxyPhase::Pending, WaylandProxyPhase::Active] {
            assert_eq!(phase.transition(ProxyLifecycleEvent::Disconnected), None);
        }
    }

    #[test]
    fn duplicate_connection_event_is_idempotent() {
        assert_eq!(
            WaylandProxyPhase::Active.transition(ProxyLifecycleEvent::Connected),
            Some(WaylandProxyPhase::Active)
        );
    }

    #[test]
    fn idle_start_then_stop_completes_at_fence_reply() {
        let mut playback = PlaybackLifecycle::Idle;
        let mut cleanup = IdleCleanup::Ready;

        mark_file_start_requested(&mut playback);
        cleanup.begin();

        assert_eq!(playback, PlaybackLifecycle::StartingFromIdle);
        assert!(cleanup.fence_reply(playback));
        assert_eq!(cleanup, IdleCleanup::Ready);
    }

    #[test]
    fn started_file_waits_for_idle_after_fence_reply() {
        let mut playback = PlaybackLifecycle::Idle;
        let mut cleanup = IdleCleanup::Ready;

        mark_file_start_requested(&mut playback);
        cleanup.begin();
        mark_file_started(&mut playback);

        assert!(!cleanup.fence_reply(playback));
        assert_eq!(cleanup, IdleCleanup::AwaitingIdle);
        assert!(cleanup.finish_idle(PlaybackLifecycle::Ended));
    }

    #[test]
    fn end_file_before_fence_reply_still_waits_for_idle() {
        let mut cleanup = IdleCleanup::Ready;
        cleanup.begin();

        assert!(!cleanup.fence_reply(PlaybackLifecycle::Ended));
        assert_eq!(cleanup, IdleCleanup::AwaitingIdle);
        assert!(cleanup.finish_idle(PlaybackLifecycle::Ended));
    }

    #[test]
    fn idle_observed_before_fence_reply_completes_the_barrier() {
        let mut cleanup = IdleCleanup::Ready;
        cleanup.begin();

        assert!(cleanup.fence_reply(PlaybackLifecycle::Idle));
        assert_eq!(cleanup, IdleCleanup::Ready);
    }

    #[test]
    fn active_replacement_preserves_non_idle_origin() {
        let mut playback = PlaybackLifecycle::Active;
        let mut cleanup = IdleCleanup::Ready;

        mark_file_start_requested(&mut playback);
        mark_file_start_requested(&mut playback);
        cleanup.begin();

        assert_eq!(playback, PlaybackLifecycle::StartingFromPlayback);
        assert!(!cleanup.fence_reply(playback));
        assert_eq!(cleanup, IdleCleanup::AwaitingIdle);
    }

    #[test]
    fn start_file_after_an_old_idle_still_waits_for_the_stop_idle() {
        let mut playback = PlaybackLifecycle::Idle;
        let mut cleanup = IdleCleanup::Ready;

        mark_file_start_requested(&mut playback);
        cleanup.begin();
        playback = PlaybackLifecycle::Idle;
        mark_file_started(&mut playback);

        assert_eq!(playback, PlaybackLifecycle::Active);
        assert!(!cleanup.fence_reply(playback));
        assert_eq!(cleanup, IdleCleanup::AwaitingIdle);
    }

    #[test]
    fn natural_idle_reconciles_only_after_end_file() {
        let mut cleanup = IdleCleanup::Ready;

        assert!(!cleanup.finish_idle(PlaybackLifecycle::Active));
        assert!(!cleanup.finish_idle(PlaybackLifecycle::StartingFromIdle));
        assert!(cleanup.finish_idle(PlaybackLifecycle::Ended));
    }

    #[test]
    fn start_after_end_file_waits_for_the_matching_idle() {
        let cleanup = IdleCleanup::Ready;
        let load = MpvMessage::WaylandCommand {
            cmd: "loadfile".to_string(),
            args: vec!["video.mkv".to_string(), "replace".to_string()],
        };

        assert!(should_defer_start_until_idle(
            PlaybackLifecycle::Ended,
            cleanup,
            &load
        ));
        assert!(!should_defer_start_until_idle(
            PlaybackLifecycle::Idle,
            cleanup,
            &load
        ));
    }

    #[test]
    fn idle_cleanup_defers_mutations_but_not_lifecycle_messages() {
        let cleanup = IdleCleanup::AwaitingFence;
        let load = MpvMessage::WaylandCommand {
            cmd: "loadfile".to_string(),
            args: vec!["video.mkv".to_string(), "replace".to_string()],
        };

        assert!(cleanup.should_defer(&load));
        assert!(!cleanup.should_defer(&MpvMessage::StartFile));
        assert!(!cleanup.should_defer(&MpvMessage::EndFile));
        assert!(!cleanup.should_defer(&MpvMessage::Idle));
        assert!(!cleanup.should_defer(&MpvMessage::FenceReply(7)));
        assert!(!cleanup.should_defer(&MpvMessage::Shutdown));
    }

    #[test]
    fn idle_barrier_reuses_pending_or_retained_vo_only() {
        assert!(WaylandProxyPhase::Pending.survives_idle(false, true, true));
        assert!(!WaylandProxyPhase::Pending.survives_idle(false, true, false));
        assert!(!WaylandProxyPhase::Pending.survives_idle(false, false, true));
        assert!(WaylandProxyPhase::Active.survives_idle(true, false, true));
        assert!(!WaylandProxyPhase::Active.survives_idle(false, false, true));
    }
}

impl SendMpv {
    // Blocks until the next event. Returns false once mpv shuts down.
    fn handle_event(&self) -> bool {
        let Some(event) = self.wait_event(1.0) else {
            return true;
        };

        match event {
            Ok(event) => match event {
                Event::LogMessage {
                    prefix,
                    text,
                    log_level,
                    ..
                } => logging::emit_log(prefix, log_level, text),
                Event::PropertyChange { name, change, .. } => match name {
                    "duration" => {
                        if let PropertyData::Double(dur) = change {
                            let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Duration(dur));
                        }
                    }
                    "pause" => {
                        if let PropertyData::Flag(pause) = change
                            && (self.has_file.get() || pause)
                        {
                            let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Pause(pause));
                        }
                    }
                    "cache-speed" => {
                        if let PropertyData::Int64(speed) = change {
                            let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::CacheSpeed(speed));
                        }
                    }
                    "track-list" => {
                        if let PropertyData::Str(node) = change {
                            let _ = MPV_EVENT_CHANNEL
                                .tx
                                .send(ListenEvent::TrackList(node_to_tracks(node)));
                        }
                    }
                    "chapter-list" => {
                        if let PropertyData::Str(node) = change {
                            let _ = MPV_EVENT_CHANNEL
                                .tx
                                .send(ListenEvent::ChapterList(node_to_chapter_list(node)));
                        }
                    }
                    "playlist" => {
                        if let PropertyData::Str(node) = change {
                            let _ = MPV_EVENT_CHANNEL
                                .tx
                                .send(ListenEvent::Playlist(node_to_playlist(node)));
                        }
                    }
                    "volume" => {
                        if let PropertyData::Int64(volume) = change {
                            let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Volume(volume));
                        }
                    }
                    "speed" => {
                        if let PropertyData::Double(speed) = change {
                            let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Speed(speed));
                        }
                    }
                    "demuxer-cache-time" => {
                        if let PropertyData::Int64(time) = change {
                            let _ = MPV_EVENT_CHANNEL
                                .tx
                                .send(ListenEvent::DemuxerCacheTime(time));
                        }
                    }
                    "time-pos" => {
                        if let PropertyData::Int64(time) = change {
                            let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::TimePos(time));
                        }
                    }
                    "eof-reached" => {
                        if let PropertyData::Flag(true) = change {
                            let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::PlaybackEnded);
                        }
                    }
                    "paused-for-cache" => {
                        if let PropertyData::Flag(pause) = change {
                            let seeking = self.get_property::<bool>("seeking").unwrap_or(false);
                            let time_millis =
                                self.get_property::<f64>("audio-pts").unwrap_or(0.0) * 1000.0;
                            let _ = MPV_EVENT_CHANNEL
                                .tx
                                .send(ListenEvent::PausedForCache(pause || seeking, time_millis));
                        }
                    }
                    _ => {}
                },
                Event::Seek { .. } => {
                    let time_millis = self.get_property::<f64>("audio-pts").unwrap_or(0.0) * 1000.0;
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Seek(time_millis));
                }
                Event::PlaybackRestart { .. } => {
                    let time_millis = self.get_property::<f64>("audio-pts").unwrap_or(0.0) * 1000.0;
                    let _ = MPV_EVENT_CHANNEL
                        .tx
                        .send(ListenEvent::PlaybackRestart(time_millis));
                }
                Event::FileLoaded => {
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::FileLoaded);
                }
                Event::EndFile(reason) => {
                    self.has_file.set(false);
                    let _ = MPV_CTRL.tx.send(MpvMessage::EndFile);
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Eof(reason));
                }
                Event::StartFile => {
                    self.has_file.set(true);
                    let _ = MPV_CTRL.tx.send(MpvMessage::StartFile);
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::StartFile);
                    let pause = self.get_property::<bool>("pause").unwrap_or(false);
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Pause(pause));
                }
                Event::GetPropertyReply { reply_userdata, .. } => {
                    let _ = MPV_CTRL.tx.send(MpvMessage::FenceReply(reply_userdata));
                }
                Event::Shutdown => {
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Shutdown);
                    let _ = MPV_CTRL.tx.send(MpvMessage::Shutdown);
                    return false;
                }
                Event::Deprecated(event)
                    if event.event_id == libmpv2_sys::mpv_event_id_MPV_EVENT_IDLE =>
                {
                    let _ = MPV_CTRL.tx.send(MpvMessage::Idle);
                }
                _ => {}
            },
            Err(error) => {
                let libmpv2::Error::Raw(code) = error else {
                    return true;
                };

                // libmpv2 converts END_FILE with a non-zero media error into
                // Err instead of Event::EndFile. The only async request here
                // reads the always-available idle-active property, so an
                // active-file Raw error is the media termination needed by the
                // stop/idle barrier.
                if self.has_file.replace(false) {
                    let _ = MPV_CTRL.tx.send(MpvMessage::EndFile);
                }

                let message = MutsumiMpvError::from_code(code).to_string();
                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Error(message));
            }
        }

        true
    }

    fn get_property_value(
        &self, property: &str, value_type: MpvValueType,
    ) -> libmpv2::Result<MpvValue> {
        match value_type {
            MpvValueType::Bool => self.get_property::<bool>(property).map(MpvValue::Bool),
            MpvValueType::I64 => self.get_property::<i64>(property).map(MpvValue::I64),
            MpvValueType::F64 => self.get_property::<f64>(property).map(MpvValue::F64),
            MpvValueType::String => self.get_property::<String>(property).map(MpvValue::String),
        }
    }
}

fn node_to_chapter_list(value: &str) -> ChapterList {
    let mut chapters = Vec::new();

    let Ok(json) = serde_json::from_str::<Value>(value) else {
        return ChapterList(chapters);
    };
    let Some(array) = json.as_array() else {
        return ChapterList(chapters);
    };

    for node in array {
        let Some(obj) = node.as_object() else {
            continue;
        };

        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let time = obj
            .get("time")
            .and_then(Value::as_f64)
            .or_else(|| obj.get("time").and_then(Value::as_i64).map(|v| v as f64))
            .unwrap_or(0.0);

        chapters.push(Chapter { title, time });
    }

    ChapterList(chapters)
}

pub struct ChapterList(pub Vec<Chapter>);

impl IntoIterator for ChapterList {
    type Item = Chapter;
    type IntoIter = std::vec::IntoIter<Chapter>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

pub struct Chapter {
    pub title: String,
    pub time: f64,
}

#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub filename: String,
    pub title: String,
    pub current: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Playlist(pub Vec<PlaylistEntry>);

impl IntoIterator for Playlist {
    type Item = PlaylistEntry;
    type IntoIter = std::vec::IntoIter<PlaylistEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

fn node_to_playlist(value: &str) -> Playlist {
    let mut entries = Vec::new();

    let Ok(json) = serde_json::from_str::<Value>(value) else {
        return Playlist(entries);
    };
    let Some(array) = json.as_array() else {
        return Playlist(entries);
    };

    for node in array {
        let Some(obj) = node.as_object() else {
            continue;
        };

        let filename = obj
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let current = obj.get("current").and_then(Value::as_bool).unwrap_or(false);

        entries.push(PlaylistEntry {
            filename,
            title,
            current,
        });
    }

    Playlist(entries)
}

#[derive(Debug)]
pub struct MpvTrack {
    pub id: i64,
    pub title: String,
    pub lang: String,
    pub type_: String,
}

#[derive(Debug)]
pub struct DanmakuTrack {
    pub external_url: String,
}

pub struct MpvTracks {
    pub audio_tracks: Vec<MpvTrack>,
    pub sub_tracks: Vec<MpvTrack>,
    pub danmaku_track: Option<DanmakuTrack>,
}

fn node_to_tracks(value: &str) -> MpvTracks {
    let mut audio_tracks = Vec::new();
    let mut sub_tracks = Vec::new();
    let mut danmaku_track = None;

    let Ok(json) = serde_json::from_str::<Value>(value) else {
        return MpvTracks {
            audio_tracks,
            sub_tracks,
            danmaku_track,
        };
    };
    let Some(array) = json.as_array() else {
        return MpvTracks {
            audio_tracks,
            sub_tracks,
            danmaku_track,
        };
    };

    for node in array {
        let Some(obj) = node.as_object() else {
            continue;
        };

        let id = obj.get("id").and_then(Value::as_i64).unwrap_or(0);
        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let lang = obj
            .get("lang")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let type_ = obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        if type_ == "sub" && lang == "danmaku" {
            let external = obj
                .get("external")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if !external {
                continue;
            }

            let external_filename = obj
                .get("external-filename")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let Some(external) =
                external_filename.strip_prefix("edl://!no_clip;!delay_open,media_type=sub;%44%")
            else {
                continue;
            };

            danmaku_track = Some(DanmakuTrack {
                external_url: external.to_string(),
            });

            continue;
        }

        let track = MpvTrack {
            id,
            title,
            lang,
            type_,
        };

        if track.type_ == "audio" {
            audio_tracks.push(track);
        } else if track.type_ == "sub" {
            sub_tracks.push(track);
        }
    }

    MpvTracks {
        audio_tracks,
        sub_tracks,
        danmaku_track,
    }
}
