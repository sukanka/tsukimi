// IPC backend: spawns mpv as a subprocess and talks to it over a Unix
// domain socket. The whole thing is unix-only (UnixStream, prctl); on
// Windows we expose a non-functional stub so mpvglarea.rs compiles
// without #[cfg]-fork at every call site, and is_ipc() in MPVGLArea
// guarantees the stub paths are never taken at runtime.

#[cfg(not(unix))]
mod stub {
    #[derive(Default)]
    pub struct MpvIpcClient;

    impl MpvIpcClient {
        pub fn new() -> Self {
            Self
        }
        pub fn play(&self, _url: &str, _percentage: f64, _vo: &str) {}
        pub fn stop(&self) {}
        pub fn command(&self, _cmd: &str, _args: &[&str]) {}
        pub fn set_property_string(&self, _name: &str, _value: &str) {}
        pub fn set_property_f64(&self, _name: &str, _value: f64) {}
        pub fn set_property_i64(&self, _name: &str, _value: i64) {}
        pub fn position(&self) -> f64 {
            0.0
        }
        pub fn get_track_id(&self, _type_: &str) -> i64 {
            0
        }
        pub fn clear_danmaku_overlay(&self) {}
    }
}

#[cfg(not(unix))]
pub use stub::MpvIpcClient;

#[cfg(unix)]
use std::{
    cell::RefCell,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    process::{Child, Command},
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use super::tsukimi_mpv::{
    Chapter, ChapterList, ListenEvent, MPV_EVENT_CHANNEL, MpvTrack, MpvTracks,
};

#[cfg(unix)]
pub struct MpvIpcClient {
    child: RefCell<Option<Child>>,
    socket_path: String,
    event_handle: RefCell<Option<JoinHandle<()>>>,
    overlay_handle: RefCell<Option<JoinHandle<()>>>,
    overlay_alive: Arc<AtomicBool>,
    /// Long-lived blocking command writer (with short timeouts) wrapped in
    /// a BufReader so the overlay tick can read mpv's ack reply line by
    /// line and pace its sending against mpv's actual processing speed.
    /// This is what prevents queued backlogs from bursting out as scroll
    /// jitter under main-thread spikes.
    writer: Arc<Mutex<Option<BufReader<UnixStream>>>>,
    /// Cached latest playback position (seconds), for position() queries
    pub last_time_pos: Arc<Mutex<f64>>,
    /// Wall-clock instant when last_time_pos was last refreshed; used by
    /// the overlay tick thread to extrapolate time between updates.
    pub last_instant: Arc<Mutex<Instant>>,
    /// Cached pause state, observed via property-change.
    pub paused: Arc<AtomicBool>,
    /// Cached playback speed, observed via property-change.
    pub speed: Arc<Mutex<f64>>,
    /// Cached latest track info, for get_track_id() queries
    pub last_tracks: Arc<Mutex<Option<MpvTracks>>>,
}

#[cfg(unix)]
impl MpvIpcClient {
    pub fn new() -> Self {
        let socket_path = format!("/tmp/tsukimi-mpv-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&socket_path);

        Self {
            child: RefCell::new(None),
            socket_path,
            event_handle: RefCell::new(None),
            overlay_handle: RefCell::new(None),
            overlay_alive: Arc::new(AtomicBool::new(false)),
            writer: Arc::new(Mutex::new(None)),
            last_time_pos: Arc::new(Mutex::new(0.0)),
            last_instant: Arc::new(Mutex::new(Instant::now())),
            paused: Arc::new(AtomicBool::new(false)),
            speed: Arc::new(Mutex::new(1.0)),
            last_tracks: Arc::new(Mutex::new(None)),
        }
    }

    /// Spawn mpv subprocess, connect IPC socket, start event listener
    pub fn play(&self, url: &str, percentage: f64, vo: &str) {
        self.stop();

        let _ = std::fs::remove_file(&self.socket_path);

        let mut cmd = Command::new("mpv");
        cmd.arg(format!("--input-ipc-server={}", self.socket_path))
            .arg(format!("--vo={}", vo))
            .arg("--keep-open=always")
            .arg("--input-vo-keyboard=yes")
            .arg("--input-default-bindings=yes")
            .arg(format!(
                "--volume={}",
                crate::ui::models::SETTINGS.mpv_default_volume()
            ))
            .arg(format!(
                "--sub-font-size={}",
                crate::ui::models::SETTINGS.mpv_subtitle_size()
            ))
            .arg(format!(
                "--cache-secs={}",
                crate::ui::models::SETTINGS.mpv_cache_time()
            ))
            .arg("--hwdec=auto-safe");

        if percentage > 0.0 {
            cmd.arg(format!("--start={}%", percentage as u32));
        }

        cmd.arg(url);

        // Linux: have mpv receive SIGTERM if tsukimi dies, so closing the
        // app (or a crash) doesn't leave the mpv window playing on.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                // SIGTERM = 15
                let r = libc::prctl(libc::PR_SET_PDEATHSIG, 15);
                if r != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().expect("Failed to spawn mpv");
        self.child.replace(Some(child));

        let socket_path = self.socket_path.clone();
        let last_time_pos = Arc::clone(&self.last_time_pos);
        let last_tracks = Arc::clone(&self.last_tracks);
        let last_instant = Arc::clone(&self.last_instant);
        let paused = Arc::clone(&self.paused);
        let speed = Arc::clone(&self.speed);
        let overlay_alive_event = Arc::clone(&self.overlay_alive);
        let event_handle = std::thread::Builder::new()
            .name("mpv-ipc-event".into())
            .spawn(move || {
                let mut retries = 0;
                let stream = loop {
                    if let Ok(s) = UnixStream::connect(&socket_path) {
                        break s;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    retries += 1;
                    if retries >= 100 {
                        let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Shutdown);
                        return;
                    }
                };

                // Observe properties
                let observe_props: &[&str] = &[
                    "time-pos",
                    "pause",
                    "duration",
                    "cache-speed",
                    "track-list",
                    "paused-for-cache",
                    "demuxer-cache-time",
                    "volume",
                    "chapter-list",
                    "speed",
                ];
                {
                    let mut s = stream.try_clone().unwrap();
                    for (i, prop) in observe_props.iter().enumerate() {
                        let msg = serde_json::json!({
                            "command": ["observe_property", i, prop],
                        });
                        let _ =
                            writeln!(s, "{}", serde_json::to_string(&msg).unwrap());
                    }
                }

                let reader = BufReader::new(stream);
                let mut shutdown_sent = false;
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let Ok(event) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if let Some(event_name) =
                        event.get("event").and_then(|v| v.as_str())
                    {
                        match event_name {
                            "shutdown" => {
                                // Stop overlay tick before mpv tears the socket
                                // down so it can't block on a dying writer.
                                overlay_alive_event.store(false, Ordering::SeqCst);
                                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Shutdown);
                                shutdown_sent = true;
                                break;
                            }
                            "start-file" => {
                                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::StartFile);
                            }
                            "end-file" => {
                                let reason = event
                                    .get("reason")
                                    .and_then(|v| v.as_str())
                                    .map(|r| match r {
                                        "eof" => 0u32,
                                        "stop" => 2u32,
                                        "quit" => 3u32,
                                        _ => 1u32,
                                    })
                                    .unwrap_or(0);
                                let _ =
                                    MPV_EVENT_CHANNEL.tx.send(ListenEvent::Eof(reason));
                            }
                            "seek" => {
                                let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Seek);
                            }
                            "playback-restart" => {
                                let _ = MPV_EVENT_CHANNEL
                                    .tx
                                    .send(ListenEvent::PlaybackRestart);
                            }
                            "property-change" => {
                                let prop_name = event
                                    .get("name")
                                    .and_then(|v| v.as_str());
                                let Some(prop_name) = prop_name else { continue };
                                let Some(data) = event.get("data") else { continue };
                                match prop_name {
                                    "time-pos" => {
                                        if let Some(t) = data.as_f64() {
                                            // Don't overwrite valid position with 0 during shutdown
                                            let is_zero = *last_time_pos.lock().unwrap() == 0.0;
                                            if t > 0.0 || is_zero {
                                                *last_time_pos.lock().unwrap() = t;
                                                *last_instant.lock().unwrap() = Instant::now();
                                            }
                                            let _ = MPV_EVENT_CHANNEL.tx.send(
                                                ListenEvent::TimePos(t),
                                            );
                                        }
                                    }
                                    "pause" => {
                                        if let Some(p) = data.as_bool() {
                                            paused.store(p, Ordering::SeqCst);
                                            // When unpausing, refresh the wall-clock anchor so
                                            // overlay extrapolation doesn't jump.
                                            if !p {
                                                *last_instant.lock().unwrap() = Instant::now();
                                            }
                                            let _ = MPV_EVENT_CHANNEL
                                                .tx
                                                .send(ListenEvent::Pause(p));
                                        }
                                    }
                                    "duration" => {
                                        if let Some(d) = data.as_f64() {
                                            let _ = MPV_EVENT_CHANNEL
                                                .tx
                                                .send(ListenEvent::Duration(d));
                                        }
                                    }
                                    "cache-speed" => {
                                        if let Some(s) = data.as_i64() {
                                            let _ = MPV_EVENT_CHANNEL
                                                .tx
                                                .send(ListenEvent::CacheSpeed(s));
                                        }
                                    }
                                    "track-list" => {
                                        let tracks = parse_track_list(data);
                                        *last_tracks.lock().unwrap() = Some(tracks.clone());
                                        let _ = MPV_EVENT_CHANNEL
                                            .tx
                                            .send(ListenEvent::TrackList(tracks));
                                    }
                                    "paused-for-cache" => {
                                        if let Some(p) = data.as_bool() {
                                            let _ = MPV_EVENT_CHANNEL.tx.send(
                                                ListenEvent::PausedForCache(p),
                                            );
                                        }
                                    }
                                    "demuxer-cache-time" => {
                                        if let Some(t) = data.as_f64() {
                                            let _ = MPV_EVENT_CHANNEL.tx.send(
                                                ListenEvent::DemuxerCacheTime(t as i64),
                                            );
                                        }
                                    }
                                    "volume" => {
                                        if let Some(v) = data.as_f64() {
                                            let _ = MPV_EVENT_CHANNEL
                                                .tx
                                                .send(ListenEvent::Volume(v as i64));
                                        }
                                    }
                                    "chapter-list" => {
                                        let chapters = parse_chapter_list(data);
                                        let _ = MPV_EVENT_CHANNEL.tx.send(
                                            ListenEvent::ChapterList(chapters),
                                        );
                                    }
                                    "speed" => {
                                        if let Some(s) = data.as_f64() {
                                            *speed.lock().unwrap() = s;
                                            let _ = MPV_EVENT_CHANNEL
                                                .tx
                                                .send(ListenEvent::Speed(s));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Event loop ended = mpv exited. Make sure the overlay tick
                // can't keep poking a dead socket.
                overlay_alive_event.store(false, Ordering::SeqCst);
                if !shutdown_sent {
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Shutdown);
                }
            })
            .expect("Failed to spawn mpv IPC event thread");

        self.event_handle.replace(Some(event_handle));

        // ~60Hz overlay tick with reply pacing: send command, wait for
        // mpv's ack, then sleep up to a frame period before sending the
        // next. mpv-side load determines the natural cadence — when
        // it's busy (key event, menu redraw, heavy frame) the next
        // tick is delayed to match, so commands never queue up and we
        // never see a backlog burst out as scroll jitter.
        self.overlay_alive.store(true, Ordering::SeqCst);
        let alive = Arc::clone(&self.overlay_alive);
        let last_time_pos = Arc::clone(&self.last_time_pos);
        let last_instant = Arc::clone(&self.last_instant);
        let paused = Arc::clone(&self.paused);
        let speed = Arc::clone(&self.speed);
        let writer_slot = Arc::clone(&self.writer);
        let socket_path = self.socket_path.clone();
        let overlay_handle = std::thread::Builder::new()
            .name("mpv-ipc-overlay".into())
            .spawn(move || {
                let frame = Duration::from_millis(16);
                let mut request_id: u64 = 1;
                let mut last_sent = Instant::now() - frame;
                while alive.load(Ordering::SeqCst) {
                    // Sleep up to one frame, but no longer — if mpv was
                    // slow this iteration, we already paid that wait.
                    let now = Instant::now();
                    let sleep_for = frame.saturating_sub(now.duration_since(last_sent));
                    if !sleep_for.is_zero() {
                        std::thread::sleep(sleep_for);
                    }
                    if !alive.load(Ordering::SeqCst) {
                        break;
                    }

                    let state_guard =
                        super::danmaku_ass::OVERLAY_STATE.read().unwrap();
                    let Some((events, config)) = state_guard.as_ref() else {
                        last_sent = Instant::now();
                        continue;
                    };
                    if paused.load(Ordering::SeqCst) {
                        last_sent = Instant::now();
                        continue;
                    }
                    let base_time = *last_time_pos.lock().unwrap();
                    let elapsed = last_instant.lock().unwrap().elapsed().as_secs_f64();
                    let cur_speed = *speed.lock().unwrap();
                    let t = base_time + elapsed * cur_speed;
                    let data = super::danmaku_ass::render_overlay_data(
                        t * 1000.0,
                        events,
                        config,
                    );
                    drop(state_guard);

                    request_id = request_id.wrapping_add(1);
                    let payload = serde_json::json!({
                        "command": ["osd-overlay", 0, "ass-events", data, 1920, 1080],
                        "request_id": request_id,
                    });
                    let Ok(json) = serde_json::to_string(&payload) else {
                        last_sent = Instant::now();
                        continue;
                    };

                    let mut guard = writer_slot.lock().unwrap();
                    if guard.is_none() && alive.load(Ordering::SeqCst)
                        && let Ok(s) = UnixStream::connect(&socket_path)
                    {
                        // Blocking with short timeouts: writes block briefly
                        // on backpressure (preferred over WouldBlock since
                        // we want the tick paced by mpv), reads block until
                        // ack arrives or 100ms timeout.
                        let _ = s.set_write_timeout(Some(Duration::from_millis(100)));
                        let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                        *guard = Some(BufReader::new(s));
                    }
                    let Some(reader) = guard.as_mut() else {
                        last_sent = Instant::now();
                        continue;
                    };

                    if writeln!(reader.get_mut(), "{json}").is_err() {
                        *guard = None;
                        last_sent = Instant::now();
                        continue;
                    }

                    // Wait for matching ack. mpv may push events
                    // (property-change etc.) on this socket too, so
                    // skip lines without the matching request_id.
                    // Capped at ~3 lines so we don't starve other ticks.
                    for _ in 0..3 {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => {
                                *guard = None;
                                break;
                            }
                            Ok(_) => {
                                let matched = serde_json::from_str::<Value>(&line)
                                    .ok()
                                    .and_then(|v| v.get("request_id")?.as_u64())
                                    == Some(request_id);
                                if matched {
                                    break;
                                }
                            }
                        }
                    }
                    last_sent = Instant::now();
                }
            })
            .expect("Failed to spawn mpv IPC overlay thread");

        self.overlay_handle.replace(Some(overlay_handle));
    }

    /// Kill mpv subprocess, clean up socket
    pub fn stop(&self) {
        self.overlay_alive.store(false, Ordering::SeqCst);
        if let Some(handle) = self.overlay_handle.borrow_mut().take() {
            let _ = handle.join();
        }
        if let Some(mut child) = self.child.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.event_handle.borrow_mut().take() {
            let _ = handle.join();
        }
        *self.writer.lock().unwrap() = None;
        let _ = std::fs::remove_file(&self.socket_path);
    }

    /// Remove the danmaku overlay (mpv osd-overlay slot 0).
    pub fn clear_danmaku_overlay(&self) {
        // mpv's osd-overlay format must be "none" or "ass-events";
        // empty data + format=none clears the slot.
        self.command("osd-overlay", &["0", "none", ""]);
    }

    /// Send a raw JSON IPC message via socket. One-shot connect: closing
    /// the socket makes mpv discard the unread reply, so we can't build
    /// up a backlog that would eventually stall mpv's input thread.
    fn send_raw(&self, msg: &str) {
        if let Ok(mut stream) = UnixStream::connect(&self.socket_path) {
            let _ = writeln!(stream, "{msg}");
        }
    }

    fn send_json(&self, msg: &Value) {
        if let Ok(json) = serde_json::to_string(msg) {
            self.send_raw(&json);
        }
    }

    pub fn command(&self, cmd: &str, args: &[&str]) {
        let args: Vec<Value> = std::iter::once(Value::String(cmd.to_string()))
            .chain(args.iter().map(|a| Value::String(a.to_string())))
            .collect();
        let msg = serde_json::json!({ "command": args });
        self.send_json(&msg);
    }

    pub fn set_property_string(&self, name: &str, value: &str) {
        let msg = serde_json::json!({
            "command": ["set_property", name, value],
        });
        self.send_json(&msg);
    }

    pub fn set_property_bool(&self, name: &str, value: bool) {
        let msg = serde_json::json!({
            "command": ["set_property", name, Value::Bool(value)],
        });
        self.send_json(&msg);
    }

    pub fn set_property_f64(&self, name: &str, value: f64) {
        let msg = serde_json::json!({
            "command": ["set_property", name, value],
        });
        self.send_json(&msg);
    }

    pub fn set_property_i64(&self, name: &str, value: i64) {
        let msg = serde_json::json!({
            "command": ["set_property", name, value],
        });
        self.send_json(&msg);
    }

    /// Get current track id (from cached track list)
    pub fn get_track_id(&self, type_: &str) -> i64 {
        self.last_tracks
            .lock()
            .unwrap()
            .as_ref()
            .map(|tracks| {
                let list = if type_ == "aid" {
                    &tracks.audio_tracks
                } else {
                    &tracks.sub_tracks
                };
                list.first().map(|t| t.id).unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// Get current playback position (from cache)
    pub fn position(&self) -> f64 {
        *self.last_time_pos.lock().unwrap()
    }
}

#[cfg(unix)]
impl Drop for MpvIpcClient {
    fn drop(&mut self) {
        self.overlay_alive.store(false, Ordering::SeqCst);
        if let Some(handle) = self.overlay_handle.borrow_mut().take() {
            let _ = handle.join();
        }
        if let Some(mut child) = self.child.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.event_handle.borrow_mut().take() {
            let _ = handle.join();
        }
        *self.writer.lock().unwrap() = None;
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
fn parse_track_list(data: &Value) -> MpvTracks {
    let mut audio_tracks = Vec::new();
    let mut sub_tracks = Vec::new();
    if let Some(arr) = data.as_array() {
        for node in arr {
            let id = node.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let title = node
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let lang = node
                .get("lang")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let type_ = node
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let track = MpvTrack {
                id,
                title,
                lang,
                type_: type_.clone(),
            };
            if type_ == "audio" {
                audio_tracks.push(track);
            } else if type_ == "sub" {
                sub_tracks.push(track);
            }
        }
    }
    MpvTracks { audio_tracks, sub_tracks }
}

#[cfg(unix)]
fn parse_chapter_list(data: &Value) -> ChapterList {
    let mut chapters = Vec::new();
    if let Some(arr) = data.as_array() {
        for node in arr {
            let title = node
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let time = node.get("time").and_then(|v| v.as_f64()).unwrap_or(0.0);
            chapters.push(Chapter { title, time });
        }
    }
    ChapterList(chapters)
}
