use gtk::{
    glib,
    prelude::*,
    subclass::prelude::*,
};

use mutsumi::*;

const DANMAKU_DISTRIBUTION_BUCKETS: usize = 180;

fn build_danmaku_distribution(timeline: &[f64], duration: f64) -> Vec<f32> {
    if duration <= 0.0 || timeline.is_empty() {
        return Vec::new();
    }

    let mut buckets = vec![0.0_f32; DANMAKU_DISTRIBUTION_BUCKETS];
    for time in timeline.iter().copied() {
        if !(0.0..=duration).contains(&time) {
            continue;
        }
        let index = ((time / duration) * DANMAKU_DISTRIBUTION_BUCKETS as f64).floor() as usize;
        buckets[index.min(DANMAKU_DISTRIBUTION_BUCKETS - 1)] += 1.0;
    }

    let mut smoothed = vec![0.0_f32; DANMAKU_DISTRIBUTION_BUCKETS];
    for index in 0..DANMAKU_DISTRIBUTION_BUCKETS {
        let previous = index
            .checked_sub(1)
            .and_then(|index| buckets.get(index))
            .copied()
            .unwrap_or_default();
        let next = buckets.get(index + 1).copied().unwrap_or_default();
        smoothed[index] = previous * 0.25 + buckets[index] * 0.5 + next * 0.25;
    }

    let max = smoothed.iter().copied().fold(0.0_f32, f32::max);
    if max == 0.0 {
        return Vec::new();
    }

    smoothed
        .into_iter()
        .map(|value| (value / max).sqrt())
        .collect()
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
            self.snapshot_danmaku_distribution(snapshot);
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

        pub(super) fn set_danmaku_timeline(&self, timeline: Vec<f64>) {
            self.danmaku_timeline.replace(timeline);
            self.rebuild_danmaku_distribution();
        }

        pub(super) fn rebuild_danmaku_distribution(&self) {
            let distribution = super::build_danmaku_distribution(
                &self.danmaku_timeline.borrow(),
                self.obj().adjustment().upper(),
            );
            self.danmaku_distribution.replace(distribution);
            self.obj().queue_draw();
        }

        fn snapshot_danmaku_distribution(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let width = obj.width() as f32;
            let height = obj.height() as f32;
            let distribution = self.danmaku_distribution.borrow();
            if width <= 1.0 || height <= 1.0 || distribution.is_empty() {
                return;
            }

            let bucket_width = width / distribution.len() as f32;
            let gap = (bucket_width * 0.28).clamp(0.35, 1.8);
            let bar_width = (bucket_width - gap).max(0.8);
            let max_height = (height * 0.68).max(4.0);
            let baseline = height * 0.84;
            for (index, value) in distribution.iter().copied().enumerate() {
                if value <= 0.0 {
                    continue;
                }
                let bounds = graphene::Rect::new(
                    index as f32 * bucket_width + gap * 0.5,
                    (baseline - value * max_height).max(0.0),
                    bar_width,
                    (value * max_height).max(1.0),
                );
                snapshot.append_color(&gdk::RGBA::new(1.0, 1.0, 1.0, 0.16 + value * 0.22), &bounds);
            }
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

    pub fn is_dragging(&self) -> bool {
        self.imp().is_dragging.get()
    }

    pub fn set_chapter_list(&self, chapter_list: ChapterList) {
        self.clear_marks();

        for chapter in chapter_list {
            self.add_mark(chapter.time, gtk::PositionType::Top, None);
        }
    }

    pub fn set_danmaku_timeline(&self, timeline: Vec<f64>) {
        self.imp().set_danmaku_timeline(timeline);
    }

    pub fn refresh_danmaku_distribution(&self) {
        self.imp().rebuild_danmaku_distribution();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danmaku_distribution_ignores_out_of_range_values() {
        let distribution = build_danmaku_distribution(&[-1.0, 0.0, 50.0, 100.0, 101.0], 100.0);
        assert_eq!(distribution.len(), DANMAKU_DISTRIBUTION_BUCKETS);
        assert!(distribution.iter().all(|value| (0.0..=1.0).contains(value)));
        assert!(!distribution.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn danmaku_distribution_requires_a_duration() {
        assert!(build_danmaku_distribution(&[1.0], 0.0).is_empty());
    }
}
