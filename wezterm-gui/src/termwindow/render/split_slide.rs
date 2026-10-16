//! A pane opened by a split slides in from the side it was opened on.
//!
//! Only the picture moves. By the time the new pane is first painted the
//! split has already given every pane its final size, once, so the programs
//! in them see a single resize rather than one a frame. The new pane is
//! painted where it ends up, recorded, and replayed shifted out towards the
//! edge it comes from, cropped to its own place, the divider beside it going
//! along.
//!
//! Nothing says a pane came by a split, wherever the split was asked for
//! (a key, the menu, the CLI, another client). It is read off the layout:
//! one pane more than last frame, in the same tab, with nobody gone, next
//! to a pane that used to cover the ground it now stands on.

use ::window::RectF;
use mux::pane::PaneId;
use mux::tab::{PositionedPane, PositionedSplit, SplitDirection, TabId};
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_millis(180);
/// A slide is shown in no fewer frames than this. Where a frame takes
/// longer than its share of `DURATION` -- a busy window, a build without
/// optimisation -- the slide takes longer rather than skip most of its way:
/// timed on the clock alone it got one frame in, at a third of the way,
/// and was then found at its end.
const MIN_FRAMES: u32 = 8;
/// The first frame is drawn as if one had already passed, so the pane is
/// on its way in that frame rather than still all outside its place.
const FIRST_FRAME: Duration = Duration::from_micros(16_667);

/// The edge of its place a new pane comes in from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellRect {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
}

impl CellRect {
    fn of(pos: &PositionedPane) -> Self {
        Self {
            left: pos.left,
            top: pos.top,
            width: pos.width,
            height: pos.height,
        }
    }

    fn right(&self) -> usize {
        self.left + self.width
    }

    fn bottom(&self) -> usize {
        self.top + self.height
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.left < other.right()
            && other.left < self.right()
            && self.top < other.bottom()
            && other.top < self.bottom()
    }

    fn rows_overlap(&self, other: &Self) -> bool {
        self.top < other.bottom() && other.top < self.bottom()
    }

    fn cols_overlap(&self, other: &Self) -> bool {
        self.left < other.right() && other.left < self.right()
    }
}

/// The panes a tab showed in one frame.
#[derive(Debug, PartialEq, Eq)]
pub struct TabLayout {
    tab_id: TabId,
    zoomed: bool,
    panes: Vec<(PaneId, CellRect)>,
}

impl TabLayout {
    pub fn of(tab_id: TabId, panes: &[PositionedPane]) -> Self {
        Self {
            tab_id,
            zoomed: panes.iter().any(|pos| pos.is_zoomed),
            panes: panes
                .iter()
                .map(|pos| (pos.pane.pane_id(), CellRect::of(pos)))
                .collect(),
        }
    }

    fn rect(&self, pane_id: PaneId) -> Option<CellRect> {
        self.panes
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, rect)| *rect)
    }

    /// The pane `self` gained over `before` by a split, the edge it comes
    /// in from and a pane that made room for it. Anything else that adds a pane -- a tab switched to,
    /// a zoom let go of, an overlay closed -- also changes or removes one
    /// that was there, or leaves no neighbour that gave up the room.
    fn opened_by_split(&self, before: &Self) -> Option<(PaneId, Edge, PaneId)> {
        if self.tab_id != before.tab_id
            || self.zoomed
            || before.zoomed
            || before.panes.is_empty()
            || self.panes.len() != before.panes.len() + 1
        {
            return None;
        }
        let mut new = None;
        for (id, rect) in &self.panes {
            if before.rect(*id).is_none() {
                if new.is_some() {
                    return None;
                }
                new = Some((*id, *rect));
            }
        }
        let (new_id, new) = new?;
        self.panes.iter().find_map(|(id, now)| {
            let was = before.rect(*id)?;
            // It stood where the new pane is now, and gave that up.
            if was == *now || !was.overlaps(&new) {
                return None;
            }
            let edge = if new.left == now.right() + 1 && new.rows_overlap(now) {
                Edge::Right
            } else if new.right() + 1 == now.left && new.rows_overlap(now) {
                Edge::Left
            } else if new.top == now.bottom() + 1 && new.cols_overlap(now) {
                Edge::Bottom
            } else if new.bottom() + 1 == now.top && new.cols_overlap(now) {
                Edge::Top
            } else {
                return None;
            };
            Some((new_id, edge, *id))
        })
    }
}

/// A slide under way.
#[derive(Clone, Copy, Debug)]
pub struct SplitSlide {
    pub pane_id: PaneId,
    pub edge: Edge,
    /// A pane that made room for it. Until the new pane gets there, the
    /// room still looks like this one's.
    pub from_pane: PaneId,
    started: Instant,
    /// Frames painted with it, this one included.
    frames: u32,
}

impl SplitSlide {
    /// How much of the way it still has to go, 1 at the start and 0 on
    /// arrival; `None` once it is there.
    fn remaining(&self, now: Instant) -> Option<f32> {
        let elapsed = now.saturating_duration_since(self.started) + FIRST_FRAME;
        let by_clock = elapsed.as_secs_f32() / DURATION.as_secs_f32();
        let by_frames = self.frames as f32 / MIN_FRAMES as f32;
        let t = by_clock.min(by_frames);
        if t >= 1.0 {
            return None;
        }
        Some(1.0 - ease_out(t))
    }

    /// The shift that puts the pane `remaining` of the way back out of a
    /// place `width` by `height` pixels.
    pub fn offset(&self, remaining: f32, width: f32, height: f32) -> (f32, f32) {
        match self.edge {
            Edge::Right => (remaining * width, 0.),
            Edge::Left => (-remaining * width, 0.),
            Edge::Bottom => (0., remaining * height),
            Edge::Top => (0., -remaining * height),
        }
    }

    /// The divider the split put along the pane's leading edge.
    pub fn leads(&self, pos: &PositionedPane, split: &PositionedSplit) -> bool {
        let pane = CellRect::of(pos);
        let rows = split.top < pane.bottom() && pane.top < split.top + split.size;
        let cols = split.left < pane.right() && pane.left < split.left + split.size;
        match (self.edge, split.direction) {
            (Edge::Right, SplitDirection::Horizontal) => split.left + 1 == pane.left && rows,
            (Edge::Left, SplitDirection::Horizontal) => split.left == pane.right() && rows,
            (Edge::Bottom, SplitDirection::Vertical) => split.top + 1 == pane.top && cols,
            (Edge::Top, SplitDirection::Vertical) => split.top == pane.bottom() && cols,
            _ => false,
        }
    }
}

/// What of `place` the pane, shifted by `shift`, has yet to cover.
pub fn wake(place: RectF, shift: (f32, f32)) -> RectF {
    let (dx, dy) = (
        shift.0.clamp(-place.width(), place.width()),
        shift.1.clamp(-place.height(), place.height()),
    );
    if dx > 0. {
        euclid::rect(place.min_x(), place.min_y(), dx, place.height())
    } else if dx < 0. {
        euclid::rect(place.max_x() + dx, place.min_y(), -dx, place.height())
    } else if dy > 0. {
        euclid::rect(place.min_x(), place.min_y(), place.width(), dy)
    } else if dy < 0. {
        euclid::rect(place.min_x(), place.max_y() + dy, place.width(), -dy)
    } else {
        euclid::rect(place.min_x(), place.min_y(), 0., 0.)
    }
}

/// Fast, then settling. Squared rather than cubed: cubed, the first two
/// frames covered two thirds of the way and the rest was barely seen to move.
fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(2)
}

/// Last frame's layout and the slide it started, if one is still running.
#[derive(Default)]
pub struct SplitSlides {
    last: Option<TabLayout>,
    running: Option<SplitSlide>,
}

impl SplitSlides {
    /// Take this frame's layout; the slide to paint it with, and how much
    /// of its way is left.
    pub fn advance(
        &mut self,
        layout: Option<TabLayout>,
        enabled: bool,
        now: Instant,
    ) -> Option<(SplitSlide, f32)> {
        let before = std::mem::replace(&mut self.last, layout);
        if !enabled {
            self.running = None;
            return None;
        }
        if let (Some(before), Some(layout)) = (before.as_ref(), self.last.as_ref()) {
            if let Some((pane_id, edge, from_pane)) = layout.opened_by_split(before) {
                self.running = Some(SplitSlide {
                    pane_id,
                    edge,
                    from_pane,
                    started: now,
                    frames: 0,
                });
            }
        }
        let slide = self.running.as_mut()?;
        slide.frames += 1;
        let slide = *slide;
        // Gone from the tab, or the tab gone from the window: nothing left
        // to slide.
        let on_screen = self
            .last
            .as_ref()
            .is_some_and(|layout| layout.rect(slide.pane_id).is_some() && !layout.zoomed);
        match slide.remaining(now).filter(|_| on_screen) {
            Some(remaining) => Some((slide, remaining)),
            None => {
                self.running = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(tab_id: TabId, panes: &[(PaneId, [usize; 4])]) -> TabLayout {
        TabLayout {
            tab_id,
            zoomed: false,
            panes: panes
                .iter()
                .map(|(id, [left, top, width, height])| {
                    (
                        *id,
                        CellRect {
                            left: *left,
                            top: *top,
                            width: *width,
                            height: *height,
                        },
                    )
                })
                .collect(),
        }
    }

    fn new_and_edge((new, edge, _): (PaneId, Edge, PaneId)) -> (PaneId, Edge) {
        (new, edge)
    }

    #[test]
    fn a_split_slides_the_new_pane_in_from_the_side_it_opened_on() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        let right = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(right.opened_by_split(&one).map(new_and_edge), Some((2, Edge::Right)));
        let left = layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]);
        assert_eq!(left.opened_by_split(&one).map(new_and_edge), Some((2, Edge::Left)));
        let below = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        assert_eq!(below.opened_by_split(&one).map(new_and_edge), Some((2, Edge::Bottom)));
        let above = layout(1, &[(2, [0, 0, 81, 11]), (1, [0, 12, 81, 12])]);
        assert_eq!(above.opened_by_split(&one).map(new_and_edge), Some((2, Edge::Top)));
    }

    #[test]
    fn a_split_across_the_whole_tab_slides_in_beside_every_pane_it_squeezed() {
        let two = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        let three = layout(
            1,
            &[(1, [0, 0, 40, 12]), (2, [0, 13, 40, 11]), (3, [41, 0, 40, 24])],
        );
        assert_eq!(
            three.opened_by_split(&two).map(new_and_edge),
            Some((3, Edge::Right))
        );
    }

    #[test]
    fn a_pane_that_did_not_come_by_a_split_does_not_slide() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        // Another tab with two panes.
        assert_eq!(layout(2, &two.panes_spec()).opened_by_split(&one), None);
        // A zoom let go of.
        let zoomed = TabLayout {
            zoomed: true,
            ..layout(1, &[(1, [0, 0, 81, 24])])
        };
        assert_eq!(two.opened_by_split(&zoomed), None);
        // An overlay closed: the pane under it comes back in its place.
        let overlay = layout(1, &[(1, [0, 0, 40, 24]), (9, [41, 0, 40, 24])]);
        let back = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(back.opened_by_split(&overlay), None);
        // A window's first frame.
        assert_eq!(
            two.opened_by_split(&TabLayout {
                tab_id: 1,
                zoomed: false,
                panes: vec![]
            }),
            None
        );
        // Its neighbour has not given up the room yet.
        let stale = layout(1, &[(1, [0, 0, 81, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(stale.opened_by_split(&one), None);
    }

    impl TabLayout {
        fn panes_spec(&self) -> Vec<(PaneId, [usize; 4])> {
            self.panes
                .iter()
                .map(|(id, r)| (*id, [r.left, r.top, r.width, r.height]))
                .collect()
        }
    }

    /// The remaining way of each frame painted with the slide, a frame
    /// every `frame` from the split on.
    fn run(frame: Duration) -> Vec<f32> {
        let start = Instant::now();
        let mut slides = SplitSlides::default();
        let one = || Some(layout(1, &[(1, [0, 0, 81, 24])]));
        let two = || Some(layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]));
        assert!(slides.advance(one(), true, start).is_none());
        let mut steps = vec![];
        let mut now = start;
        while let Some((slide, remaining)) = slides.advance(two(), true, now) {
            assert_eq!((slide.pane_id, slide.edge, slide.from_pane), (2, Edge::Right, 1));
            steps.push(remaining);
            now += frame;
            assert!(steps.len() < 100);
        }
        // And it does not start again.
        assert!(slides.advance(two(), true, now + frame).is_none());
        steps
    }

    #[test]
    fn a_slide_starts_on_the_split_and_settles_into_place() {
        let steps = run(Duration::from_micros(8_333));
        // On its way from the first frame, never back, and there within
        // its time.
        assert!(steps[0] < 1.0 && steps[0] > 0.6, "{steps:?}");
        // No frame jumps much more of the way than the first.
        let first = 1.0 - steps[0];
        assert!(
            steps.windows(2).all(|w| w[0] - w[1] <= first * 1.05),
            "{steps:?}"
        );
        assert!(steps.windows(2).all(|w| w[1] < w[0]), "{steps:?}");
        assert!(steps.len() as u32 >= MIN_FRAMES, "{steps:?}");
        assert!(steps.len() <= 22, "{steps:?}");
        let slide = SplitSlide {
            pane_id: 2,
            edge: Edge::Right,
            from_pane: 1,
            started: Instant::now(),
            frames: 1,
        };
        assert_eq!(slide.offset(0.5, 300., 200.), (150., 0.));
    }

    #[test]
    fn slow_frames_slow_the_slide_rather_than_skip_it() {
        let steps = run(Duration::from_millis(60));
        assert!(steps.len() as u32 >= MIN_FRAMES - 1, "{steps:?}");
        assert!(steps.windows(2).all(|w| w[1] < w[0]), "{steps:?}");
    }

    #[test]
    fn with_the_setting_off_nothing_slides() {
        let start = Instant::now();
        let mut slides = SplitSlides::default();
        assert!(slides
            .advance(Some(layout(1, &[(1, [0, 0, 81, 24])])), false, start)
            .is_none());
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert!(slides.advance(Some(two), false, start).is_none());
    }

    #[test]
    fn a_pane_closed_mid_slide_stops_it() {
        let start = Instant::now();
        let mut slides = SplitSlides::default();
        slides.advance(Some(layout(1, &[(1, [0, 0, 81, 24])])), true, start);
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert!(slides.advance(Some(two), true, start).is_some());
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        assert!(slides
            .advance(Some(one), true, start + Duration::from_millis(10))
            .is_none());
    }
}
