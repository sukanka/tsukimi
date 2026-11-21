//! Color management for the virtual surfaces consumed by GTK.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{
        Rc,
        Weak,
    },
};

use wl_proxy::{
    object::{
        Object,
        ObjectCoreApi,
        ObjectUtils,
    },
    protocols::{
        color_management_v1::{
            wp_color_management_output_v1::{
                WpColorManagementOutputV1,
                WpColorManagementOutputV1Handler,
            },
            wp_color_management_surface_feedback_v1::{
                WpColorManagementSurfaceFeedbackV1,
                WpColorManagementSurfaceFeedbackV1Error,
                WpColorManagementSurfaceFeedbackV1Handler,
            },
            wp_color_management_surface_v1::{
                WpColorManagementSurfaceV1,
                WpColorManagementSurfaceV1Error,
                WpColorManagementSurfaceV1Handler,
            },
            wp_color_manager_v1::{
                WpColorManagerV1,
                WpColorManagerV1Error,
                WpColorManagerV1Handler,
                WpColorManagerV1Primaries,
                WpColorManagerV1RenderIntent,
                WpColorManagerV1TransferFunction,
            },
            wp_image_description_creator_icc_v1::{
                WpImageDescriptionCreatorIccV1,
                WpImageDescriptionCreatorIccV1Handler,
            },
            wp_image_description_creator_params_v1::{
                WpImageDescriptionCreatorParamsV1,
                WpImageDescriptionCreatorParamsV1Handler,
            },
            wp_image_description_info_v1::WpImageDescriptionInfoV1,
            wp_image_description_v1::{
                WpImageDescriptionV1,
                WpImageDescriptionV1Handler,
            },
        },
        wayland::{
            wl_output::WlOutput,
            wl_surface::WlSurface,
        },
    },
};

use super::{
    super::color::ColorDescription,
    SharedState,
};

struct SurfaceColor {
    has_color_object: bool,
    pending: Option<ColorDescription>,
    current: ColorDescription,
}

impl Default for SurfaceColor {
    fn default() -> Self {
        Self {
            has_color_object: false,
            pending: None,
            current: ColorDescription::srgb(),
        }
    }
}

pub(super) struct ColorManagementState {
    preferred: ColorDescription,
    preferred_identity: u32,
    next_identity: u32,
    upstream_identities: HashMap<u32, Weak<u32>>,
    surfaces: HashMap<u64, SurfaceColor>,
    feedback: HashMap<u64, (u64, Weak<WpColorManagementSurfaceFeedbackV1>)>,
}

impl ColorManagementState {
    pub(super) fn new(preferred: ColorDescription) -> Self {
        Self {
            preferred: complete_preferred(preferred),
            preferred_identity: 1,
            next_identity: 2,
            upstream_identities: HashMap::new(),
            surfaces: HashMap::new(),
            feedback: HashMap::new(),
        }
    }

    fn allocate_identity(&mut self) -> u32 {
        let identity = self.next_identity;
        self.next_identity = identity.wrapping_add(1).max(1);
        identity
    }

    fn upstream_identity(&mut self, upstream: u32) -> Rc<u32> {
        if let Some(identity) = self
            .upstream_identities
            .get(&upstream)
            .and_then(Weak::upgrade)
        {
            return identity;
        }

        // Local and forwarded descriptions share one downstream identity namespace.
        // Keep an upstream identity mapped for as long as a live description uses it.
        self.upstream_identities
            .retain(|_, identity| identity.strong_count() > 0);
        let identity = Rc::new(self.allocate_identity());
        self.upstream_identities
            .insert(upstream, Rc::downgrade(&identity));
        identity
    }

    pub(super) fn set_preferred(&mut self, preferred: ColorDescription) {
        let preferred = complete_preferred(preferred);
        if self.preferred == preferred {
            return;
        }
        self.preferred = preferred;
        self.preferred_identity = self.allocate_identity();
        let identity = self.preferred_identity;
        self.feedback.retain(|_, (_, feedback)| {
            if let Some(feedback) = feedback.upgrade() {
                feedback.send_preferred_changed(identity);
                true
            } else {
                false
            }
        });
    }

    pub(super) fn commit(&mut self, surface_id: u64) -> (ColorDescription, bool) {
        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            return (ColorDescription::srgb(), false);
        };
        let mut changed = false;
        if let Some(pending) = surface.pending.take() {
            changed = surface.current != pending;
            surface.current = pending;
        }
        (surface.current.clone(), changed)
    }

    pub(super) fn remove_surface(&mut self, surface_id: u64) {
        self.surfaces.remove(&surface_id);
        self.feedback.retain(|_, (id, _)| *id != surface_id);
    }
}

pub(super) struct ColorManagerHandler {
    pub(super) shared: Rc<RefCell<SharedState>>,
}

impl WpColorManagerV1Handler for ColorManagerHandler {
    fn handle_get_surface(
        &mut self, slf: &Rc<WpColorManagerV1>, id: &Rc<WpColorManagementSurfaceV1>,
        surface: &Rc<WlSurface>,
    ) {
        let surface_id = surface.unique_id();
        let mut state = self.shared.borrow_mut();
        let color = state
            .color_management
            .surfaces
            .entry(surface_id)
            .or_default();
        if color.has_color_object {
            post_error(
                slf,
                WpColorManagerV1Error::SURFACE_EXISTS.0,
                "surface already has color management",
            );
            return;
        }
        color.has_color_object = true;
        id.set_forward_to_server(false);
        id.set_handler(SurfaceColorHandler {
            shared: Rc::clone(&self.shared),
            surface_id,
        });
    }

    fn handle_get_surface_feedback(
        &mut self, _slf: &Rc<WpColorManagerV1>, id: &Rc<WpColorManagementSurfaceFeedbackV1>,
        surface: &Rc<WlSurface>,
    ) {
        id.set_forward_to_server(false);
        id.set_handler(FeedbackHandler {
            shared: Rc::clone(&self.shared),
        });
        self.shared
            .borrow_mut()
            .color_management
            .feedback
            .insert(id.unique_id(), (surface.unique_id(), Rc::downgrade(id)));
    }

    fn handle_create_parametric_creator(
        &mut self, slf: &Rc<WpColorManagerV1>, creator: &Rc<WpImageDescriptionCreatorParamsV1>,
    ) {
        creator.set_handler(ParamsHandler {
            shared: Rc::clone(&self.shared),
            color: ColorDescription::default(),
        });
        slf.send_create_parametric_creator(creator);
    }

    fn handle_get_output(
        &mut self, slf: &Rc<WpColorManagerV1>, id: &Rc<WpColorManagementOutputV1>,
        output: &Rc<WlOutput>,
    ) {
        id.set_handler(ForwardedDescriptionFactory(Rc::clone(&self.shared)));
        slf.send_get_output(id, output);
    }

    fn handle_create_icc_creator(
        &mut self, slf: &Rc<WpColorManagerV1>, creator: &Rc<WpImageDescriptionCreatorIccV1>,
    ) {
        creator.set_handler(ForwardedDescriptionFactory(Rc::clone(&self.shared)));
        slf.send_create_icc_creator(creator);
    }

    fn handle_create_windows_scrgb(
        &mut self, slf: &Rc<WpColorManagerV1>, description: &Rc<WpImageDescriptionV1>,
    ) {
        description.set_handler(DescriptionHandler::forwarded(
            &self.shared,
            ColorDescription::default(),
        ));
        slf.send_create_windows_scrgb(description);
    }
}

struct SurfaceColorHandler {
    shared: Rc<RefCell<SharedState>>,
    surface_id: u64,
}

impl SurfaceColorHandler {
    fn set_pending(&self, slf: &Rc<WpColorManagementSurfaceV1>, color: ColorDescription) {
        let mut shared = self.shared.borrow_mut();
        if let Some(surface) = shared.color_management.surfaces.get_mut(&self.surface_id) {
            surface.pending = Some(color);
        } else {
            post_error(
                slf,
                WpColorManagementSurfaceV1Error::INERT.0,
                "underlying surface was destroyed",
            );
        }
    }
}

impl WpColorManagementSurfaceV1Handler for SurfaceColorHandler {
    fn handle_destroy(&mut self, slf: &Rc<WpColorManagementSurfaceV1>) {
        if let Some(surface) = self
            .shared
            .borrow_mut()
            .color_management
            .surfaces
            .get_mut(&self.surface_id)
        {
            surface.has_color_object = false;
            surface.pending = Some(ColorDescription::srgb());
        }
        slf.delete_id();
    }

    fn handle_set_image_description(
        &mut self, slf: &Rc<WpColorManagementSurfaceV1>, description: &Rc<WpImageDescriptionV1>,
        _intent: WpColorManagerV1RenderIntent,
    ) {
        let color = description
            .try_get_handler_ref::<DescriptionHandler>()
            .ok()
            .and_then(|handler| handler.ready_color().cloned());
        let Some(color) = color else {
            post_error(
                slf,
                WpColorManagementSurfaceV1Error::IMAGE_DESCRIPTION.0,
                "image description is not ready",
            );
            return;
        };
        // A client can destroy the description as soon as this request is sent.
        self.set_pending(slf, color);
    }

    fn handle_unset_image_description(&mut self, slf: &Rc<WpColorManagementSurfaceV1>) {
        self.set_pending(slf, ColorDescription::srgb());
    }
}

struct FeedbackHandler {
    shared: Rc<RefCell<SharedState>>,
}

impl WpColorManagementSurfaceFeedbackV1Handler for FeedbackHandler {
    fn handle_destroy(&mut self, slf: &Rc<WpColorManagementSurfaceFeedbackV1>) {
        self.shared
            .borrow_mut()
            .color_management
            .feedback
            .remove(&slf.unique_id());
        slf.delete_id();
    }

    fn handle_get_preferred(
        &mut self, slf: &Rc<WpColorManagementSurfaceFeedbackV1>,
        description: &Rc<WpImageDescriptionV1>,
    ) {
        let state = self.shared.borrow();
        let color = &state.color_management;
        if !color.feedback.contains_key(&slf.unique_id()) {
            post_error(
                slf,
                WpColorManagementSurfaceFeedbackV1Error::INERT.0,
                "underlying surface was destroyed",
            );
            return;
        }
        description.set_forward_to_server(false);
        description.set_handler(DescriptionHandler::Preferred(color.preferred.clone()));
        description.send_ready(color.preferred_identity);
    }

    fn handle_get_preferred_parametric(
        &mut self, slf: &Rc<WpColorManagementSurfaceFeedbackV1>,
        description: &Rc<WpImageDescriptionV1>,
    ) {
        self.handle_get_preferred(slf, description);
    }
}

// These paths still forward creation and replies. Install the description handler to
// translate ready identities even when there are no captured parametric setters.
struct ForwardedDescriptionFactory(Rc<RefCell<SharedState>>);

impl WpColorManagementOutputV1Handler for ForwardedDescriptionFactory {
    fn handle_get_image_description(
        &mut self, slf: &Rc<WpColorManagementOutputV1>, description: &Rc<WpImageDescriptionV1>,
    ) {
        description.set_handler(DescriptionHandler::forwarded(
            &self.0,
            ColorDescription::default(),
        ));
        slf.send_get_image_description(description);
    }
}

impl WpImageDescriptionCreatorIccV1Handler for ForwardedDescriptionFactory {
    fn handle_create(
        &mut self, slf: &Rc<WpImageDescriptionCreatorIccV1>, description: &Rc<WpImageDescriptionV1>,
    ) {
        description.set_handler(DescriptionHandler::forwarded(
            &self.0,
            ColorDescription::default(),
        ));
        slf.send_create(description);
    }
}

enum DescriptionHandler {
    Preferred(ColorDescription),
    Forwarded {
        shared: Rc<RefCell<SharedState>>,
        color: ColorDescription,
        identity: Option<Rc<u32>>,
    },
}

impl DescriptionHandler {
    fn forwarded(shared: &Rc<RefCell<SharedState>>, color: ColorDescription) -> Self {
        Self::Forwarded {
            shared: Rc::clone(shared),
            color,
            identity: None,
        }
    }

    fn ready_color(&self) -> Option<&ColorDescription> {
        match self {
            Self::Preferred(color)
            | Self::Forwarded {
                color,
                identity: Some(_),
                ..
            } => Some(color),
            Self::Forwarded { identity: None, .. } => None,
        }
    }
}

impl WpImageDescriptionV1Handler for DescriptionHandler {
    fn handle_ready(&mut self, slf: &Rc<WpImageDescriptionV1>, upstream: u32) {
        if let Self::Forwarded {
            shared, identity, ..
        } = self
        {
            let translated = shared
                .borrow_mut()
                .color_management
                .upstream_identity(upstream);
            slf.send_ready(*translated);
            *identity = Some(translated);
        }
    }

    fn handle_destroy(&mut self, slf: &Rc<WpImageDescriptionV1>) {
        match self {
            Self::Preferred(_) => slf.delete_id(),
            Self::Forwarded { identity, .. } => {
                identity.take();
                slf.send_destroy();
            }
        }
    }

    fn handle_get_information(
        &mut self, slf: &Rc<WpImageDescriptionV1>, info: &Rc<WpImageDescriptionInfoV1>,
    ) {
        match self {
            Self::Preferred(color) => {
                info.set_forward_to_server(false);
                send_information(info, color);
                info.send_done();
                info.delete_id();
            }
            Self::Forwarded { .. } => slf.send_get_information(info),
        }
    }
}

struct ParamsHandler {
    shared: Rc<RefCell<SharedState>>,
    color: ColorDescription,
}

impl WpImageDescriptionCreatorParamsV1Handler for ParamsHandler {
    fn handle_create(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>,
        description: &Rc<WpImageDescriptionV1>,
    ) {
        description.set_handler(DescriptionHandler::forwarded(
            &self.shared,
            std::mem::take(&mut self.color),
        ));
        slf.send_create(description);
    }

    fn handle_set_tf_named(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>,
        tf: WpColorManagerV1TransferFunction,
    ) {
        self.color.tf_named = Some(tf.0);
        slf.send_set_tf_named(tf);
    }

    fn handle_set_tf_power(&mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, exponent: u32) {
        self.color.tf_power = Some(exponent);
        slf.send_set_tf_power(exponent);
    }

    fn handle_set_primaries_named(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>,
        primaries: WpColorManagerV1Primaries,
    ) {
        self.color.primaries_named = Some(primaries.0);
        slf.send_set_primaries_named(primaries);
    }

    fn handle_set_primaries(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, rx: i32, ry: i32, gx: i32, gy: i32,
        bx: i32, by: i32, wx: i32, wy: i32,
    ) {
        self.color.primaries = Some([rx, ry, gx, gy, bx, by, wx, wy]);
        slf.send_set_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    }

    fn handle_set_luminances(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, min: u32, max: u32, reference: u32,
    ) {
        self.color.luminances = Some((min, max, reference));
        slf.send_set_luminances(min, max, reference);
    }

    fn handle_set_mastering_display_primaries(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, rx: i32, ry: i32, gx: i32, gy: i32,
        bx: i32, by: i32, wx: i32, wy: i32,
    ) {
        self.color.target_primaries = Some([rx, ry, gx, gy, bx, by, wx, wy]);
        slf.send_set_mastering_display_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    }

    fn handle_set_mastering_luminance(
        &mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, min: u32, max: u32,
    ) {
        self.color.target_luminance = Some((min, max));
        slf.send_set_mastering_luminance(min, max);
    }

    fn handle_set_max_cll(&mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, max_cll: u32) {
        self.color.max_cll = Some(max_cll);
        slf.send_set_max_cll(max_cll);
    }

    fn handle_set_max_fall(&mut self, slf: &Rc<WpImageDescriptionCreatorParamsV1>, max_fall: u32) {
        self.color.max_fall = Some(max_fall);
        slf.send_set_max_fall(max_fall);
    }
}

fn complete_preferred(preferred: ColorDescription) -> ColorDescription {
    // The feedback reply must contain the mandatory parametric information.
    // Host feedback provides it, and sRGB is the fallback for incomplete feedback.
    if preferred.primaries.is_none()
        || preferred.luminances.is_none()
        || (preferred.tf_named.is_none() && preferred.tf_power.is_none())
    {
        ColorDescription::srgb()
    } else {
        preferred
    }
}

fn send_information(info: &Rc<WpImageDescriptionInfoV1>, color: &ColorDescription) {
    let primaries = color.primaries.expect("preferred color has primaries");
    let [rx, ry, gx, gy, bx, by, wx, wy] = primaries;
    info.send_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    if let Some(named) = color.primaries_named {
        info.send_primaries_named(WpColorManagerV1Primaries(named));
    }
    if let Some(power) = color.tf_power {
        info.send_tf_power(power);
    }
    if let Some(named) = color.tf_named {
        info.send_tf_named(WpColorManagerV1TransferFunction(named));
    }
    let (min, max, reference) = color.luminances.expect("preferred color has luminances");
    info.send_luminances(min, max, reference);
    let [rx, ry, gx, gy, bx, by, wx, wy] = color.target_primaries.unwrap_or(primaries);
    info.send_target_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    let (min, max) = color.target_luminance.unwrap_or((min, max));
    info.send_target_luminance(min, max);
    if let Some(max_cll) = color.max_cll {
        info.send_target_max_cll(max_cll);
    }
    if let Some(max_fall) = color.max_fall {
        info.send_target_max_fall(max_fall);
    }
}

fn post_error<T: Object>(object: &Rc<T>, code: u32, message: &str) {
    if let Some(client) = object.client() {
        let object: Rc<dyn Object> = object.clone();
        client.display().send_error(object, code, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_changes_apply_on_the_matching_surface_commit() {
        let sdr = ColorDescription::srgb();
        let pq = ColorDescription {
            primaries_named: Some(6),
            tf_named: Some(11),
            ..ColorDescription::default()
        };
        let mut colors = ColorManagementState::new(sdr.clone());
        colors.surfaces.entry(1).or_default().pending = Some(pq.clone());

        assert_eq!(colors.commit(2), (sdr.clone(), false));
        assert_eq!(colors.commit(1), (pq.clone(), true));
        assert_eq!(colors.commit(1), (pq, false));
        colors.surfaces.get_mut(&1).unwrap().pending = Some(sdr.clone());
        assert_eq!(colors.commit(1), (sdr, true));
    }
}
