use dandanapi::CommentData;
use mutsumi::{
    Color,
    Danmaku,
    DanmakuMode,
};
use serde::{
    Deserialize,
    Serialize,
};

pub const DEFAULT_DANMAKU_SERVER_LABEL: &str = "Default (api.dandanplay.net)";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DanmakuServer {
    pub name: String,
    pub url: String,
}

pub fn danmaku_combo_to_server_index(selected: u32) -> i32 {
    selected as i32 - 1
}

pub fn danmaku_server_to_combo_index(server_index: i32) -> u32 {
    (server_index + 1) as u32
}

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
        let Some(content) = self.m else {
            return Danmaku {
                content: String::new(),
                start: 0.0,
                color: Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                },
                mode: DanmakuMode::Scroll,
            };
        };

        let Some(params) = self.p else {
            return Danmaku {
                content,
                start: 0.0,
                color: Color::default(),
                mode: DanmakuMode::Scroll,
            };
        };

        let parts: Vec<&str> = params.split(',').collect();
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
            .unwrap_or(0x00ff_ffff);

        Danmaku {
            content,
            start: start * 1000.0,
            color: Color {
                r: ((color >> 16) & 0xff) as u8,
                g: ((color >> 8) & 0xff) as u8,
                b: (color & 0xff) as u8,
                a: 255,
            },
            mode: match mode {
                1 => DanmakuMode::Scroll,
                2 => DanmakuMode::TopCenter,
                3 => DanmakuMode::BottomCenter,
                _ => DanmakuMode::Scroll,
            },
        }
    }
}
