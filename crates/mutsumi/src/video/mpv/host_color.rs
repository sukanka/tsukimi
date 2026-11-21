use std::{
    cell::RefCell,
    rc::{
        Rc,
        Weak,
    },
    time::Duration,
};

use gdk_wayland::{
    WaylandSurface,
    prelude::*,
};
use gtk::{
    gdk,
    glib,
};
use wayland_client::{
    Connection,
    Dispatch,
    EventQueue,
    Proxy,
    QueueHandle,
    WEnum,
    protocol::{
        wl_registry,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_feedback_v1::{
        self,
        WpColorManagementSurfaceFeedbackV1,
    },
    wp_color_manager_v1::{
        self,
        WpColorManagerV1,
    },
    wp_image_description_info_v1::{
        self,
        WpImageDescriptionInfoV1,
    },
    wp_image_description_v1::{
        self,
        WpImageDescriptionV1,
    },
};

use super::color::{
    ColorDescription,
    PREFERRED_COLOR,
};

const DISPLAY_FEEDBACK_KEY: &str = "mutsumi-host-color-feedback";

thread_local! {
    // The proxy has one preferred-color watch. The latest realization owns it.
    static ACTIVE_DISPLAY: RefCell<Weak<RefCell<DisplayFeedback>>> = const { RefCell::new(Weak::new()) };
}

/// Observes the preferred color of a realized GTK surface on the GTK thread.
pub(crate) struct HostColorFeedback {
    display: Weak<RefCell<DisplayFeedback>>,
    attachment: Weak<()>,
}

struct DisplayFeedback {
    registry: wl_registry::WlRegistry,
    queue: EventQueue<FeedbackState>,
    state: FeedbackState,
    source: Option<glib::SourceId>,
}

#[derive(Default)]
struct FeedbackState {
    manager: Option<ColorManager>,
    attachment: Option<Attachment>,
}

struct ColorManager {
    name: u32,
    proxy: WpColorManagerV1,
    parametric: bool,
}

struct Attachment {
    // Dropping this token invalidates the widget guard when another player realizes.
    _lifetime: Rc<()>,
    surface: WlSurface,
    feedback: Option<SurfaceFeedback>,
}

struct SurfaceFeedback {
    proxy: WpColorManagementSurfaceFeedbackV1,
    pending: Option<PendingDescription>,
}

enum PendingDescription {
    Image(WpImageDescriptionV1),
    Info {
        proxy: WpImageDescriptionInfoV1,
        description: ColorDescription,
    },
}

impl HostColorFeedback {
    pub(crate) fn new(surface: &gdk::Surface) -> Option<Self> {
        if let Some(previous) = ACTIVE_DISPLAY.with(|active| active.take().upgrade()) {
            previous.borrow_mut().detach();
        }

        let surface = surface.downcast_ref::<WaylandSurface>()?;
        let display = surface.display();
        if display.is_closed() {
            return None;
        }
        let surface = surface.wl_surface()?;
        let feedback = DisplayFeedback::for_display(&display, &surface)?;

        let lifetime = Rc::new(());
        let attachment = Rc::downgrade(&lifetime);
        let display = Rc::downgrade(&feedback);
        ACTIVE_DISPLAY.with(|active| active.replace(display.clone()));
        let source = glib::timeout_add_local(Duration::from_millis(50), {
            let display = display.clone();
            move || {
                let Some(feedback) = display.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let mut feedback = feedback.borrow_mut();
                if let Err(error) = feedback.dispatch() {
                    tracing::warn!(%error, "Wayland color feedback dispatch failed");
                    feedback.source.take();
                    feedback.detach();
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            }
        });
        let mut feedback = feedback.borrow_mut();
        feedback.state.attachment = Some(Attachment {
            _lifetime: lifetime,
            surface,
            feedback: None,
        });
        feedback.source = Some(source);
        Some(Self {
            display,
            attachment,
        })
    }
}

impl Drop for HostColorFeedback {
    fn drop(&mut self) {
        if self.attachment.strong_count() != 0
            && let Some(display) = self.display.upgrade()
        {
            display.borrow_mut().detach();
        }
    }
}

impl DisplayFeedback {
    fn for_display(display: &gdk::Display, surface: &WlSurface) -> Option<Rc<RefCell<Self>>> {
        // SAFETY: this private key holds Rc<RefCell<Self>> and is used on the GTK thread.
        if let Some(feedback) = unsafe { display.data::<Rc<RefCell<Self>>>(DISPLAY_FEEDBACK_KEY) } {
            return Some(unsafe { feedback.as_ref() }.clone());
        }

        let connection = Connection::from_backend(surface.backend().upgrade()?);
        let queue = connection.new_event_queue();
        // wl_registry has no wire destructor. Keep one for the lifetime of the display.
        let registry = connection.display().get_registry(&queue.handle(), ());
        let feedback = Rc::new(RefCell::new(Self {
            registry,
            queue,
            state: FeedbackState::default(),
            source: None,
        }));
        display.connect_closed(|display, _| {
            // SAFETY: same key and type as above. Drop the guest objects before GDK
            // disconnects. The widget guard and timeout hold only weak references.
            drop(unsafe { display.steal_data::<Rc<RefCell<Self>>>(DISPLAY_FEEDBACK_KEY) });
        });
        // SAFETY: the value is owned by the display and has no reference to it.
        unsafe { display.set_data(DISPLAY_FEEDBACK_KEY, feedback.clone()) };
        Some(feedback)
    }

    fn dispatch(&mut self) -> Result<(), wayland_client::DispatchError> {
        // GTK reads and flushes the shared connection. Dispatch only our queued events.
        self.queue.dispatch_pending(&mut self.state)?;
        if let (Some(manager), Some(attachment)) = (&self.state.manager, &mut self.state.attachment)
            && manager.parametric
            && attachment.feedback.is_none()
        {
            let handle = self.queue.handle();
            let proxy = manager
                .proxy
                .get_surface_feedback(&attachment.surface, &handle, ());
            let mut feedback = SurfaceFeedback {
                proxy,
                pending: None,
            };
            feedback.request_preferred(&handle);
            attachment.feedback = Some(feedback);
        }
        Ok(())
    }

    fn detach(&mut self) {
        if let Some(source) = self.source.take() {
            source.remove();
        }
        if self.state.attachment.take().is_some() {
            publish(ColorDescription::srgb());
        }
    }
}

impl Drop for DisplayFeedback {
    fn drop(&mut self) {
        self.detach();
        self.state.manager.take();
        if let Some(backend) = self.registry.backend().upgrade() {
            let _ = backend.destroy_object(&self.registry.id());
        }
    }
}

impl Drop for ColorManager {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

impl FeedbackState {
    fn feedback(&mut self) -> Option<&mut SurfaceFeedback> {
        self.attachment.as_mut()?.feedback.as_mut()
    }
}

impl SurfaceFeedback {
    fn request_preferred(&mut self, handle: &QueueHandle<FeedbackState>) {
        self.pending = Some(PendingDescription::Image(
            self.proxy.get_preferred_parametric(handle, ()),
        ));
    }
}

impl Drop for SurfaceFeedback {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

impl Drop for PendingDescription {
    fn drop(&mut self) {
        match self {
            Self::Image(image) => image.destroy(),
            Self::Info { proxy, .. } => {
                // The server destroys this on done. Discard the local proxy if
                // we detach or request a new description before it arrives.
                if let Some(backend) = proxy.backend().upgrade() {
                    let _ = backend.destroy_object(&proxy.id());
                }
            }
        }
    }
}

fn publish(description: ColorDescription) {
    PREFERRED_COLOR.send_if_modified(|current| {
        if *current == description {
            return false;
        }
        *current = description;
        true
    });
}

impl Dispatch<wl_registry::WlRegistry, ()> for FeedbackState {
    fn event(
        state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(),
        _: &Connection, handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == WpColorManagerV1::interface().name
                && version >= 1
                && state.manager.is_none() =>
            {
                state.manager = Some(ColorManager {
                    name,
                    proxy: registry.bind(name, 1, handle, ()),
                    parametric: false,
                });
            }
            wl_registry::Event::GlobalRemove { name }
                if state
                    .manager
                    .as_ref()
                    .is_some_and(|manager| manager.name == name) =>
            {
                if let Some(attachment) = &mut state.attachment {
                    attachment.feedback = None;
                    publish(ColorDescription::srgb());
                }
                state.manager.take();
            }
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagerV1, ()> for FeedbackState {
    fn event(
        state: &mut Self, proxy: &WpColorManagerV1, event: wp_color_manager_v1::Event, _: &(),
        _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let Some(manager) = &mut state.manager
            && manager.proxy == *proxy
            && matches!(
                event,
                wp_color_manager_v1::Event::SupportedFeature {
                    feature: WEnum::Value(wp_color_manager_v1::Feature::Parametric),
                }
            )
        {
            manager.parametric = true;
        }
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, ()> for FeedbackState {
    fn event(
        state: &mut Self, proxy: &WpColorManagementSurfaceFeedbackV1,
        event: wp_color_management_surface_feedback_v1::Event, _: &(), _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let Some(feedback) = state.feedback()
            && feedback.proxy == *proxy
            && matches!(
                event,
                wp_color_management_surface_feedback_v1::Event::PreferredChanged { .. }
            )
        {
            feedback.request_preferred(handle);
        }
    }
}

impl Dispatch<WpImageDescriptionV1, ()> for FeedbackState {
    fn event(
        state: &mut Self, image: &WpImageDescriptionV1, event: wp_image_description_v1::Event,
        _: &(), _: &Connection, handle: &QueueHandle<Self>,
    ) {
        let Some(feedback) = state.feedback() else {
            return;
        };
        if !matches!(&feedback.pending, Some(PendingDescription::Image(pending)) if pending == image)
        {
            return;
        }
        match event {
            wp_image_description_v1::Event::Ready { .. } => {
                feedback.pending = Some(PendingDescription::Info {
                    proxy: image.get_information(handle, ()),
                    description: ColorDescription::default(),
                });
            }
            wp_image_description_v1::Event::Failed { cause, .. } => {
                tracing::debug!(?cause, "Wayland preferred color description unavailable");
                feedback.pending = None;
                publish(ColorDescription::srgb());
            }
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for FeedbackState {
    fn event(
        state: &mut Self, info: &WpImageDescriptionInfoV1,
        event: wp_image_description_info_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        let Some(feedback) = state.feedback() else {
            return;
        };
        let Some(PendingDescription::Info { proxy, description }) = &mut feedback.pending else {
            return;
        };
        if proxy != info {
            return;
        }

        use wp_image_description_info_v1::Event;
        match event {
            Event::Primaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                description.primaries = Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            Event::PrimariesNamed { primaries } => {
                description.primaries_named = Some(primaries.into())
            }
            Event::TfPower { eexp } => description.tf_power = Some(eexp),
            Event::TfNamed { tf } => description.tf_named = Some(tf.into()),
            Event::Luminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                description.luminances = Some((min_lum, max_lum, reference_lum));
            }
            Event::TargetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                description.target_primaries = Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            Event::TargetLuminance { min_lum, max_lum } => {
                description.target_luminance = Some((min_lum, max_lum));
            }
            Event::TargetMaxCll { max_cll } => description.max_cll = Some(max_cll),
            Event::TargetMaxFall { max_fall } => description.max_fall = Some(max_fall),
            Event::Done => {
                let description = std::mem::take(description);
                feedback.pending = None;
                match description.to_gdk_color_state() {
                    Ok(_) => publish(description),
                    Err(error) => {
                        tracing::debug!(%error, "Unsupported Wayland preferred color uses the SDR fallback");
                        publish(ColorDescription::srgb());
                    }
                }
            }
            _ => {}
        }
    }
}
