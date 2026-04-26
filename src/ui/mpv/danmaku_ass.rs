use std::sync::RwLock;

use danmakw::{Color, Danmaku, DanmakuMode};
use once_cell::sync::Lazy;

const SCROLL_DURATION_MS: f64 = 25000.0;
const CENTER_DURATION_MS: f64 = 5000.0;
const PLAY_RES_X: u32 = 1920;
const PLAY_RES_Y: u32 = 1080;

type OverlayState = Lazy<RwLock<Option<(Vec<OverlayEvent>, AssDanmakuConfig)>>>;

pub static OVERLAY_STATE: OverlayState = Lazy::new(|| RwLock::new(None));

pub struct AssDanmakuConfig {
    pub font_name: String,
    pub font_size: u32,
    pub row_spacing: u32,
    pub max_scroll_lines: u32,
    pub max_top_lines: u32,
    pub max_bottom_lines: u32,
    pub top_padding: u32,
    pub speed_factor: f64,
    pub opacity: f64,
}

impl AssDanmakuConfig {
    pub fn line_height(&self) -> u32 {
        self.font_size + self.row_spacing
    }
}

pub struct OverlayEvent {
    pub start_ms: f64,
    pub end_ms: f64,
    pub x1: f64,
    pub y: f64,
    pub x2: f64,
    pub mode: DanmakuMode,
    pub color: Color,
    pub content: String,
}

pub fn prepare_overlay_events(
    danmaku: &[Danmaku],
    config: &AssDanmakuConfig,
) -> Vec<OverlayEvent> {
    let mut danmaku: Vec<Danmaku> = danmaku.to_vec();
    danmaku.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut scroll_rows: Vec<Option<f64>> = vec![None; config.max_scroll_lines as usize];
    let mut top_rows: Vec<Option<f64>> = vec![None; config.max_top_lines as usize];
    let mut bottom_rows: Vec<Option<f64>> = vec![None; config.max_bottom_lines as usize];

    let mut events = Vec::new();
    let line_height = config.line_height() as f64;
    let center_duration = CENTER_DURATION_MS;

    for d in &danmaku {
        match d.mode {
            DanmakuMode::Scroll => {
                let text_width = estimate_text_width(&d.content, config.font_size);
                let time_on_screen = SCROLL_DURATION_MS / config.speed_factor;
                let end_ms = d.start + time_on_screen;

                let row_idx = scroll_rows
                    .iter()
                    .position(|row| row.is_none_or(|free_time| free_time <= d.start));
                let Some(row_idx) = row_idx else { continue };
                scroll_rows[row_idx] = Some(end_ms);

                let y = config.top_padding as f64 + row_idx as f64 * line_height;

                events.push(OverlayEvent {
                    start_ms: d.start, end_ms,
                    x1: PLAY_RES_X as f64, y,
                    x2: -(text_width),
                    mode: DanmakuMode::Scroll,
                    color: d.color,
                    content: escape_ass_text(&d.content),
                });
            }
            DanmakuMode::TopCenter => {
                let row_idx = top_rows
                    .iter()
                    .position(|row| row.is_none_or(|free_time| free_time <= d.start));
                let Some(row_idx) = row_idx else { continue };
                top_rows[row_idx] = Some(d.start + center_duration);

                let y = config.top_padding as f64 + row_idx as f64 * line_height;

                events.push(OverlayEvent {
                    start_ms: d.start,
                    end_ms: d.start + center_duration,
                    x1: PLAY_RES_X as f64 / 2.0, y,
                    x2: 0.0,
                    mode: DanmakuMode::TopCenter,
                    color: d.color,
                    content: escape_ass_text(&d.content),
                });
            }
            DanmakuMode::BottomCenter => {
                let row_idx = bottom_rows
                    .iter()
                    .position(|row| row.is_none_or(|free_time| free_time <= d.start));
                let Some(row_idx) = row_idx else { continue };
                bottom_rows[row_idx] = Some(d.start + center_duration);

                let y = PLAY_RES_Y as f64
                    - config.top_padding as f64
                    - (row_idx + 1) as f64 * line_height;

                events.push(OverlayEvent {
                    start_ms: d.start,
                    end_ms: d.start + center_duration,
                    x1: PLAY_RES_X as f64 / 2.0, y,
                    x2: 0.0,
                    mode: DanmakuMode::BottomCenter,
                    color: d.color,
                    content: escape_ass_text(&d.content),
                });
            }
        }
    }

    events
}

pub fn render_overlay_data(
    time_ms: f64,
    events: &[OverlayEvent],
    config: &AssDanmakuConfig,
) -> String {
    let font = if config.font_name.is_empty() { "sans-serif" } else { &config.font_name };
    let opacity = (config.opacity * 255.0).round() as u8;

    let window_start = time_ms - SCROLL_DURATION_MS - 2000.0;
    let lo = events.partition_point(|e| e.start_ms < window_start);

    let mut parts = Vec::new();

    for event in &events[lo..] {
        if event.start_ms > time_ms { break; }
        if time_ms > event.end_ms { continue; }

        let pos = match event.mode {
            DanmakuMode::Scroll => {
                let p = ((time_ms - event.start_ms) / (event.end_ms - event.start_ms)).clamp(0.0, 1.0);
                let x = event.x1 + (event.x2 - event.x1) * p;
                format!("\\pos({:.0},{:.0})\\an8", x, event.y)
            }
            DanmakuMode::BottomCenter => {
                format!("\\pos({:.0},{:.0})\\an2", event.x1, event.y)
            }
            DanmakuMode::TopCenter => {
                format!("\\pos({:.0},{:.0})\\an8", event.x1, event.y)
            }
        };

        let Color { r, g, b, a: ca } = event.color;
        let combined_a = 255u8.saturating_sub(
            ((ca as f64 / 255.0) * opacity as f64).round() as u8,
        );

        parts.push(format!(
            "{{\\fn{font}\\fs{fs}\\c&H{b:02X}{g:02X}{r:02X}&\\alpha&H{aa:02X}&\\bord1\\3c&H000000&{pos}}}{content}",
            fs = config.font_size,
            b = b, g = g, r = r,
            aa = combined_a,
            pos = pos,
            content = event.content,
        ));
    }

    parts.join("\n")
}

fn estimate_text_width(text: &str, font_size: u32) -> f64 {
    let mut width = 0.0;
    for ch in text.chars() {
        width += if ch.is_ascii() { font_size as f64 * 0.6 } else { font_size as f64 * 1.0 };
    }
    width.max(1.0)
}

fn escape_ass_text(text: &str) -> String {
    text.replace('{', "\\{")
        .replace('}', "\\}")
        .replace('\n', "\\N")
        .replace('\r', "")
}
