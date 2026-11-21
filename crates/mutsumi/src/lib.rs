pub mod control;
pub mod video;

pub use control::*;
pub use danmakw::*;
pub use video::*;

pub fn force_gl_renderer() {
    unsafe {
        std::env::set_var("GSK_RENDERER", "gl");
    }
}

/// Enable GTK's Wayland color management before initializing GTK.
///
/// # Safety
/// Call before starting threads because this changes the process environment.
pub unsafe fn enable_color_management() {
    unsafe { enable_gdk_debug_flag("color-mgmt") };
}

/// Force GTK's HDR renderer for troubleshooting.
pub fn enable_gdk_hdr() {
    unsafe {
        enable_color_management();
        enable_gdk_debug_flag("hdr");
    }
}

unsafe fn enable_gdk_debug_flag(flag: &str) {
    let mut flags = std::env::var_os("GDK_DEBUG").unwrap_or_default();
    // GLib treats the tokens following `all` as exclusions.
    if flags
        .to_string_lossy()
        .split([':', ';', ',', ' ', '\t', '\n'])
        .any(|value| {
            [flag, "all", "help"]
                .iter()
                .any(|name| value.eq_ignore_ascii_case(name))
        })
    {
        return;
    }
    if !flags.is_empty() {
        flags.push(":");
    }
    flags.push(flag);
    unsafe { std::env::set_var("GDK_DEBUG", flags) };
}

pub fn init() {
    video::init();
    control::init();
    danmakw::init();
}
