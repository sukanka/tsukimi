use gtk::gdk;
use once_cell::sync::Lazy;
use tokio::sync::watch;

/// Wayland image-description values, kept independent of GTK for the proxy thread.
/// Named values use the color-management protocol's enums, not CICP code points.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ColorDescription {
    pub primaries_named: Option<u32>,
    pub primaries: Option<[i32; 8]>,
    pub tf_named: Option<u32>,
    pub tf_power: Option<u32>,
    pub luminances: Option<(u32, u32, u32)>,
    pub target_primaries: Option<[i32; 8]>,
    pub target_luminance: Option<(u32, u32)>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

impl ColorDescription {
    /// The protocol's ordinary sRGB encoding, including its default luminances.
    pub(crate) fn srgb() -> Self {
        let primaries = [
            640000, 330000, 300000, 600000, 150000, 60000, 312700, 329000,
        ];
        Self {
            primaries_named: Some(1),
            primaries: Some(primaries),
            tf_named: Some(9),
            luminances: Some((2000, 80, 80)),
            target_primaries: Some(primaries),
            target_luminance: Some((2000, 80)),
            ..Self::default()
        }
    }

    /// Validate the encodings our GDK texture bridge can represent.
    /// This part is pure Rust and may also be called by the proxy thread.
    pub(crate) fn cicp(&self) -> Result<(u32, u32), String> {
        let primaries = match self.primaries_named {
            Some(1) => 1,  // sRGB / BT.709
            Some(3) => 5,  // PAL / BT.601 625
            Some(4) => 6,  // NTSC / BT.601 525
            Some(6) => 9,  // BT.2020
            Some(9) => 12, // Display P3
            value => return Err(format!("unsupported Wayland color primaries: {value:?}")),
        };
        let transfer = match self.tf_named {
            Some(2) => 4,   // gamma 2.2
            Some(3) => 5,   // gamma 2.8
            Some(5) => 8,   // linear
            Some(9) => 13,  // sRGB
            Some(11) => 16, // PQ
            Some(13) => 18, // HLG
            value => return Err(format!("unsupported Wayland transfer function: {value:?}")),
        };
        Ok((primaries, transfer))
    }

    /// Must run on the GTK thread. These are rendered RGB pixels, not source YUV.
    pub(crate) fn to_gdk_color_state(&self) -> Result<gdk::ColorState, String> {
        let (primaries, transfer) = self.cicp()?;
        let params = gdk::CicpParams::new();
        params.set_color_primaries(primaries);
        params.set_transfer_function(transfer);
        params.set_matrix_coefficients(0);
        params.set_range(gdk::CicpRange::Full);
        params
            .build_color_state()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn to_gdk_dmabuf_color_state(&self, fourcc: u32) -> Option<gdk::ColorState> {
        // GTK has no public fourcc classification API. These are the RGB
        // formats supported by GTK 4.24.1. YUV needs matrix/range information
        // that this bridge does not carry, so leave its GTK defaults intact.
        if matches!(
            &fourcc.to_le_bytes(),
            b"AR24"
                | b"BA24"
                | b"AB24"
                | b"RA24"
                | b"XR24"
                | b"BX24"
                | b"XB24"
                | b"RX24"
                | b"BG24"
                | b"RG24"
                | b"AB48"
                | b"AB4H"
                | b"R8  "
                | b"R16 "
                | b"AR30"
                | b"XR30"
                | b"AB30"
                | b"XB30"
        ) {
            self.to_gdk_color_state().ok()
        } else {
            None
        }
    }
}

/// The real GTK window's current preferred output; unknown displays start in SDR.
pub(crate) static PREFERRED_COLOR: Lazy<watch::Sender<ColorDescription>> =
    Lazy::new(|| watch::channel(ColorDescription::srgb()).0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_rgb_pq_and_preserves_yuv_defaults() {
        let pq = ColorDescription {
            primaries_named: Some(6),
            tf_named: Some(11),
            ..ColorDescription::default()
        };
        let texture_color = pq
            .to_gdk_dmabuf_color_state(u32::from_le_bytes(*b"AB30"))
            .unwrap();
        assert!(texture_color.equivalent(&gdk::ColorState::rec2100_pq()));
        assert!(
            pq.to_gdk_dmabuf_color_state(u32::from_le_bytes(*b"P010"))
                .is_none()
        );
    }
}
