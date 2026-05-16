use std::{
    cell::RefCell,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    process::{Child, Command},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

use serde_json::Value;
use super::tsukimi_mpv::{
    Chapter, ChapterList, ListenEvent, MPV_EVENT_CHANNEL, MpvTrack, MpvTracks,
};

pub struct MpvIpcClient {
    child: RefCell<Option<Child>>,
    socket_path: String,
    event_handle: RefCell<Option<JoinHandle<()>>>,
    /// Cached latest playback position (seconds), for position() queries
    pub last_time_pos: Arc<Mutex<f64>>,
    /// Cached latest track info, for get_track_id() queries
    pub last_tracks: Arc<Mutex<Option<MpvTracks>>>,
}

impl MpvIpcClient {
    pub fn new() -> Self {
        let socket_path = format!("/tmp/tsukimi-mpv-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&socket_path);

        Self {
            child: RefCell::new(None),
            socket_path,
            event_handle: RefCell::new(None),
            last_time_pos: Arc::new(Mutex::new(0.0)),
            last_tracks: Arc::new(Mutex::new(None)),
        }
    }

    /// Spawn mpv subprocess, connect IPC socket, start event listener
    pub fn play(&self, url: &str, percentage: f64) {
        self.stop();

        let _ = std::fs::remove_file(&self.socket_path);

        let mut cmd = Command::new("mpv");
        cmd.arg(format!("--input-ipc-server={}", self.socket_path))
            .arg("--vo=gpu-next")
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

        let child = cmd.spawn().expect("Failed to spawn mpv");
        self.child.replace(Some(child));

        let socket_path = self.socket_path.clone();
        let last_time_pos = Arc::clone(&self.last_time_pos);
        let last_tracks = Arc::clone(&self.last_tracks);
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
                                            }
                                            let _ = MPV_EVENT_CHANNEL.tx.send(
                                                ListenEvent::TimePos(t as i64),
                                            );
                                        }
                                    }
                                    "pause" => {
                                        if let Some(p) = data.as_bool() {
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
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Event loop ended = mpv exited
                if !shutdown_sent {
                    let _ = MPV_EVENT_CHANNEL.tx.send(ListenEvent::Shutdown);
                }
            })
            .expect("Failed to spawn mpv IPC event thread");

        self.event_handle.replace(Some(event_handle));
    }

    /// Kill mpv subprocess, clean up socket
    pub fn stop(&self) {
        if let Some(mut child) = self.child.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.event_handle.borrow_mut().take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }

    /// Send a raw JSON IPC message via socket
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

impl Drop for MpvIpcClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.event_handle.borrow_mut().take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

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
