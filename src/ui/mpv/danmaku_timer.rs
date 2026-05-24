use std::sync::Arc;

use libmpv2::Mpv;

#[derive(Clone)]
pub struct MpvTimer {
    pub mpv: Arc<Mpv>,
}

impl MpvTimer {
    pub fn new(mpv: Arc<Mpv>) -> Self {
        Self { mpv }
    }

    pub fn time_milis(&self) -> f64 {
        self.mpv
            .get_property::<f64>("time-pos")
            .unwrap_or_default()
            * 1000.0
    }
}
