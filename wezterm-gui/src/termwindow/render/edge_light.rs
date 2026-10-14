//! The light that runs round copied and pasted text.
//!
//! A copy lights up the selection it took. A paste lights up the text it put
//! in, once the program has echoed it at the cursor where the paste went --
//! text that never shows up there (a password prompt, an editor in a mode
//! that takes keys as commands) gets no light rather than a wrong one.
//! Either way the light follows the rounded outline the selection is drawn
//! with (`selection_shape`), and the setting picks white, colour or none.
//! Two lights run along its top and bottom from where the selection was
//! begun to its other end: a selection made backwards (dragged right to
//! left, or up) lights from its end.

use crate::customglyph::{BlockAlpha, BlockCoord, Poly, PolyCommand, PolyStyle};
use crate::quad::{QuadTrait, TripleLayerQuadAllocator};
use crate::termwindow::render::selection_shape::{corner_radius, selected_span, Outline};
use crate::termwindow::TermWindowNotif;
use ::window::WindowOps;
use mux::pane::{Pane, PaneId};
use mux::renderable::StableCursorPosition;
use std::convert::TryFrom;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::color::LinearRgba;
use termwiz::surface::Line;
use wezterm_term::StableRowIndex;

/// One light goes round, at this speed in UI points a second: a longer
/// outline takes longer, within these bounds.
const SPEED: f32 = 700.;
const MIN_DURATION: Duration = Duration::from_millis(700);
const MAX_DURATION: Duration = Duration::from_millis(1600);
/// The fade at the end, the light's and a pasted block's.
const FADE: Duration = Duration::from_millis(250);
/// The longest a light's tail gets, in UI points.
const MAX_TAIL: f32 = 110.;
const LINE_WIDTH: f32 = 2.;
/// Any gap between the text's block and the line the light runs on, in UI
/// points. None: the line hugs the block from outside, its inner edge on
/// the block's edge.
const OUTSET: f32 = 0.;
/// How long a paste has to show up before its light is given up.
const PASTE_WAIT: Duration = Duration::from_millis(1000);
/// Longer pastes are not looked for on screen.
const PASTE_MAX_CHARS: usize = 4000;
/// Lights running at once, at most; another replaces the oldest.
const MAX_LIGHTS: usize = 8;
/// The light is a run of round dots, so it bends round the outline's
/// corners, and its halo with it, instead of squaring them off.
const DOT: &[Poly] = &[Poly {
    path: &[PolyCommand::Circle {
        center: (BlockCoord::Frac(1, 2), BlockCoord::Frac(1, 2)),
        radius: BlockCoord::Frac(1, 2),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

/// One light, on rows of one pane.
pub struct EdgeLight {
    pane_id: PaneId,
    started: Instant,
    first_row: StableRowIndex,
    /// What each row from `first_row` covers, in cells; a selection's run on
    /// to the end of the row, cut to the columns drawn when painted.
    spans: Vec<Range<usize>>,
    /// The selection runs on above the first of `spans` and below the last,
    /// off the screen it was copied on.
    continues: (bool, bool),
    colored: bool,
    /// It runs from the end back to the start: the selection was made
    /// backwards.
    reversed: bool,
    /// The text's block fades out with the light: a paste's, or a selection
    /// that was let go as it was copied.
    fill: bool,
    /// How long it takes, from the length of its outline when first
    /// painted; until then, the longest it could.
    duration: Option<Duration>,
}

impl EdgeLight {
    fn duration(&self) -> Duration {
        self.duration.unwrap_or(MAX_DURATION)
    }

    /// Still on its way round, not yet fading.
    fn running(&self, now: Instant) -> bool {
        now.duration_since(self.started) < self.duration()
    }

    fn finished(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= self.duration() + FADE
    }
}

/// Where a pane's rows are drawn, for the lights on them.
pub struct LightArea {
    /// The rows drawn, the first with its top at `origin`.
    pub rows: Range<StableRowIndex>,
    pub origin: (f32, f32),
    pub cell: (f32, f32),
    /// The columns drawn.
    pub cols: usize,
    /// The pane's edges, as left, top, right, bottom: where a light would
    /// run past one, it runs just inside it.
    pub bounds: (f32, f32, f32, f32),
    /// Those of the rows drawn of double width (or height), whose cells
    /// are twice as wide.
    pub double_width: Vec<StableRowIndex>,
    /// The selection is drawn with rounded corners, and so is the light.
    pub rounded: bool,
}

/// A paste whose text is still to appear on screen.
pub struct PendingPaste {
    pane_id: PaneId,
    asked: Instant,
    row: StableRowIndex,
    col: usize,
    text: String,
    colored: bool,
}

/// Whether the light is coloured, or `None` when it is off.
fn light_setting() -> Option<bool> {
    use crate::native_settings::NativeCopyEffect;
    match crate::native_settings::copy_effect() {
        NativeCopyEffect::Off => None,
        NativeCopyEffect::White => Some(false),
        NativeCopyEffect::Color => Some(true),
    }
}

/// Where `text` ends if it is what the cells from `col` of the first of
/// `lines` read (running on across wrapped rows): the line it ends on and
/// the column just past it. `None` while they do not read it (yet).
pub fn pasted_extent(lines: &[&Line], col: usize, text: &str) -> Option<(usize, usize)> {
    let mut matched = 0;
    for (index, line) in lines.iter().enumerate() {
        let from = if index == 0 { col } else { 0 };
        for cell in line.visible_cells().filter(|cell| cell.cell_index() >= from) {
            let piece = cell.str();
            if !text[matched..].starts_with(piece) {
                return None;
            }
            matched += piece.len();
            if matched == text.len() {
                return Some((index, cell.cell_index() + cell.width()));
            }
        }
        if !line.last_cell_was_wrapped() {
            return None;
        }
    }
    None
}

/// The rows a paste from `col` to `end` (line, column) covers, as spans.
fn pasted_spans(col: usize, end: (usize, usize), cols: usize) -> Vec<Range<usize>> {
    (0..=end.0)
        .map(|row| {
            let start = if row == 0 { col } else { 0 };
            let stop = if row == end.0 { end.1 } else { cols };
            start..stop.max(start)
        })
        .collect()
}

/// The alpha for each of the dots `step` apart, `size` across, that pile
/// up to `alpha` where they overlap: about `size / step` cover any point.
fn overlapped_alpha(alpha: f32, step: f32, size: f32) -> f32 {
    let alpha = alpha.clamp(0., 1.);
    if alpha >= 1. {
        return 1.;
    }
    1. - (1. - alpha).powf(step / size.max(step))
}

/// How one light (or two) is drawn on an outline this frame.
struct LightStyle {
    /// How far through its run, eased.
    progress: f32,
    fade: f32,
    colored: bool,
    reversed: bool,
    light_background: bool,
}

/// The two lights on an outline `total` long, `progress` of the way through
/// their run down both sides from its start to `meet` (`reversed`, from
/// `meet` back to the start): each as (where its head is, which way it runs,
/// the point its tail may not reach back past), and how far the longer side
/// goes.
fn light_heads(total: f32, meet: f32, progress: f32, reversed: bool) -> ([(f32, f32, f32); 2], f32) {
    let heads = if reversed {
        [(meet - progress * meet, -1., meet), (meet + progress * (total - meet), 1., meet)]
    } else {
        [(progress * meet, 1., 0.), (total - progress * (total - meet), -1., total)]
    };
    (heads, meet.max(total - meet))
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4. * t * t * t
    } else {
        1. - (-2. * t + 2.).powi(3) / 2.
    }
}

/// The coloured light's colours, evenly along it left to right: blue,
/// violet, pink, amber.
const COLOR_STOPS: [(u8, u8, u8); 4] = [
    (0x5a, 0xa0, 0xff),
    (0xb9, 0x6e, 0xff),
    (0xff, 0x6e, 0xaa),
    (0xff, 0xb4, 0x5a),
];

/// The light's colours, converted once for all of a frame's dots: the
/// coloured light's, and the plain one's on a dark and a light background.
struct LightColors {
    stops: [LinearRgba; 4],
    white: LinearRgba,
    graphite: LinearRgba,
}

impl LightColors {
    fn new() -> Self {
        Self {
            stops: COLOR_STOPS.map(|(r, g, b)| LinearRgba::with_srgba(r, g, b, 255)),
            white: LinearRgba::with_srgba(255, 255, 255, 255),
            graphite: LinearRgba::with_srgba(72, 72, 78, 255),
        }
    }
}

/// The colour along the coloured light, `across` from its left (0) to its
/// right (1). The plain light is white, or graphite on a light background,
/// where white outside the block would not show.
fn light_color(colors: &LightColors, colored: bool, across: f32, light_background: bool) -> LinearRgba {
    if !colored {
        return if light_background {
            colors.graphite
        } else {
            colors.white
        };
    }
    let stops = &colors.stops;
    let at = across.clamp(0., 1.) * (stops.len() - 1) as f32;
    let index = (at.floor() as usize).min(stops.len() - 2);
    let t = at - index as f32;
    let (a, b) = (stops[index], stops[index + 1]);
    LinearRgba::with_components(
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
        1.,
    )
}

impl crate::TermWindow {
    fn add_edge_light(&mut self, light: EdgeLight) {
        let now = Instant::now();
        // The same rows lit again while their light is on its way round --
        // a drag let go, then copied with the keys -- keep that light, with
        // the block if this one has it.
        if let Some(running) = self.edge_lights.iter_mut().find(|running| {
            running.pane_id == light.pane_id
                && running.first_row == light.first_row
                && running.spans == light.spans
                && running.running(now)
        }) {
            running.fill |= light.fill;
            return;
        }
        // A new selection's light takes over from an earlier selection's on
        // the pane (a triple click lights the word, then its line); a block
        // fading out finishes.
        if !light.fill {
            self.edge_lights
                .retain(|running| running.pane_id != light.pane_id || running.fill);
        }
        if self.edge_lights.len() >= MAX_LIGHTS {
            self.edge_lights.remove(0);
        }
        self.edge_lights.push(light);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Light up `pane`'s selection, as much of it as is on screen, because
    /// it was just copied. `fill` keeps its block fading with the light, for
    /// a selection about to be let go.
    pub fn light_up_selection(&mut self, pane: &Arc<dyn Pane>, fill: bool) {
        let Some(colored) = light_setting() else {
            return;
        };
        let (range, rectangular, backwards) = {
            let selection = self.selection(pane.pane_id());
            (selection.range, selection.rectangular, selection.backwards)
        };
        let Some(range) = range else {
            return;
        };
        let reversed = backwards || (range.start.y, range.start.x) > (range.end.y, range.end.x);
        let range = range.normalize();
        let dims = pane.get_dimensions();
        let top = self.get_viewport(pane.pane_id()).unwrap_or(dims.physical_top);
        // One row past the viewport: a smooth scroll shows part of it.
        let shown = top..top + dims.viewport_rows as StableRowIndex + 1;
        let selected = range.rows();
        let rows = selected.start.max(shown.start)..selected.end.min(shown.end);
        if rows.is_empty() {
            return;
        }
        let continues = (selected.start < rows.start, selected.end > rows.end);
        let first_row = rows.start;
        let spans = rows
            .map(|row| selected_span(&range, row, rectangular, usize::MAX))
            .collect();
        self.add_edge_light(EdgeLight {
            pane_id: pane.pane_id(),
            started: Instant::now(),
            first_row,
            spans,
            continues,
            colored,
            reversed,
            fill,
            duration: None,
        });
    }

    /// `text` was just pasted into `pane`, whose cursor was at `cursor`
    /// before it: light it up once it shows there. Only a single line is
    /// looked for; a paste of many runs commands, and what those print is
    /// not the paste.
    pub fn light_up_paste(&mut self, pane: &Arc<dyn Pane>, text: &str, cursor: StableCursorPosition) {
        let Some(colored) = light_setting() else {
            return;
        };
        let text = text.strip_suffix('\n').unwrap_or(text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        // Not the space a dropped path is followed by.
        let text = text.trim_end_matches([' ', '\t']);
        if text.is_empty() || text.contains(['\n', '\r']) || text.chars().count() > PASTE_MAX_CHARS {
            return;
        }
        let asked = Instant::now();
        self.pending_paste = Some(PendingPaste {
            pane_id: pane.pane_id(),
            asked,
            row: cursor.y,
            col: cursor.x,
            text: text.to_string(),
            colored,
        });
        if let Some(window) = self.window.clone() {
            window.invalidate();
            // Let go once it is given up even if its pane is never drawn
            // again: the text may be a password typed where nothing echoes.
            promise::spawn::spawn(async move {
                smol::Timer::after(PASTE_WAIT).await;
                window.notify(TermWindowNotif::Apply(Box::new(move |myself| {
                    if myself.pending_paste.as_ref().is_some_and(|p| p.asked == asked) {
                        myself.pending_paste = None;
                    }
                })));
            })
            .detach();
        }
    }

    /// Look for the pending paste among the drawn `lines` of `pane_id`
    /// (from `stable_top`), and light it up once it is there.
    pub fn find_pending_paste(
        &mut self,
        pane_id: PaneId,
        stable_top: StableRowIndex,
        lines: &[&mut Line],
        cols: usize,
    ) {
        let Some(pending) = self.pending_paste.as_ref().filter(|p| p.pane_id == pane_id) else {
            return;
        };
        let now = Instant::now();
        if now.duration_since(pending.asked) > PASTE_WAIT {
            self.pending_paste = None;
            return;
        }
        let found = usize::try_from(pending.row - stable_top)
            .ok()
            .filter(|first| *first < lines.len())
            .and_then(|first| {
                let lines: Vec<&Line> = lines[first..].iter().map(|line| &**line).collect();
                pasted_extent(&lines, pending.col, &pending.text)
            });
        let Some(end) = found else {
            self.update_next_frame_time(Some(now + Duration::from_millis(30)));
            return;
        };
        let pending = self.pending_paste.take().expect("checked above");
        self.add_edge_light(EdgeLight {
            pane_id,
            started: now,
            first_row: pending.row,
            spans: pasted_spans(pending.col, end, cols),
            continues: (false, false),
            colored: pending.colored,
            // Typed in from its start.
            reversed: false,
            fill: true,
            duration: None,
        });
    }

    /// The cells of `rows` whose text a light's block lies under, cut to
    /// `cols`, one span per row: that text keeps the selection's colours
    /// while the block is still more than half there. Empty (and not
    /// allocated) when there is none.
    pub fn light_block_spans(
        &self,
        pane_id: PaneId,
        rows: Range<StableRowIndex>,
        cols: usize,
    ) -> Vec<Range<usize>> {
        let now = Instant::now();
        let mut spans = vec![];
        for light in &self.edge_lights {
            if light.pane_id != pane_id
                || !light.fill
                || now.duration_since(light.started) >= light.duration() + FADE / 2
            {
                continue;
            }
            for (index, span) in light.spans.iter().enumerate() {
                let row = light.first_row + index as StableRowIndex;
                if !rows.contains(&row) {
                    continue;
                }
                if spans.is_empty() {
                    spans = vec![0..0; (rows.end - rows.start) as usize];
                }
                let end = span.end.min(cols);
                let slot = (row - rows.start) as usize;
                if spans[slot].is_empty() {
                    spans[slot] = span.start.min(end)..end;
                }
            }
        }
        spans
    }

    /// Paint the lights running on `pane_id`, drawn in `area`; a filled
    /// block in `block_color`. On a light `background` the white light's
    /// halo is a shadow, which is what gives it an edge there.
    pub fn paint_edge_lights(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        pane_id: PaneId,
        area: &LightArea,
        block_color: LinearRgba,
        background: LinearRgba,
    ) -> anyhow::Result<()> {
        let light_background =
            0.2126 * background.0 + 0.7152 * background.1 + 0.0722 * background.2 > 0.4;
        if self.edge_lights.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        self.edge_lights.retain(|light| !light.finished(now));
        if self.edge_lights.is_empty() {
            return Ok(());
        }
        self.update_next_frame_time(Some(now + Duration::from_millis(16)));
        let lights: Vec<usize> = (0..self.edge_lights.len())
            .filter(|&index| self.edge_lights[index].pane_id == pane_id)
            .collect();
        for index in lights {
            let light = &self.edge_lights[index];
            // Only the rows on screen, from the first of them.
            let light_rows = light.first_row..light.first_row + light.spans.len() as StableRowIndex;
            let shown = light_rows.start.max(area.rows.start)..light_rows.end.min(area.rows.end);
            if shown.is_empty() {
                continue;
            }
            let skip = (shown.start - light.first_row) as usize;
            // Cut to the columns drawn as the selection is, and counted in
            // single cells: a double-width row's are twice as wide.
            let spans: Vec<Range<usize>> = light.spans[skip..skip + (shown.end - shown.start) as usize]
                .iter()
                .zip(shown.clone())
                .map(|(span, row)| {
                    let scale = if area.double_width.contains(&row) { 2 } else { 1 };
                    let end = span.end.min(area.cols / scale);
                    span.start.min(end) * scale..end * scale
                })
                .collect();
            let top = area.origin.1 + (shown.start - area.rows.start) as f32 * area.cell.1;
            // Where the selection runs on past what is drawn, its edge there
            // is a cut, square, not a corner.
            let cut = (
                light.continues.0 || shown.start > light_rows.start,
                light.continues.1 || shown.end < light_rows.end,
            );
            let (colored, reversed, fill) = (light.colored, light.reversed, light.fill);
            let started = light.started;

            let radius = if area.rounded {
                corner_radius(area.cell.0, area.cell.1)
            } else {
                0.
            };
            let width = self.ui_f32(LINE_WIDTH).max(1.);
            // A ring round the outside of the block, the light's middle half
            // its width out from the block's edge -- inside the pane, where
            // that would cross its edge, the rows being cut short of it.
            let reach = self.ui_f32(OUTSET) + width;
            let inset = -(self.ui_f32(OUTSET) + width / 2.);
            let within = (
                area.bounds.0 + reach,
                area.bounds.1 + reach,
                area.bounds.2 - reach,
                area.bounds.3 - reach,
            );
            let outlines = Outline::of_rows(
                &spans,
                (area.origin.0, top),
                area.cell,
                radius,
                inset,
                within,
                cut,
            );
            let duration = match light.duration {
                Some(duration) => duration,
                None => {
                    // The longer of the two sides, at a steady speed.
                    let way = outlines
                        .iter()
                        .map(|outline| outline.meet.max(outline.length() - outline.meet))
                        .fold(0., f32::max);
                    let seconds = way / self.ui_f32(SPEED).max(1.);
                    let duration = Duration::from_secs_f32(seconds)
                        .clamp(MIN_DURATION, MAX_DURATION);
                    self.edge_lights[index].duration = Some(duration);
                    duration
                }
            };
            let elapsed = now.duration_since(started);
            let fade = if elapsed > duration {
                1. - (elapsed - duration).as_secs_f32() / FADE.as_secs_f32()
            } else {
                1.
            };
            let progress = ease_in_out((elapsed.as_secs_f32() / duration.as_secs_f32()).min(1.));

            if fill {
                let color = block_color.mul_alpha(fade.clamp(0., 1.));
                let last = spans.len().saturating_sub(1);
                for (row, span) in spans.iter().enumerate() {
                    // Running on past an edge, as the selection was drawn.
                    let above = match row {
                        0 if cut.0 => span.clone(),
                        0 => 0..0,
                        _ => spans[row - 1].clone(),
                    };
                    let below = match spans.get(row + 1) {
                        Some(next) => next.clone(),
                        None if row == last && cut.1 => span.clone(),
                        None => 0..0,
                    };
                    self.paint_selection_row(
                        layers,
                        0,
                        area.origin.0,
                        top + row as f32 * area.cell.1,
                        area.cell.0,
                        area.cell.1,
                        span,
                        &(above, below),
                        color,
                        area.rounded,
                        None,
                    )?;
                }
            }
            let style = LightStyle {
                progress,
                fade: fade.clamp(0., 1.),
                colored,
                reversed,
                light_background,
            };
            for outline in &outlines {
                self.paint_light_on(layers, outline, &style)?;
            }
        }
        Ok(())
    }

    /// A round dot of `size` centred on `at`.
    fn paint_dot(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        at: (f32, f32),
        size: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let mut quad = self.poly_quad(
            layers,
            2,
            euclid::point2(at.0 - size / 2., at.1 - size / 2.),
            DOT,
            0,
            euclid::size2(size, size),
            color,
        )?;
        quad.set_grayscale();
        quad.set_hsv(None);
        Ok(())
    }

    /// The lights on one outline, as far along and as faded as `style` says.
    fn paint_light_on(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        outline: &Outline,
        style: &LightStyle,
    ) -> anyhow::Result<()> {
        let LightStyle {
            progress,
            fade,
            colored,
            reversed,
            light_background,
        } = *style;
        let total = outline.length();
        if total <= 0. || fade <= 0. {
            return Ok(());
        }
        let (heads, travel) = light_heads(total, outline.meet, progress, reversed);
        let tail = self.ui_f32(MAX_TAIL).min(travel * 0.33);
        let width = self.ui_f32(LINE_WIDTH).max(1.);
        // A dot every pixel, each overlapping the next, reads as one
        // continuous line; spaced wider the tail breaks into a dotted one.
        let step = 1.;
        let (x_lo, x_hi) = outline.x_range;
        let across = |x: f32| if x_hi > x_lo { (x - x_lo) / (x_hi - x_lo) } else { 0. };
        let colors = LightColors::new();
        for (head, direction, limit) in heads {
            let mut back = 0.;
            while back < tail {
                let along = head - direction * back;
                let beyond = if direction > 0. { along < limit } else { along > limit };
                if beyond {
                    break;
                }
                let (x, y) = outline.point_at(along);
                let left = 1. - back / tail;
                let color = light_color(&colors, colored, across(x), light_background);
                let size = (width * (0.65 + 0.35 * left)).max(2.);
                let core = overlapped_alpha(left.powf(1.4) * fade, step, size);
                self.paint_dot(layers, (x, y), size, color.mul_alpha(core))?;
                // A soft halo round the brightest part, two rings of it:
                // light on a dark background, a shadow under white light on
                // a light one.
                if left > 0.7 {
                    let (halo_color, strength) = if light_background && !colored {
                        (LinearRgba::with_srgba(0, 0, 0, 255), 0.12)
                    } else {
                        (color, 0.08)
                    };
                    let strength = strength * (left - 0.7) / 0.3 * fade;
                    let (outer, inner) = (width * 3.4, width * 2.2);
                    let outer_alpha = overlapped_alpha(strength * 0.6, step, outer);
                    let inner_alpha = overlapped_alpha(strength, step, inner);
                    self.paint_dot(layers, (x, y), outer, halo_color.mul_alpha(outer_alpha))?;
                    self.paint_dot(layers, (x, y), inner, halo_color.mul_alpha(inner_alpha))?;
                }
                back += step;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::surface::SEQ_ZERO;

    fn line(text: &str, wrapped: bool) -> Line {
        let mut line = Line::from_text(text, &Default::default(), SEQ_ZERO, None);
        line.set_last_cell_was_wrapped(wrapped, SEQ_ZERO);
        line
    }

    #[test]
    fn a_paste_is_found_where_it_was_typed_and_only_there() {
        let shown = line("$ git status --short", false);
        let lines = vec![&shown];
        assert_eq!(pasted_extent(&lines, 2, "git status"), Some((0, 12)));
        assert_eq!(pasted_extent(&lines, 2, "git status --short"), Some((0, 20)));
        // Not (yet) what was pasted.
        assert_eq!(pasted_extent(&lines, 2, "git stash"), None);
        assert_eq!(pasted_extent(&lines, 2, "git status --short --branch"), None);
    }

    #[test]
    fn a_paste_runs_on_across_a_wrapped_row() {
        let first = line("$ abcdef", true);
        let second = line("ghij", false);
        let lines = vec![&first, &second];
        assert_eq!(pasted_extent(&lines, 2, "abcdefgh"), Some((1, 2)));
        assert_eq!(pasted_spans(2, (1, 2), 8), vec![2..8, 0..2]);
        // A row that ends without wrapping stops it.
        let unwrapped = line("$ abcdef", false);
        let lines = vec![&unwrapped, &second];
        assert_eq!(pasted_extent(&lines, 2, "abcdefgh"), None);
    }

    #[test]
    fn the_coloured_light_goes_blue_to_amber() {
        let colors = LightColors::new();
        let start = light_color(&colors, true, 0., false);
        let end = light_color(&colors, true, 1., false);
        assert!(start.2 > start.0, "blue at the left");
        assert!(end.0 > end.2, "amber at the right");
        assert_eq!(light_color(&colors, false, 0.5, false), LinearRgba::with_srgba(255, 255, 255, 255));
        // White would vanish on a light background: graphite there.
        let plain = light_color(&colors, false, 0.5, true);
        assert!(plain.0 < 0.2 && plain.1 < 0.2 && plain.2 < 0.2);
    }

    #[test]
    fn overlapping_dots_add_up_to_the_alpha_asked_for() {
        let each = overlapped_alpha(0.5, 1., 4.);
        // Four of them over one point leave what one at 0.5 would.
        assert!((1. - (1. - each).powi(4) - 0.5).abs() < 1e-5);
        assert_eq!(overlapped_alpha(1., 1., 4.), 1.);
        assert_eq!(overlapped_alpha(0., 1., 4.), 0.);
        // A dot no wider than the step overlaps nothing.
        assert!((overlapped_alpha(0.3, 2., 1.) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn a_light_runs_from_where_the_selection_was_begun() {
        // An outline 100 long whose far end (the middle of the last row's
        // right side) is 40 along it.
        let heads = |progress, reversed| light_heads(100., 40., progress, reversed).0;
        // Forwards they leave the start and meet at the far end.
        assert_eq!(heads(0., false), [(0., 1., 0.), (100., -1., 100.)]);
        assert_eq!(heads(1., false), [(40., 1., 0.), (40., -1., 100.)]);
        // Backwards they leave the far end and meet at the start.
        assert_eq!(heads(0., true), [(40., -1., 40.), (40., 1., 40.)]);
        assert_eq!(heads(1., true), [(0., -1., 40.), (100., 1., 40.)]);
        // Either way the longer side sets how far they go.
        assert_eq!(light_heads(100., 40., 0.5, true).1, 60.);
    }

    #[test]
    fn the_light_eases_in_and_out() {
        assert_eq!(ease_in_out(0.), 0.);
        assert_eq!(ease_in_out(1.), 1.);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
        assert!(ease_in_out(0.1) < 0.1 && ease_in_out(0.9) > 0.9);
    }
}
