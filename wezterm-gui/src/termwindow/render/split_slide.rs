//! A pane opened by a split slides in from the side it was opened on, and
//! a pane closed goes back out the same way.
//!
//! Only the picture moves. By the time the new pane is first painted the
//! split has already given every pane its final size, once, so the programs
//! in them see a single resize rather than one a frame. The new pane is
//! painted where it ends up, recorded, and replayed shifted out towards the
//! edge it comes from, cropped to its own place, the divider beside it going
//! along. A closed pane is out of the tab, and its room given to the panes
//! beside it, by the time it is found missing: it is painted from last
//! frame's hold on it, where it stood, over them on its way out.
//!
//! Room given or taken along a pane's left or top moves the pane, and its
//! text with it, rather than only changing its size. Drawn where they stand
//! by then, those panes would have their text gone from where it was at the
//! first frame, and found across the window at the last. They are drawn
//! from where they stood instead, and go along with the slide.
//!
//! Nothing says a pane came by a split, wherever the split was asked for
//! (a key, the menu, the CLI, another client). It is read off the layout:
//! one pane more than last frame, in the same tab, with nobody gone, next
//! to a pane that used to cover the ground it now stands on. A closed pane
//! is read off it backwards: one pane fewer, nobody new, and a pane that
//! stood beside it covering its ground.

use ::window::RectF;
use mux::pane::{Pane, PaneId};
use mux::tab::{PositionedPane, PositionedSplit, SplitDirection, TabId};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_millis(180);
/// Going out is quicker than coming in: by then the panes left are the
/// ones being looked at.
const LEAVE_DURATION: Duration = Duration::from_millis(140);
/// A slide is shown in no fewer frames than this. Where a frame takes
/// longer than its share of `DURATION` -- a busy window, a build without
/// optimisation -- the slide takes longer rather than skip most of its way:
/// timed on the clock alone it got one frame in, at a third of the way,
/// and was then found at its end.
const MIN_FRAMES: u32 = 8;
/// The first frame is drawn as if one had already passed, so the pane is
/// on its way in that frame rather than still all outside its place.
const FIRST_FRAME: Duration = Duration::from_micros(16_667);
/// A slide painted this many times and still not at its end is let go of.
/// Frames that are painted but never shown -- a pass that fails every time
/// -- would otherwise have it ask for frames for ever.
const MOST_PAINTS: u32 = 8 * MIN_FRAMES;

/// The edge of its place a new pane comes in from, or a closed one goes
/// out by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// The shift that puts a pane `out` of the way out through this edge
    /// of a place `width` by `height` pixels.
    fn shift(self, out: f32, width: f32, height: f32) -> (f32, f32) {
        match self {
            Edge::Right => (out * width, 0.),
            Edge::Left => (-out * width, 0.),
            Edge::Bottom => (0., out * height),
            Edge::Top => (0., -out * height),
        }
    }

    /// Whether `split` runs along the side of `pane` across from this edge:
    /// the divider its split put in.
    fn leads(self, pane: CellRect, split: &PositionedSplit) -> bool {
        let rows = split.top < pane.bottom() && pane.top < split.top + split.size;
        let cols = split.left < pane.right() && pane.left < split.left + split.size;
        match (self, split.direction) {
            (Edge::Right, SplitDirection::Horizontal) => split.left + 1 == pane.left && rows,
            (Edge::Left, SplitDirection::Horizontal) => split.left == pane.right() && rows,
            (Edge::Bottom, SplitDirection::Vertical) => split.top + 1 == pane.top && cols,
            (Edge::Top, SplitDirection::Vertical) => split.top == pane.bottom() && cols,
            _ => false,
        }
    }

    /// That divider, for a pane at `pane` that was one side of a split: it
    /// runs the whole of that side.
    fn divider(self, pane: CellRect) -> PositionedSplit {
        let (direction, left, top, size) = match self {
            Edge::Right => (
                SplitDirection::Horizontal,
                pane.left.saturating_sub(1),
                pane.top,
                pane.height,
            ),
            Edge::Left => (SplitDirection::Horizontal, pane.right(), pane.top, pane.height),
            Edge::Bottom => (
                SplitDirection::Vertical,
                pane.left,
                pane.top.saturating_sub(1),
                pane.width,
            ),
            Edge::Top => (SplitDirection::Vertical, pane.left, pane.bottom(), pane.width),
        };
        PositionedSplit {
            // Only ever painted, never dragged: no node of the tab's tree.
            index: 0,
            direction,
            left,
            top,
            size,
        }
    }

    /// The edge across from this one.
    fn opposite(self) -> Self {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    /// Where the side of `rect` across from this edge is.
    fn far_side(self, rect: RectF) -> f32 {
        match self {
            Edge::Left => rect.max_x(),
            Edge::Right => rect.min_x(),
            Edge::Top => rect.max_y(),
            Edge::Bottom => rect.min_y(),
        }
    }

    /// The cells a pane that stood at `was` and stands at `now` moved by,
    /// along this edge: towards it, into the room of a pane going out
    /// through it, or away from it, giving room to one coming in by it.
    /// Room given or taken along the right or the bottom only changes the
    /// size of the panes beside it.
    fn moved(self, was: CellRect, now: CellRect, towards: bool) -> Option<usize> {
        let (was, now) = match self {
            Edge::Left => (was.left, now.left),
            Edge::Top => (was.top, now.top),
            Edge::Right | Edge::Bottom => return None,
        };
        let by = if towards {
            was.checked_sub(now)
        } else {
            now.checked_sub(was)
        };
        by.filter(|by| *by > 0)
    }
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

    /// The cells a divider runs along.
    fn of_split(split: &PositionedSplit) -> Self {
        let (width, height) = match split.direction {
            SplitDirection::Horizontal => (1, split.size),
            SplitDirection::Vertical => (split.size, 1),
        };
        Self {
            left: split.left,
            top: split.top,
            width,
            height,
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

    fn contains(&self, other: &Self) -> bool {
        self.left <= other.left
            && other.right() <= self.right()
            && self.top <= other.top
            && other.bottom() <= self.bottom()
    }

    fn union(&self, other: &Self) -> Self {
        let left = self.left.min(other.left);
        let top = self.top.min(other.top);
        Self {
            left,
            top,
            width: self.right().max(other.right()) - left,
            height: self.bottom().max(other.bottom()) - top,
        }
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
    /// in from and a pane that made room for it. Anything else that adds a
    /// pane -- a tab switched to, a zoom let go of, an overlay closed -- also
    /// changes or removes one that was there, or leaves no neighbour that
    /// gave up the room.
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

    /// The pane `before` had that `self` lost by its closing, as its place
    /// in `before`, and the edge it goes out by: away from the panes that
    /// took its room. A tab switched away from or a zoom changes the tab or
    /// zooms it; an overlay puts another pane where one was.
    fn closed(&self, before: &Self) -> Option<(usize, Edge)> {
        if self.tab_id != before.tab_id
            || self.zoomed
            || before.zoomed
            || self.panes.is_empty()
            || self.panes.len() + 1 != before.panes.len()
        {
            return None;
        }
        let mut gone = None;
        for (index, (id, rect)) in before.panes.iter().enumerate() {
            if self.rect(*id).is_none() {
                if gone.is_some() {
                    return None;
                }
                gone = Some((index, *rect));
            }
        }
        let (index, gone) = gone?;
        self.panes.iter().find_map(|(id, now)| {
            let was = before.rect(*id)?;
            // It stood beside the closed pane, and has its ground now.
            if was == *now || !now.overlaps(&gone) {
                return None;
            }
            let edge = if gone.left == was.right() + 1 && gone.rows_overlap(&was) {
                Edge::Right
            } else if gone.right() + 1 == was.left && gone.rows_overlap(&was) {
                Edge::Left
            } else if gone.top == was.bottom() + 1 && gone.cols_overlap(&was) {
                Edge::Bottom
            } else if gone.bottom() + 1 == was.top && gone.cols_overlap(&was) {
                Edge::Top
            } else {
                return None;
            };
            Some((index, edge))
        })
    }

    /// The panes that moved over between `before` and `self` for a slide
    /// through `edge` (see `Edge::moved`). The mux keeps the size of the
    /// first of two panes and puts what changes on the second, so they move
    /// as one block, all by as much.
    fn moved(&self, before: &Self, edge: Edge, towards: bool) -> Option<Moved> {
        let mut moved: Option<Moved> = None;
        for (id, now) in &self.panes {
            let Some(was) = before.rect(*id) else {
                continue;
            };
            let Some(by) = edge.moved(was, *now, towards) else {
                continue;
            };
            moved = match moved {
                None => Some(Moved { block: *now, by }),
                Some(moved) if moved.by == by => Some(Moved {
                    block: moved.block.union(now),
                    by,
                }),
                // Not one block: drawn where they stand.
                Some(_) => return None,
            };
        }
        moved
    }
}

/// Panes moved over for a slide, as the block they make where they stand
/// now, and the cells they moved by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Moved {
    block: CellRect,
    by: usize,
}

/// A slide under way.
#[derive(Clone, Copy, Debug)]
pub struct SplitSlide {
    pub pane_id: PaneId,
    pub edge: Edge,
    /// A pane that made room for it. Until the new pane gets there, the
    /// room still looks like this one's.
    pub from_pane: PaneId,
    /// Where it ends up.
    rect: CellRect,
    /// The panes that moved over to make its room.
    moved: Option<Moved>,
    started: Instant,
    /// Frames shown with it so far.
    shown: u32,
    /// Times painted with it so far, shown or not.
    paints: u32,
}

impl SplitSlide {
    /// How much of the way it still has to go, 1 at the start and 0 on
    /// arrival; `None` once it is there.
    fn remaining(&self, now: Instant) -> Option<f32> {
        progress(self.started, self.shown, DURATION, now).map(|t| 1.0 - ease_out(t))
    }

    /// Whether `pos` moved over to make the room, and goes along.
    pub fn moves(&self, pos: &PositionedPane) -> bool {
        self.moved
            .is_some_and(|moved| moved.block.contains(&CellRect::of(pos)))
    }
}

/// A closed pane on its way out.
#[derive(Clone, Debug)]
pub struct Leaving<P = PositionedPane> {
    /// The pane as last painted, to paint it again from.
    pub pane: P,
    pub edge: Edge,
    /// Where it stood.
    rect: CellRect,
    /// The panes that moved over into its room.
    moved: Option<Moved>,
    /// Its nav bar showed its tab. One an overlay stood in for shows none.
    showed_tab: bool,
    tab_id: TabId,
    started: Instant,
    /// Frames shown with it so far.
    shown: u32,
    /// Times painted with it so far, shown or not.
    paints: u32,
}

impl<P> Leaving<P> {
    /// How far out it is, 0 at the start and 1 when gone; `None` once it is.
    fn out(&self, now: Instant) -> Option<f32> {
        progress(self.started, self.shown, LEAVE_DURATION, now).map(ease_in)
    }

    /// The divider it goes with: the one between it and the panes that have
    /// its room, which no longer has a place in the tab.
    pub fn divider(&self) -> PositionedSplit {
        self.edge.divider(self.rect)
    }

    /// Whether it stood over or under another pane, where its nav bar had
    /// a button to collapse the two.
    pub fn stood_in_a_column(&self) -> bool {
        matches!(self.edge, Edge::Top | Edge::Bottom)
    }

    /// Whether its nav bar showed its tab.
    pub fn showed_tab(&self) -> bool {
        self.showed_tab
    }
}

impl Leaving {
    /// Whether `pos` moved over into its room, and goes along.
    pub fn moves(&self, pos: &PositionedPane) -> bool {
        self.moved
            .is_some_and(|moved| moved.block.contains(&CellRect::of(pos)))
    }
}

/// How far through a slide lasting `duration` is, from 0 to 1, in the
/// frame after `shown` frames; `None` once it is over.
fn progress(started: Instant, shown: u32, duration: Duration, now: Instant) -> Option<f32> {
    let elapsed = now.saturating_duration_since(started) + FIRST_FRAME;
    let by_clock = elapsed.as_secs_f32() / duration.as_secs_f32();
    let by_frames = (shown + 1) as f32 / MIN_FRAMES as f32;
    let t = by_clock.min(by_frames);
    (t < 1.0).then_some(t)
}

/// Fast, then settling. Squared rather than cubed: cubed, the first two
/// frames covered two thirds of the way and the rest was barely seen to move.
fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(2)
}

/// Slow, then quick: `ease_out` played backwards.
fn ease_in(t: f32) -> f32 {
    t.clamp(0.0, 1.0).powi(2)
}

/// `rect` cut to `area`; nothing, at its corner, where they do not meet.
fn within(rect: RectF, area: RectF) -> RectF {
    rect.intersection(&area)
        .unwrap_or_else(|| euclid::rect(rect.min_x(), rect.min_y(), 0., 0.))
}

/// The shift along one axis, at most the whole of `place` that way.
fn clamped(place: RectF, shift: (f32, f32)) -> (f32, f32) {
    (
        shift.0.clamp(-place.width(), place.width()),
        shift.1.clamp(-place.height(), place.height()),
    )
}

/// What of `place` a pane shifted by `shift` still covers.
fn cover(place: RectF, shift: (f32, f32)) -> RectF {
    let (dx, dy) = clamped(place, shift);
    euclid::rect(
        place.min_x() + dx.max(0.),
        place.min_y() + dy.max(0.),
        place.width() - dx.abs(),
        place.height() - dy.abs(),
    )
}

/// What of `place` a pane shifted by `shift` has yet to cover.
fn wake(place: RectF, shift: (f32, f32)) -> RectF {
    let (dx, dy) = clamped(place, shift);
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

/// What of `area` lies past `rect` from `edge`: the far side of a pane on
/// a slide through `edge`, where the panes it comes up against are.
fn beyond(edge: Edge, rect: RectF, area: RectF) -> RectF {
    let (mut left, mut top, mut right, mut bottom) =
        (area.min_x(), area.min_y(), area.max_x(), area.max_y());
    match edge {
        Edge::Left => left = left.max(rect.max_x()),
        Edge::Right => right = right.min(rect.min_x()),
        Edge::Top => top = top.max(rect.max_y()),
        Edge::Bottom => bottom = bottom.min(rect.min_y()),
    }
    euclid::rect(left, top, (right - left).max(0.), (bottom - top).max(0.))
}

/// How a pane or a divider is drawn in a frame of a slide: shifted by
/// `shift` from where it stands, and cut to `clip`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drawn {
    pub shift: (f32, f32),
    pub clip: RectF,
}

/// What one frame of a slide draws otherwise than where it stands. What
/// it moves or cuts stays clear of everything else, the sliding pane
/// included: the quad layers within a z-index are one order for the whole
/// window, so of two panes drawn over one another, the text of the one
/// beneath would show through the ground of the other.
#[derive(Debug, Default)]
pub struct SlideFrame {
    /// The pane coming in.
    arriving: Option<(PaneId, Drawn)>,
    /// The divider coming in with it, by the edge it comes in from and its
    /// place.
    leading: Option<(Edge, CellRect, Drawn)>,
    /// The closed pane on its way out, and the divider going with it.
    pub leaving: Option<(Drawn, Drawn)>,
    /// The panes moved over for the slide, and the dividers between them:
    /// one block, up against the far side of the pane on its way.
    block: Option<(CellRect, Drawn)>,
    /// Where a closed pane stood: the panes and dividers that reach into
    /// it are cut short of what of it it still covers.
    room: Option<(CellRect, Drawn)>,
    /// Room no pane covers in this frame, and the pane to fill it like.
    pub fills: Vec<(PaneId, RectF)>,
}

impl SlideFrame {
    /// `slide` coming into `place`, `remaining` of the way still to go;
    /// `moving` where each pane that moved over for it stands, `cell` the
    /// size of a cell. Nothing is drawn outside `area`.
    pub fn coming(
        slide: &SplitSlide,
        remaining: f32,
        place: RectF,
        moving: &[(PaneId, RectF)],
        cell: (f32, f32),
        area: RectF,
    ) -> Self {
        let place = within(place, area);
        let shift = slide.edge.shift(remaining, place.width(), place.height());
        let arriving = Drawn { shift, clip: place };
        let mut frame = Self {
            arriving: Some((slide.pane_id, arriving)),
            leading: Some((slide.edge, slide.rect, arriving)),
            // Its room is the pane's that made it until it gets there.
            fills: vec![(slide.from_pane, wake(place, shift))],
            ..Self::default()
        };
        if let Some(pushed) = slide.moved.filter(|_| !moving.is_empty()) {
            // Up against its far side, from where they stood. They move by
            // whole cells, the room by the half cells either side of it as
            // well: what of it is left between them is filled, and nothing
            // they cover, or a see-through ground would be laid on twice.
            let back = slide.edge.shift(
                remaining,
                pushed.by as f32 * cell.0,
                pushed.by as f32 * cell.1,
            );
            let room = moving
                .iter()
                .fold(place, |room, (_, rect)| room.union(&within(*rect, area)));
            frame.block = Some((
                pushed.block,
                Drawn {
                    shift: back,
                    clip: beyond(slide.edge, cover(place, shift), room),
                },
            ));
            frame.leading = Some((slide.edge, slide.rect, Drawn { shift, clip: room }));
            frame.fills = vec![(
                slide.from_pane,
                beyond(slide.edge.opposite(), wake(place, back), wake(place, shift)),
            )];
            // Behind them, room that is theirs and they are not in yet, along
            // the block's far side: one further in has another of them there.
            let far = slide.edge.far_side(room);
            frame.fills.extend(
                moving
                    .iter()
                    .map(|(pane_id, rect)| (*pane_id, within(*rect, area)))
                    .filter(|(_, rect)| (slide.edge.far_side(*rect) - far).abs() < 0.5)
                    .map(|(pane_id, rect)| (pane_id, wake(rect, back))),
            );
        }
        frame
    }

    /// `leaving` going out of `place`, `out` of the way gone; `moving` where
    /// each pane that moved over into its room stands, `cell` the size of a
    /// cell. Nothing is drawn outside `area`.
    pub fn going<P>(
        leaving: &Leaving<P>,
        out: f32,
        place: RectF,
        moving: &[(PaneId, RectF)],
        cell: (f32, f32),
        area: RectF,
    ) -> Self {
        let place = within(place, area);
        let shift = leaving.edge.shift(out, place.width(), place.height());
        let covered = cover(place, shift);
        let mut frame = Self {
            // Its divider runs along its side, just outside its place.
            leaving: Some((Drawn { shift, clip: place }, Drawn { shift, clip: area })),
            ..Self::default()
        };
        match leaving.moved.filter(|_| !moving.is_empty()) {
            // From where they stood, up against it.
            Some(pushed) => {
                let (dx, dy) = leaving.edge.shift(
                    1.0 - out,
                    pushed.by as f32 * cell.0,
                    pushed.by as f32 * cell.1,
                );
                let room = moving
                    .iter()
                    .fold(place, |room, (_, rect)| room.union(&within(*rect, area)));
                frame.block = Some((
                    pushed.block,
                    Drawn {
                        shift: (-dx, -dy),
                        clip: beyond(leaving.edge, covered, room),
                    },
                ));
            }
            // Where they stand, cut short of it.
            None => {
                frame.room = Some((
                    leaving.rect,
                    Drawn {
                        shift: (0., 0.),
                        clip: beyond(leaving.edge, covered, area),
                    },
                ));
            }
        }
        frame
    }

    /// How the frame draws `pos`, where not as it stands.
    pub fn pane(&self, pos: &PositionedPane) -> Option<Drawn> {
        self.pane_at(pos.pane.pane_id(), CellRect::of(pos))
    }

    fn pane_at(&self, pane_id: PaneId, rect: CellRect) -> Option<Drawn> {
        match self.arriving {
            Some((arriving, drawn)) if arriving == pane_id => Some(drawn),
            _ => self.moved_at(rect),
        }
    }

    /// How the frame draws `split`, where not as it stands.
    pub fn split(&self, split: &PositionedSplit) -> Option<Drawn> {
        match self.leading {
            Some((edge, pane, drawn)) if edge.leads(pane, split) => Some(drawn),
            _ => self.moved_at(CellRect::of_split(split)),
        }
    }

    fn moved_at(&self, rect: CellRect) -> Option<Drawn> {
        match (self.block, self.room) {
            (Some((block, drawn)), _) if block.contains(&rect) => Some(drawn),
            (_, Some((room, drawn))) if room.overlaps(&rect) => Some(drawn),
            _ => None,
        }
    }
}

/// Last frame's layout and panes, and the slide under way.
pub struct SplitSlides<P = PositionedPane> {
    last: Option<TabLayout>,
    /// The panes `last` was made of, in its order. A pane found closed is
    /// painted from here on its way out; held no longer than a frame.
    last_panes: Vec<P>,
    /// Swapped with `last_panes` each frame, so neither is allocated anew.
    spare_panes: Vec<P>,
    coming: Option<SplitSlide>,
    going: Option<Leaving<P>>,
    /// The panes whose nav bar the frame painted showed no tab, as one an
    /// overlay stands in for does: one of them that closes goes out so.
    bare_navs: Vec<PaneId>,
}

impl<P> Default for SplitSlides<P> {
    fn default() -> Self {
        Self {
            last: None,
            last_panes: vec![],
            spare_panes: vec![],
            coming: None,
            going: None,
            bare_navs: vec![],
        }
    }
}

/// A frame's slides, each with how far out of its place it is drawn.
pub struct Slides<P = PositionedPane> {
    pub coming: Option<(SplitSlide, f32)>,
    pub going: Option<(Leaving<P>, f32)>,
}

impl<P: Clone> SplitSlides<P> {
    /// Take this frame's layout and the panes it was made of, in its order;
    /// the slides to paint the frame with. A frame painted again -- the
    /// quads ran out, a font came in -- takes the same layout again, and
    /// gets the same slides.
    pub fn advance(
        &mut self,
        layout: Option<TabLayout>,
        panes: &[P],
        enabled: bool,
        now: Instant,
    ) -> Slides<P> {
        let before = std::mem::replace(&mut self.last, layout);
        std::mem::swap(&mut self.last_panes, &mut self.spare_panes);
        self.last_panes.clear();
        if self
            .last
            .as_ref()
            .is_some_and(|layout| layout.panes.len() == panes.len())
        {
            self.last_panes.extend_from_slice(panes);
        }
        if !enabled {
            self.coming = None;
            self.going = None;
        } else if let (Some(before), Some(layout)) = (before.as_ref(), self.last.as_ref()) {
            let opened = layout.opened_by_split(before).and_then(|(pane_id, edge, from)| {
                Some((pane_id, edge, from, layout.rect(pane_id)?))
            });
            if let Some((pane_id, edge, from_pane, rect)) = opened {
                // One slide at a time: the panes a slide draws elsewhere are
                // laid out for it alone. One still under way is cut short.
                self.going = None;
                self.coming = Some(SplitSlide {
                    pane_id,
                    edge,
                    from_pane,
                    rect,
                    moved: layout.moved(before, edge, false),
                    started: now,
                    shown: 0,
                    paints: 0,
                });
            }
            if let Some((index, edge)) = layout.closed(before) {
                let (pane_id, rect) = before.panes[index];
                // Closed before it was all the way in: it has no place of
                // its own to go out of.
                let arriving = self.coming.is_some_and(|slide| slide.pane_id == pane_id);
                if let Some(pane) = self.spare_panes.get(index).filter(|_| !arriving) {
                    self.coming = None;
                    self.going = Some(Leaving {
                        pane: pane.clone(),
                        edge,
                        rect,
                        moved: layout.moved(before, edge, true),
                        showed_tab: !self.bare_navs.contains(&pane_id),
                        tab_id: before.tab_id,
                        started: now,
                        shown: 0,
                        paints: 0,
                    });
                }
            }
        }
        // Last frame's panes have been looked through: let go of them, a
        // closed one among them.
        self.spare_panes.clear();
        self.bare_navs.clear();
        Slides {
            coming: self.advance_coming(now),
            going: self.advance_going(now),
        }
    }

    /// The frame being painted shows `pane_id`'s nav bar without a tab.
    pub fn bare_nav(&mut self, pane_id: PaneId) {
        self.bare_navs.push(pane_id);
    }

    /// The frame painted with the slides was shown: they are a frame on.
    /// Counted here rather than in `advance`, which a frame painted again
    /// calls again.
    pub fn shown(&mut self) {
        if let Some(slide) = self.coming.as_mut() {
            slide.shown += 1;
        }
        if let Some(leaving) = self.going.as_mut() {
            leaving.shown += 1;
        }
    }

    fn advance_coming(&mut self, now: Instant) -> Option<(SplitSlide, f32)> {
        let slide = self.coming.as_mut()?;
        slide.paints += 1;
        // Gone from the tab, or the tab gone from the window: nothing left
        // to slide.
        let rect = self
            .last
            .as_ref()
            .filter(|layout| !layout.zoomed && slide.paints <= MOST_PAINTS)
            .and_then(|layout| layout.rect(slide.pane_id));
        match rect.and_then(|rect| Some((rect, slide.remaining(now)?))) {
            Some((rect, remaining)) => {
                slide.rect = rect;
                Some((*slide, remaining))
            }
            None => {
                self.coming = None;
                None
            }
        }
    }

    fn advance_going(&mut self, now: Instant) -> Option<(Leaving<P>, f32)> {
        let leaving = self.going.as_mut()?;
        leaving.paints += 1;
        // Another tab, or a pane zoomed over its room: nothing to go out of.
        let on_screen = leaving.paints <= MOST_PAINTS
            && self
                .last
                .as_ref()
                .is_some_and(|layout| layout.tab_id == leaving.tab_id && !layout.zoomed);
        match leaving.out(now).filter(|_| on_screen) {
            Some(out) => Some((leaving.clone(), out)),
            None => {
                self.going = None;
                None
            }
        }
    }

    /// Let go of every pane held, and of the layout last painted. A window
    /// that paints no frames would keep them until it painted one, a closed
    /// pane among them alive, and then take whatever changed meanwhile for a
    /// split or a close to slide.
    pub fn release(&mut self) {
        self.last = None;
        self.last_panes.clear();
        self.coming = None;
        self.going = None;
        self.bare_navs.clear();
    }
}

/// What the nav bar of a closed pane on its way out showed, which the tab
/// and the mux it has left no longer say.
#[derive(Clone)]
pub struct ClosedNav {
    pub pane: Arc<dyn Pane>,
    /// It showed a tab of its own.
    pub tab: bool,
    /// It had the button that collapses it and the pane over or under it.
    pub collapsible: bool,
}

impl ClosedNav {
    /// The closed pane, if it is `pane_id`.
    pub fn pane(&self, pane_id: PaneId) -> Option<&Arc<dyn Pane>> {
        (self.pane.pane_id() == pane_id).then_some(&self.pane)
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

    fn cells([left, top, width, height]: [usize; 4]) -> CellRect {
        CellRect {
            left,
            top,
            width,
            height,
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
        // Squeezed on the side away from it, they stay where they were.
        assert_eq!(three.moved(&two, Edge::Right, false), None);
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

    fn one() -> Option<TabLayout> {
        Some(layout(1, &[(1, [0, 0, 81, 24])]))
    }

    fn two() -> Option<TabLayout> {
        Some(layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]))
    }

    /// The remaining way of each frame shown with the slide, a frame every
    /// `frame` from the split on, each painted `passes` times.
    fn run(frame: Duration, passes: usize) -> Vec<f32> {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        assert!(slides.advance(one(), &[1], true, start).coming.is_none());
        let mut steps = vec![];
        let mut now = start;
        loop {
            let mut painted = None;
            for _ in 0..passes {
                painted = slides.advance(two(), &[1, 2], true, now).coming;
            }
            let Some((slide, remaining)) = painted else {
                break;
            };
            slides.shown();
            assert_eq!((slide.pane_id, slide.edge, slide.from_pane), (2, Edge::Right, 1));
            steps.push(remaining);
            now += frame;
            assert!(steps.len() < 100);
        }
        // And it does not start again.
        assert!(slides.advance(two(), &[1, 2], true, now + frame).coming.is_none());
        steps
    }

    /// How far out the closed pane is drawn in each frame shown from its
    /// closing on, a frame every `frame`, each painted `passes` times.
    fn run_close(frame: Duration, passes: usize) -> Vec<f32> {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        assert!(slides.advance(two(), &[1, 2], true, start).going.is_none());
        let mut steps = vec![];
        let mut now = start;
        loop {
            let mut painted = None;
            for _ in 0..passes {
                painted = slides.advance(one(), &[1], true, now).going;
            }
            let Some((leaving, out)) = painted else {
                break;
            };
            slides.shown();
            assert_eq!((leaving.pane, leaving.edge), (2, Edge::Right));
            steps.push(out);
            now += frame;
            assert!(steps.len() < 100);
        }
        assert!(slides.advance(one(), &[1], true, now + frame).going.is_none());
        steps
    }

    #[test]
    fn a_slide_starts_on_the_split_and_settles_into_place() {
        let steps = run(Duration::from_micros(8_333), 1);
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
        assert_eq!(Edge::Right.shift(0.5, 300., 200.), (150., 0.));
    }

    #[test]
    fn slow_frames_slow_the_slide_rather_than_skip_it() {
        let steps = run(Duration::from_millis(60), 1);
        assert!(steps.len() as u32 >= MIN_FRAMES - 1, "{steps:?}");
        assert!(steps.windows(2).all(|w| w[1] < w[0]), "{steps:?}");
        let steps = run_close(Duration::from_millis(60), 1);
        assert!(steps.len() as u32 >= MIN_FRAMES - 1, "{steps:?}");
        assert!(steps.windows(2).all(|w| w[1] > w[0]), "{steps:?}");
    }

    #[test]
    fn a_frame_painted_again_is_not_a_frame_further_on() {
        // Slow frames, each painted twice: as many frames shown as painted
        // once, and from as near the start.
        let once = run(Duration::from_millis(60), 1);
        let twice = run(Duration::from_millis(60), 2);
        assert_eq!(twice.len(), once.len(), "{twice:?} {once:?}");
        assert!((twice[0] - once[0]).abs() < 0.01, "{twice:?} {once:?}");
        let once = run_close(Duration::from_millis(60), 1);
        let twice = run_close(Duration::from_millis(60), 2);
        assert_eq!(twice.len(), once.len(), "{twice:?} {once:?}");
        assert!(twice[0] < 0.05, "{twice:?}");
    }

    #[test]
    fn with_the_setting_off_nothing_slides() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        assert!(slides.advance(one(), &[1], false, start).coming.is_none());
        assert!(slides.advance(two(), &[1, 2], false, start).coming.is_none());
        assert!(slides.advance(one(), &[1], false, start).going.is_none());
    }

    #[test]
    fn a_pane_closed_mid_slide_stops_it() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(one(), &[1], true, start);
        assert!(slides.advance(two(), &[1, 2], true, start).coming.is_some());
        let closed = slides.advance(one(), &[1], true, start + Duration::from_millis(10));
        assert!(closed.coming.is_none());
        // Nor does it go out of a place it never got to.
        assert!(closed.going.is_none());
    }

    #[test]
    fn one_slide_at_a_time() {
        let start = Instant::now();
        // The right of two split down, then the left one closed: the column
        // takes its room.
        let three = layout(
            1,
            &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 12]), (3, [41, 13, 40, 11])],
        );
        let column = layout(1, &[(2, [0, 0, 81, 12]), (3, [0, 13, 81, 11])]);
        let split = layout(
            1,
            &[(2, [0, 0, 81, 12]), (3, [0, 13, 40, 11]), (4, [41, 13, 40, 11])],
        );
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], true, start);
        assert!(slides.advance(Some(three), &[1, 2, 3], true, start).coming.is_some());
        // Another pane closed while one comes in: that one is in at once.
        let closing = slides.advance(Some(column), &[2, 3], true, start);
        assert!(closing.coming.is_none());
        assert_eq!(closing.going.map(|(leaving, _)| leaving.pane), Some(1));
        // And a split while one goes out.
        let splitting = slides.advance(Some(split), &[2, 3, 4], true, start);
        assert!(splitting.going.is_none());
        assert_eq!(splitting.coming.map(|(slide, _)| slide.pane_id), Some(4));
    }

    /// The closed pane and the edge it goes out by, between two layouts.
    fn closed(after: &TabLayout, before: &TabLayout) -> Option<(PaneId, Edge)> {
        after
            .closed(before)
            .map(|(index, edge)| (before.panes[index].0, edge))
    }

    #[test]
    fn a_closed_pane_goes_out_away_from_the_panes_that_take_its_room() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        let right = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(closed(&one, &right), Some((2, Edge::Right)));
        let left = layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]);
        assert_eq!(closed(&one, &left), Some((2, Edge::Left)));
        let below = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        assert_eq!(closed(&one, &below), Some((2, Edge::Bottom)));
        let above = layout(1, &[(2, [0, 0, 81, 11]), (1, [0, 12, 81, 12])]);
        assert_eq!(closed(&one, &above), Some((2, Edge::Top)));
        // A column of two takes the room of the pane beside it.
        let three = layout(
            1,
            &[(1, [0, 0, 40, 12]), (2, [0, 13, 40, 11]), (3, [41, 0, 40, 24])],
        );
        let column = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        assert_eq!(closed(&column, &three), Some((3, Edge::Right)));
    }

    #[test]
    fn a_pane_that_was_not_closed_does_not_go_out() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        // Another tab with one pane.
        assert_eq!(closed(&layout(2, &one.panes_spec()), &two), None);
        // A zoom.
        let zoomed = TabLayout {
            zoomed: true,
            ..layout(1, &[(1, [0, 0, 81, 24])])
        };
        assert_eq!(closed(&zoomed, &two), None);
        // An overlay opened over a pane.
        let overlay = layout(1, &[(1, [0, 0, 40, 24]), (9, [41, 0, 40, 24])]);
        assert_eq!(closed(&overlay, &two), None);
        // Its neighbour has not taken the room yet.
        let stale = layout(1, &[(1, [0, 0, 40, 24])]);
        assert_eq!(closed(&stale, &two), None);
        // Two gone at once.
        let three = layout(
            1,
            &[(1, [0, 0, 27, 24]), (2, [28, 0, 26, 24]), (3, [55, 0, 26, 24])],
        );
        assert_eq!(closed(&one, &three), None);
    }

    #[test]
    fn a_closed_pane_goes_out_the_way_it_came_and_sooner() {
        let steps = run_close(Duration::from_micros(8_333), 1);
        // Where it stood on the first frame, on its way after, never back.
        assert!(steps[0] < 0.05, "{steps:?}");
        assert!(steps.windows(2).all(|w| w[1] > w[0]), "{steps:?}");
        assert!(steps.len() as u32 >= MIN_FRAMES, "{steps:?}");
        let coming = run(Duration::from_micros(8_333), 1);
        assert!(steps.len() < coming.len(), "{steps:?} {coming:?}");
    }

    #[test]
    fn a_closed_pane_goes_with_the_divider_its_split_put_in() {
        let pane = CellRect {
            left: 41,
            top: 13,
            width: 40,
            height: 11,
        };
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            assert!(edge.leads(pane, &edge.divider(pane)), "{edge:?}");
        }
        assert_eq!(
            Edge::Right.divider(pane),
            PositionedSplit {
                index: 0,
                direction: SplitDirection::Horizontal,
                left: 40,
                top: 13,
                size: 11,
            }
        );
    }

    #[test]
    fn a_window_that_lets_go_has_no_closed_pane_to_paint() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], true, start);
        slides.release();
        assert!(slides.advance(one(), &[1], true, start).going.is_none());
        // Nor takes what was split meanwhile for a split to slide in.
        slides.release();
        assert!(slides.advance(two(), &[1, 2], true, start).coming.is_none());
        // Nor a tab switched to mid-way out.
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], true, start);
        assert!(slides.advance(one(), &[1], true, start).going.is_some());
        let other = Some(layout(2, &[(5, [0, 0, 81, 24])]));
        assert!(slides.advance(other, &[5], true, start).going.is_none());
    }

    #[test]
    fn a_pane_whose_nav_bar_showed_no_tab_goes_out_without_one() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], true, start);
        // An overlay stood in for it, asking whether to close it.
        slides.bare_nav(2);
        let closing = slides.advance(one(), &[1], true, start);
        assert_eq!(closing.going.map(|(leaving, _)| leaving.showed_tab()), Some(false));
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], true, start);
        slides.bare_nav(1);
        let closing = slides.advance(one(), &[1], true, start);
        assert_eq!(closing.going.map(|(leaving, _)| leaving.showed_tab()), Some(true));
    }

    #[test]
    fn a_slide_whose_frames_are_never_shown_still_ends() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(one(), &[1], true, start);
        let mut now = start;
        let mut paints = 0;
        while slides.advance(two(), &[1, 2], true, now).coming.is_some() {
            paints += 1;
            now += Duration::from_millis(16);
            assert!(paints <= MOST_PAINTS, "{paints}");
        }
        assert_eq!(paints, MOST_PAINTS);
        // Only frames that are not shown run into that: a slow one is
        // still shown in whole.
        let steps = run(Duration::from_millis(1500), 1);
        assert!(steps.len() as u32 >= MIN_FRAMES - 1, "{steps:?}");
    }

    #[test]
    fn panes_that_move_over_for_a_slide_are_one_block() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        // Room made on the left or the top moves the pane that made it.
        let left = layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]);
        assert_eq!(
            left.moved(&one, Edge::Left, false),
            Some(Moved {
                block: cells([41, 0, 40, 24]),
                by: 41
            })
        );
        let above = layout(1, &[(2, [0, 0, 81, 11]), (1, [0, 12, 81, 12])]);
        assert_eq!(
            above.moved(&one, Edge::Top, false),
            Some(Moved {
                block: cells([0, 12, 81, 12]),
                by: 12
            })
        );
        // On the right or the bottom it only changes its size.
        let right = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(right.moved(&one, Edge::Right, false), None);
        // Room taken back moves the panes that take it, a column of them
        // together.
        assert_eq!(
            one.moved(&left, Edge::Left, true),
            Some(Moved {
                block: cells([0, 0, 81, 24]),
                by: 41
            })
        );
        let three = layout(
            1,
            &[(3, [0, 0, 40, 24]), (1, [41, 0, 40, 12]), (2, [41, 13, 40, 11])],
        );
        let column = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        assert_eq!(
            column.moved(&three, Edge::Left, true),
            Some(Moved {
                block: cells([0, 0, 81, 24]),
                by: 41
            })
        );
        // Panes that moved by different amounts are not one block.
        let apart = layout(1, &[(1, [0, 0, 30, 24]), (2, [31, 0, 50, 24])]);
        let wider = layout(1, &[(1, [10, 0, 30, 24]), (2, [45, 0, 36, 24])]);
        assert_eq!(wider.moved(&apart, Edge::Left, false), None);
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> RectF {
        euclid::rect(x, y, w, h)
    }

    fn slide(edge: Edge, rect: CellRect, moved: Option<Moved>) -> SplitSlide {
        SplitSlide {
            pane_id: 2,
            edge,
            from_pane: 1,
            rect,
            moved,
            started: Instant::now(),
            shown: 0,
            paints: 0,
        }
    }

    fn leaving(edge: Edge, rect: CellRect, moved: Option<Moved>) -> Leaving<PaneId> {
        Leaving {
            pane: 2,
            edge,
            rect,
            moved,
            showed_tab: true,
            tab_id: 1,
            started: Instant::now(),
            shown: 0,
            paints: 0,
        }
    }

    const CELL: (f32, f32) = (10., 20.);

    /// The terminal's part of the window, a sidebar 100px wide on its left.
    fn area() -> RectF {
        rect(100., 0., 810., 500.)
    }

    #[test]
    fn a_pane_coming_in_is_drawn_inside_its_place_with_its_room_filled() {
        // Split to the right: the new pane's place is 400px wide.
        let frame = SlideFrame::coming(
            &slide(Edge::Right, cells([41, 0, 40, 24]), None),
            0.75,
            rect(505., 0., 405., 500.),
            &[],
            CELL,
            area(),
        );
        let drawn = frame.pane_at(2, cells([41, 0, 40, 24])).unwrap();
        assert_eq!(drawn.shift, (303.75, 0.));
        assert_eq!(drawn.clip, rect(505., 0., 405., 500.));
        // The pane that made the room keeps it looking its own, and stands
        // where it is.
        assert_eq!(frame.fills, vec![(1, rect(505., 0., 303.75, 500.))]);
        assert_eq!(frame.pane_at(1, cells([0, 0, 40, 24])), None);
        // Along the window's left edge, a place reaches under the sidebar,
        // which nothing is drawn over.
        let frame = SlideFrame::coming(
            &slide(Edge::Left, cells([0, 0, 40, 24]), None),
            0.5,
            rect(0., 0., 505., 500.),
            &[],
            CELL,
            area(),
        );
        let drawn = frame.pane_at(2, cells([0, 0, 40, 24])).unwrap();
        assert_eq!(drawn.clip, rect(100., 0., 405., 500.));
        assert_eq!(drawn.shift, (-202.5, 0.));
    }

    #[test]
    fn panes_moved_over_by_a_split_go_along_from_where_they_stood() {
        // Split to the left of a pane along the sidebar: it moves 41 cells
        // over, and is drawn back from there all the way at the start.
        let pushed = Moved {
            block: cells([41, 0, 40, 24]),
            by: 41,
        };
        let place = rect(0., 0., 505., 500.);
        let theirs = rect(505., 0., 405., 500.);
        let at = |remaining| {
            SlideFrame::coming(
                &slide(Edge::Left, cells([0, 0, 40, 24]), Some(pushed)),
                remaining,
                place,
                &[(1, theirs)],
                CELL,
                area(),
            )
        };
        let start = at(1.0).pane_at(1, cells([41, 0, 40, 24])).unwrap();
        assert_eq!(start.shift, (-410., 0.));
        let frame = at(0.5);
        let drawn = frame.pane_at(1, cells([41, 0, 40, 24])).unwrap();
        assert_eq!(drawn.shift, (-205., 0.));
        // Up against the new pane, which has 202.5px still to come in.
        assert_eq!(drawn.clip, rect(302.5, 0., 607.5, 500.));
        // Behind it, its room it is not in yet, and nothing it covers: a
        // see-through ground laid on twice comes out darker.
        assert!(frame.fills.contains(&(1, rect(705., 0., 205., 500.))));
        let covered = within(theirs.translate(euclid::vec2(drawn.shift.0, 0.)), drawn.clip);
        for (_, fill) in &frame.fills {
            assert!(fill.intersection(&covered).is_none(), "{fill:?} {covered:?}");
        }
        // The divider between them goes with both.
        let divider = Edge::Left.divider(cells([0, 0, 40, 24]));
        assert_eq!(frame.split(&divider).unwrap().shift, (-202.5, 0.));
        // Where whole cells come up short of the room, what is left between
        // them is filled like the pane it was.
        let short = SlideFrame::coming(
            &slide(
                Edge::Left,
                cells([0, 0, 40, 24]),
                Some(Moved {
                    block: cells([41, 0, 40, 24]),
                    by: 40,
                }),
            ),
            0.5,
            place,
            &[(1, theirs)],
            CELL,
            area(),
        );
        assert_eq!(short.fills[0], (1, rect(302.5, 0., 2.5, 500.)));
    }

    #[test]
    fn a_closed_pane_goes_out_inside_its_place_over_room_cut_short_of_it() {
        // The right of two goes: the left one stands in all the room,
        // cut where the closed pane still is.
        let frame = SlideFrame::going(
            &leaving(Edge::Right, cells([41, 0, 40, 24]), None),
            0.25,
            rect(505., 0., 405., 500.),
            &[],
            CELL,
            area(),
        );
        let (pane, divider) = frame.leaving.unwrap();
        assert_eq!(pane.shift, (101.25, 0.));
        assert_eq!(pane.clip, rect(505., 0., 405., 500.));
        assert_eq!(divider.shift, (101.25, 0.));
        let left = frame.pane_at(1, cells([0, 0, 81, 24])).unwrap();
        assert_eq!(left.shift, (0., 0.));
        assert_eq!(left.clip, rect(100., 0., 506.25, 500.));
        // A pane beside neither stands as it is.
        assert_eq!(frame.pane_at(3, cells([82, 0, 10, 24])), None);
        // Nor does it draw itself under the sidebar.
        let frame = SlideFrame::going(
            &leaving(Edge::Left, cells([0, 0, 40, 24]), None),
            0.0,
            rect(0., 0., 505., 500.),
            &[],
            CELL,
            area(),
        );
        assert_eq!(frame.leaving.unwrap().0.clip, rect(100., 0., 405., 500.));
    }

    #[test]
    fn panes_moved_over_into_a_closed_panes_room_come_from_where_they_stood() {
        // The left of two goes: the right one takes its room, 41 cells
        // over, drawn back there at first and up against it after.
        let pushed = Moved {
            block: cells([0, 0, 81, 24]),
            by: 41,
        };
        let at = |out| {
            SlideFrame::going(
                &leaving(Edge::Left, cells([0, 0, 40, 24]), Some(pushed)),
                out,
                rect(0., 0., 505., 500.),
                &[(1, rect(0., 0., 910., 500.))],
                CELL,
                area(),
            )
        };
        let start = at(0.0).pane_at(1, cells([0, 0, 81, 24])).unwrap();
        assert_eq!(start.shift, (410., 0.));
        let frame = at(0.5);
        let drawn = frame.pane_at(1, cells([0, 0, 81, 24])).unwrap();
        assert_eq!(drawn.shift, (205., 0.));
        assert_eq!(drawn.clip, rect(302.5, 0., 607.5, 500.));
        let end = at(1.0).pane_at(1, cells([0, 0, 81, 24])).unwrap();
        assert_eq!(end.shift, (0., 0.));
    }

    #[test]
    fn dividers_reaching_into_a_closed_panes_room_are_cut_short_of_it() {
        // A column of two takes the room of the pane on its right: the
        // divider between them runs on into it.
        let frame = SlideFrame::going(
            &leaving(Edge::Right, cells([41, 0, 40, 24]), None),
            0.5,
            rect(505., 0., 405., 500.),
            &[],
            CELL,
            area(),
        );
        let across = PositionedSplit {
            index: 1,
            direction: SplitDirection::Vertical,
            left: 0,
            top: 12,
            size: 81,
        };
        assert_eq!(frame.split(&across).unwrap().clip.max_x(), 505. + 202.5);
        // One beside it is not.
        let beside = PositionedSplit {
            index: 1,
            direction: SplitDirection::Horizontal,
            left: 81,
            top: 0,
            size: 24,
        };
        assert_eq!(frame.split(&beside), None);
    }
}
