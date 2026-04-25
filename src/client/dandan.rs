use dandanapi::CommentData;
use danmakw::Danmaku;
use serde::{Deserialize, Serialize};

pub const X_APPID: &str = "e9imrhcexn";
pub const SECRETE_KEY: &str = include_str!("../../secret/key");

pub const DEFAULT_DANMAKU_SERVER_LABEL: &str = "Default (api.dandanplay.net)";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DanmakuServer {
    pub name: String,
    pub url: String,
}

/// Convert combo row selected index (0-based) to server index (-1 = default).
pub fn danmaku_combo_to_server_index(selected: u32) -> i32 {
    selected as i32 - 1
}

/// Convert server index (-1 = default) to combo row selected index (0-based).
pub fn danmaku_server_to_combo_index(server_index: i32) -> u32 {
    (server_index + 1) as u32
}

/// Apply the active danmaku server's base URI to the global dandanapi client.
pub fn apply_danmaku_active_server(active: i32, servers: &[DanmakuServer]) {
    if active >= 0 && (active as usize) < servers.len() {
        let _ = dandanapi::set_base_uri(&servers[active as usize].url);
    } else {
        let _ = dandanapi::set_base_uri("");
    }
}

pub trait DanmakuConvert {
    fn into_danmaku(self) -> Danmaku;
}

impl DanmakuConvert for CommentData {
    fn into_danmaku(self) -> Danmaku {
        let Some(m) = self.m else {
            return Danmaku {
                content: String::new(),
                start: 0.0,
                color: danmakw::Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                },
                mode: danmakw::DanmakuMode::Scroll,
            };
        };

        let Some(p) = self.p else {
            return Danmaku {
                content: m,
                start: 0.0,
                color: danmakw::Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
                mode: danmakw::DanmakuMode::Scroll,
            };
        };

        let parts: Vec<&str> = p.split(',').collect();
        let start = parts
            .first()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or_default();
        let mode = parts
            .get(1)
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or_default();
        let color = parts
            .get(2)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_default();

        Danmaku {
            content: m,
            start: start * 1000.0,
            color: danmakw::Color {
                r: ((color >> 16) & 0xFF) as u8,
                g: ((color >> 8) & 0xFF) as u8,
                b: (color & 0xFF) as u8,
                a: 255,
            },
            mode: match mode {
                1 => danmakw::DanmakuMode::Scroll,
                2 => danmakw::DanmakuMode::TopCenter,
                3 => danmakw::DanmakuMode::BottomCenter,
                _ => danmakw::DanmakuMode::Scroll,
            },
        }
    }
}
