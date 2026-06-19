use dandanapi::CommentData;
use mutsumi::{
    Color,
    Danmaku,
    DanmakuMode,
};

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
