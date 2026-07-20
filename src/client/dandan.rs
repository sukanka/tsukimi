use dandanapi::CommentData;
use mutsumi::{
    Color,
    Danmaku,
    DanmakuMode,
};

pub const X_APPID: &str = "e9imrhcexn";
pub const SECRET_KEY: &str = include_str!("../../secret/key");

pub trait DanmakuConvert {
    fn into_danmaku(self) -> Option<Danmaku>;
}

impl DanmakuConvert for CommentData {
    fn into_danmaku(self) -> Option<Danmaku> {
        let content = self.m?;
        let mut parts = self.p.as_deref().unwrap_or_default().split(',');
        let start = parts
            .next()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or_default();
        let mode = parts
            .next()
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or_default();
        let color = parts
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0x00ff_ffff);

        Some(Danmaku {
            content,
            start: start * 1000.0,
            color: Color {
                r: ((color >> 16) & 0xff) as u8,
                g: ((color >> 8) & 0xff) as u8,
                b: (color & 0xff) as u8,
                a: 255,
            },
            mode: match mode {
                2 => DanmakuMode::TopCenter,
                3 => DanmakuMode::BottomCenter,
                _ => DanmakuMode::Scroll,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(params: Option<&str>, content: Option<&str>) -> CommentData {
        CommentData {
            cid: 1,
            p: params.map(str::to_owned),
            m: content.map(str::to_owned),
        }
    }

    #[test]
    fn converts_dandanplay_parameters() {
        let danmaku = comment(Some("1.5,2,16711680"), Some("hello"))
            .into_danmaku()
            .expect("valid comment");

        assert_eq!(danmaku.content, "hello");
        assert_eq!(danmaku.start, 1500.0);
        assert_eq!(danmaku.mode, DanmakuMode::TopCenter);
        assert_eq!(
            (danmaku.color.r, danmaku.color.g, danmaku.color.b),
            (255, 0, 0)
        );
    }

    #[test]
    fn ignores_comments_without_content() {
        assert!(comment(Some("1,1,0"), None).into_danmaku().is_none());
    }

    #[test]
    fn uses_safe_defaults_for_malformed_parameters() {
        let danmaku = comment(Some("invalid"), Some("hello"))
            .into_danmaku()
            .expect("comment content is present");

        assert_eq!(danmaku.start, 0.0);
        assert_eq!(danmaku.mode, DanmakuMode::Scroll);
        assert_eq!(
            (danmaku.color.r, danmaku.color.g, danmaku.color.b),
            (255, 255, 255)
        );
    }
}
