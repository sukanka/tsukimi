use gtk::{
    glib,
    prelude::*,
    subclass::prelude::*,
};

use mutsumi::*;

const DANMAKU_BUCKETS: usize = 180;

fn build_danmaku_distribution(timeline: &[f64], duration: f64) -> Vec<f32> {
    if !duration.is_finite() || duration <= 0.0 || timeline.is_empty() {
        return Vec::new();
    }

    let mut buckets = [0.0_f32; DANMAKU_BUCKETS];
    for &time in timeline {
        if (0.0..=duration).contains(&time) {
            let index = (time / duration * DANMAKU_BUCKETS as f64) as usize;
            buckets[index.min(DANMAKU_BUCKETS - 1)] += 1.0;
        }
    }

    let mut distribution = Vec::with_capacity(DANMAKU_BUCKETS);
    for (index, &count) in buckets.iter().enumerate() {
        let previous = index.checked_sub(1).map_or(0.0, |index| buckets[index]);
        let next = buckets.get(index + 1).copied().unwrap_or_default();
        distribution.push(previous * 0.25 + count * 0.5 + next * 0.25);
    }
    let peak = distribution.iter().copied().fold(0.0, f32::max);
    if peak == 0.0 {
        return Vec::new();
    }
    for value in &mut distribution {
        *value = (*value / peak).sqrt();
    }
    distribution
}

mod imp {
    use std::cell::{
        Cell,
        RefCell,
    };

    use gtk::{
        gdk,
        glib,
        graphene,
        prelude::*,
        subclass::prelude::*,
    };

    use crate::ui::mpv::sink::MPVPlaySink;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::VideoScale)]
    pub struct VideoScale {
        #[property(get, set = Self::set_player, explicit_notify, nullable)]
        pub player: glib::WeakRef<MPVPlaySink>,

        pub is_dragging: Cell<bool>,
        pub danmaku_timeline: RefCell<Vec<f64>>,
        pub danmaku_distribution: RefCell<Vec<f32>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for VideoScale {
        const NAME: &'static str = "VideoScale";
        type Type = super::VideoScale;
        type ParentType = gtk::Scale;
    }

    #[glib::derived_properties]
    impl ObjectImpl for VideoScale {
        fn constructed(&self) {
            self.parent_constructed();

            // new GestureClick with add_controller is doesn't work for connect_released
            //
            // so we need to iterate through the controllers to get the GestureClick
            // and then connect the signals
            let mut gesture = gtk::GestureClick::new();
            self.obj()
                .observe_controllers()
                .into_iter()
                .for_each(|collection| {
                    if let Ok(event) = collection
                        && event.type_() == gtk::GestureClick::static_type()
                    {
                        gesture = event.downcast::<gtk::GestureClick>().unwrap();
                    }
                });

            gesture.connect_pressed(glib::clone!(
                #[weak(rename_to = imp)]
                self,
                move |_, _, _, _| {
                    imp.on_click_pressed();
                }
            ));

            gesture.connect_released(glib::clone!(
                #[weak(rename_to = imp)]
                self,
                move |_, _, _, _| {
                    imp.on_click_released();
                }
            ));
        }
    }
    impl WidgetImpl for VideoScale {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let width = obj.width() as f32;
            let height = obj.height() as f32;
            let distribution = self.danmaku_distribution.borrow();
            if width > 1.0 && height > 1.0 && !distribution.is_empty() {
                let bucket_width = width / distribution.len() as f32;
                let gap = (bucket_width * 0.28).clamp(0.35, 1.8);
                let bar_width = (bucket_width - gap).max(0.8);
                let max_height = (height * 0.68).max(4.0);
                let baseline = height * 0.84;
                for (index, &value) in distribution.iter().enumerate() {
                    if value > 0.0 {
                        let bounds = graphene::Rect::new(
                            index as f32 * bucket_width + gap * 0.5,
                            (baseline - value * max_height).max(0.0),
                            bar_width,
                            (value * max_height).max(1.0),
                        );
                        let color = gdk::RGBA::new(1.0, 1.0, 1.0, 0.16 + value * 0.22);
                        snapshot.append_color(&color, &bounds);
                    }
                }
            }
            self.parent_snapshot(snapshot);
        }
    }
    impl RangeImpl for VideoScale {}
    impl ScaleImpl for VideoScale {}

    impl VideoScale {
        fn set_player(&self, player: Option<MPVPlaySink>) {
            if self.player.upgrade() == player {
                return;
            }
            self.player.set(player.as_ref());
        }

        fn on_click_pressed(&self) {
            self.is_dragging.set(true);
        }

        fn on_click_released(&self) {
            let obj = self.obj();
            self.on_seek_finished(obj.value());
            self.is_dragging.set(false);
        }

        fn on_seek_finished(&self, value: f64) {
            self.player.upgrade().unwrap().set_position(value);
        }
    }
}

glib::wrapper! {
    pub struct VideoScale(ObjectSubclass<imp::VideoScale>)
        @extends gtk::Widget, gtk::Scale, gtk::Range, @implements gtk::Accessible, gtk::Buildable, gtk::Orientable, gtk::ConstraintTarget;
}

impl Default for VideoScale {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoScale {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub fn update_position_callback(&self) -> glib::ControlFlow {
        let position = &self.player().unwrap().position();
        if *position > 0.0 {
            self.set_value(*position);
        }
        glib::ControlFlow::Continue
    }

    pub fn set_cache_end_time(&self, end_time: i64) {
        self.set_fill_level(end_time as f64);
    }

    pub fn reset_scale(&self) {
        self.set_value(0.0);
        self.set_fill_level(0.0);
    }

    pub(super) fn set_danmaku_timeline(&self, timeline: Vec<f64>) {
        self.imp().danmaku_timeline.replace(timeline);
        self.refresh_danmaku_distribution();
    }

    pub(super) fn refresh_danmaku_distribution(&self) {
        let imp = self.imp();
        let distribution =
            build_danmaku_distribution(&imp.danmaku_timeline.borrow(), self.adjustment().upper());
        imp.danmaku_distribution.replace(distribution);
        self.queue_draw();
    }

    pub fn is_dragging(&self) -> bool {
        self.imp().is_dragging.get()
    }

    pub fn set_chapter_list(&self, chapter_list: ChapterList) {
        self.clear_marks();

        for chapter in chapter_list {
            self.add_mark(chapter.time, gtk::PositionType::Top, None);
        }
    }
}
