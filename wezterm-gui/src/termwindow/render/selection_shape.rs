//! The selection drawn as one rounded shape instead of a strip per row.
//!
//! Each row paints its own part from three spans -- its own and its two
//! neighbours' -- so the line quad cache keeps working row by row. A corner
//! with nothing beyond it is rounded outward; where a neighbour reaches
//! further, the corner between them gets an inward curve, painted within
//! this row's height just outside its span.

use crate::quad::{QuadTrait, TripleLayerQuadAllocator};
use crate::selection::SelectionRange;
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_LEFT_ROUNDED_CORNER_MASK, BOTTOM_RIGHT_ROUNDED_CORNER,
    BOTTOM_RIGHT_ROUNDED_CORNER_MASK, TOP_LEFT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER_MASK,
    TOP_RIGHT_ROUNDED_CORNER, TOP_RIGHT_ROUNDED_CORNER_MASK,
};
use config::HsbTransform;
use std::ops::Range;
use termwiz::color::LinearRgba;
use wezterm_term::StableRowIndex;

/// Corner indices: the right corner of a side is the one after its left.
const TOP_LEFT: usize = 0;
const BOTTOM_LEFT: usize = 2;

/// The columns of `row` that `selection` covers, cut to the `cols` drawn:
/// the one rule the selection and the light round it are both drawn by.
pub fn selected_span(
    selection: &SelectionRange,
    row: StableRowIndex,
    rectangular: bool,
    cols: usize,
) -> Range<usize> {
    let span = selection.cols_for_row(row, rectangular);
    let end = span.end.min(cols);
    span.start.min(end)..end
}

/// The neighbours a row's part of the shape is drawn against: none round an
/// empty row, and its own span on a side where it or the neighbour is of
/// double width -- their cells do not line up, so the edge runs straight.
pub fn drawn_neighbours(
    own: &Range<usize>,
    above: (&Range<usize>, bool),
    below: (&Range<usize>, bool),
    double: bool,
) -> (Range<usize>, Range<usize>) {
    if own.is_empty() {
        return (0..0, 0..0);
    }
    let side = |(span, neighbour_double): (&Range<usize>, bool)| {
        if double || neighbour_double {
            own.clone()
        } else {
            span.clone()
        }
    };
    (side(above), side(below))
}

/// What each corner of a row's span does, as top left, top right, bottom
/// left, bottom right.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowCorners {
    /// Rounded outward: the shape does not go on past it.
    pub round: [bool; 4],
    /// The neighbour on that side reaches past this end of the span: an
    /// inward curve just outside it. Neither means the edge runs straight on.
    pub inward: [bool; 4],
}

pub fn row_corners(row: &Range<usize>, above: &Range<usize>, below: &Range<usize>) -> RowCorners {
    let mut corners = RowCorners::default();
    if row.is_empty() {
        return corners;
    }
    let covers = |span: &Range<usize>, col: usize| span.start <= col && col < span.end;
    // Inward only where the neighbour runs on past this end AND over it: a
    // neighbour that merely touches the corner leaves two pieces, each
    // rounded there, as the light's outline has them.
    for (left, neighbour) in [(TOP_LEFT, above), (BOTTOM_LEFT, below)] {
        let right = left + 1;
        if row.start > 0 && covers(neighbour, row.start - 1) && covers(neighbour, row.start) {
            corners.inward[left] = true;
        } else if !covers(neighbour, row.start) {
            corners.round[left] = true;
        }
        if covers(neighbour, row.end) && covers(neighbour, row.end - 1) {
            corners.inward[right] = true;
        } else if !covers(neighbour, row.end - 1) {
            corners.round[right] = true;
        }
    }
    corners
}

/// The radius of the selection's corners: a quarter of a row, but never
/// more than half a cell, so the inward curve beside a one-cell step fits.
pub fn corner_radius(cell_width: f32, cell_height: f32) -> f32 {
    (cell_height * 0.25).min(cell_width * 0.5).floor()
}

/// One stretch of an outline: a straight edge, or the arc round a corner.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stretch {
    Line { from: (f32, f32), to: (f32, f32) },
    Arc { center: (f32, f32), radius: f32, from_angle: f32, sweep: f32 },
}

impl Stretch {
    fn length(&self) -> f32 {
        match *self {
            Stretch::Line { from, to } => (to.0 - from.0).hypot(to.1 - from.1),
            Stretch::Arc { radius, sweep, .. } => radius * sweep.abs(),
        }
    }

    fn point_at(&self, along: f32) -> (f32, f32) {
        let length = self.length();
        let t = if length > 0. { (along / length).clamp(0., 1.) } else { 0. };
        match *self {
            Stretch::Line { from, to } => (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t),
            Stretch::Arc { center, radius, from_angle, sweep } => {
                let angle = from_angle + sweep * t;
                (center.0 + radius * angle.cos(), center.1 + radius * angle.sin())
            }
        }
    }
}

/// The rounded outline of one connected piece of a selection, clockwise
/// from the middle of the first row's left side.
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    stretches: Vec<Stretch>,
    /// The distance along the outline at which each stretch ends.
    ends: Vec<f32>,
    /// Where lights running down both sides meet: the middle of the last
    /// row's right side.
    pub meet: f32,
    /// The left and right edges, for colouring along the width.
    pub x_range: (f32, f32),
}

impl Outline {
    pub fn length(&self) -> f32 {
        self.ends.last().copied().unwrap_or(0.)
    }

    /// The point `along` the outline from its start; past either end it
    /// goes on round, as the outline is closed.
    pub fn point_at(&self, along: f32) -> (f32, f32) {
        let total = self.length();
        if total <= 0. {
            return self.stretches.first().map_or((0., 0.), |s| s.point_at(0.));
        }
        let along = along.rem_euclid(total);
        let index = self.ends.partition_point(|end| *end < along).min(self.stretches.len() - 1);
        let start = if index == 0 { 0. } else { self.ends[index - 1] };
        self.stretches[index].point_at(along - start)
    }

    /// The outlines of the shape `spans` make, one row of `cell` size each
    /// from `origin` down, drawn `inset` inside its edge, its rows first cut
    /// to `within` (left, top, right, bottom): one per connected piece (rows
    /// whose spans do not overlap the next row's start a new one). `cut`
    /// says the shape runs on above the first row and below the last, past
    /// what is drawn: its corners there are square, as the selection's are.
    pub fn of_rows(
        spans: &[Range<usize>],
        origin: (f32, f32),
        cell: (f32, f32),
        radius: f32,
        inset: f32,
        within: (f32, f32, f32, f32),
        cut: (bool, bool),
    ) -> Vec<Outline> {
        // Each row as (left, right, top, bottom), or none where nothing of
        // it is left inside `within`.
        let rows: Vec<Option<(f32, f32, f32, f32)>> = spans
            .iter()
            .enumerate()
            .map(|(row, span)| {
                let top = origin.1 + row as f32 * cell.1;
                let edges = (
                    (origin.0 + span.start as f32 * cell.0).max(within.0),
                    (origin.0 + span.end as f32 * cell.0).min(within.2),
                    top.max(within.1),
                    (top + cell.1).min(within.3),
                );
                if span.is_empty() || edges.0 >= edges.1 || edges.2 >= edges.3 {
                    None
                } else {
                    Some(edges)
                }
            })
            .collect();
        let mut outlines = vec![];
        let mut first = 0;
        while first < rows.len() {
            if rows[first].is_none() {
                first += 1;
                continue;
            }
            let mut last = first;
            while let (Some(a), Some(Some(b))) = (rows[last], rows.get(last + 1)) {
                if a.0.max(b.0) >= a.1.min(b.1) {
                    break;
                }
                last += 1;
            }
            let piece: Vec<(f32, f32, f32, f32)> = rows[first..=last].iter().flatten().copied().collect();
            let square = (cut.0 && first == 0, cut.1 && last + 1 == rows.len());
            outlines.push(Self::of_piece(&piece, radius, inset, square));
            first = last + 1;
        }
        outlines
    }

    /// `rows` as (left, right, top, bottom), each overlapping the next; the
    /// corners of the first row's top and the last row's bottom square as
    /// `square` says.
    fn of_piece(
        rows: &[(f32, f32, f32, f32)],
        radius: f32,
        inset: f32,
        square: (bool, bool),
    ) -> Outline {
        let n = rows.len();
        let (x0, x1, y0, y1) = (|r: usize| rows[r].0, |r: usize| rows[r].1, |r: usize| rows[r].2, |r: usize| rows[r].3);
        // The corners, clockwise from the top left.
        let mut corners = vec![(x0(0), y0(0)), (x1(0), y0(0))];
        for row in 0..n - 1 {
            if x1(row + 1) != x1(row) {
                corners.push((x1(row), y1(row)));
                corners.push((x1(row + 1), y1(row)));
            }
        }
        let bottom_right = corners.len();
        corners.push((x1(n - 1), y1(n - 1)));
        corners.push((x0(n - 1), y1(n - 1)));
        for row in (1..n).rev() {
            if x0(row - 1) != x0(row) {
                corners.push((x0(row), y0(row)));
                corners.push((x0(row - 1), y0(row)));
            }
        }
        let count = corners.len();
        let unit = |from: (f32, f32), to: (f32, f32)| {
            let length = (to.0 - from.0).hypot(to.1 - from.1);
            ((to.0 - from.0) / length, (to.1 - from.1) / length, length)
        };
        // Inset: every edge moves inward, to the right of the way it runs
        // (clockwise, y down), and each corner to where its two edges meet.
        let corners: Vec<(f32, f32)> = (0..count)
            .map(|i| {
                let (prev, here, next) = (corners[(i + count - 1) % count], corners[i], corners[(i + 1) % count]);
                let (ix, iy, _) = unit(prev, here);
                let (ox, oy, _) = unit(here, next);
                (here.0 + (-iy - oy) * inset, here.1 + (ix + ox) * inset)
            })
            .collect();
        // Each corner becomes an arc between points this far along its edges:
        // from `a` to `b` round `center`, `r` out.
        struct Turn {
            a: (f32, f32),
            b: (f32, f32),
            center: (f32, f32),
            r: f32,
        }
        let turns: Vec<Turn> = (0..count)
            .map(|i| {
                let (prev, here, next) = (corners[(i + count - 1) % count], corners[i], corners[(i + 1) % count]);
                let (ix, iy, in_len) = unit(prev, here);
                let (ox, oy, out_len) = unit(here, next);
                // Drawn `inset` in, an outer corner's curve tightens by that
                // much and an inner one's widens, so both stay concentric
                // with the shape's own; out (a negative inset) the other way
                // round, an inner curve kept to at least half its radius.
                let turns_right = ix * oy - iy * ox > 0.;
                let r = if turns_right { radius - inset } else { radius + inset };
                let r = r.max(radius * 0.5).min(in_len / 2.).min(out_len / 2.);
                let cut_here = (square.0 && i < 2)
                    || (square.1 && (i == bottom_right || i == bottom_right + 1));
                let r = if cut_here { 0. } else { r };
                let a = (here.0 - ix * r, here.1 - iy * r);
                let b = (here.0 + ox * r, here.1 + oy * r);
                // The edges meet square, so the centre is `r` from `a`
                // towards where the outline turns.
                let center = (a.0 + ox * r, a.1 + oy * r);
                Turn { a, b, center, r }
            })
            .collect();

        let start = (x0(0) + inset, (y0(0) + y1(0)) / 2.);
        let meet_point = (x1(n - 1) - inset, (y0(n - 1) + y1(n - 1)) / 2.);
        let mut stretches = vec![];
        let mut ends = vec![];
        let mut total = 0.;
        let mut meet = 0.;
        let mut push = |stretch: Stretch, stretches: &mut Vec<Stretch>, ends: &mut Vec<f32>| {
            total += stretch.length();
            stretches.push(stretch);
            ends.push(total);
            total
        };
        let mut at = start;
        for (i, &Turn { a, b, center, r }) in turns.iter().enumerate() {
            let before = push(Stretch::Line { from: at, to: a }, &mut stretches, &mut ends);
            if i == bottom_right {
                // The meeting point is on the edge that ends here.
                meet = before - (a.0 - meet_point.0).hypot(a.1 - meet_point.1);
            }
            if r > 0. {
                let from_angle = (a.1 - center.1).atan2(a.0 - center.0);
                let to_angle = (b.1 - center.1).atan2(b.0 - center.0);
                let mut sweep = to_angle - from_angle;
                if sweep > std::f32::consts::PI {
                    sweep -= 2. * std::f32::consts::PI;
                } else if sweep < -std::f32::consts::PI {
                    sweep += 2. * std::f32::consts::PI;
                }
                push(Stretch::Arc { center, radius: r, from_angle, sweep }, &mut stretches, &mut ends);
            }
            at = b;
        }
        push(Stretch::Line { from: at, to: start }, &mut stretches, &mut ends);
        let x_range = rows.iter().fold((f32::MAX, f32::MIN), |(lo, hi), r| (lo.min(r.0), hi.max(r.1)));
        Outline {
            stretches,
            ends,
            meet,
            x_range,
        }
    }
}

impl crate::TermWindow {
    /// Paint one row's part of the selection, `span` in cells from
    /// `left_x`, in the row whose top is `top_y`; unless `rounded`, the
    /// plain block it always was.
    #[allow(clippy::too_many_arguments)]
    pub fn paint_selection_row(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer: usize,
        left_x: f32,
        top_y: f32,
        cell_width: f32,
        cell_height: f32,
        span: &Range<usize>,
        neighbours: &(Range<usize>, Range<usize>),
        color: LinearRgba,
        rounded: bool,
        hsv: Option<HsbTransform>,
    ) -> anyhow::Result<()> {
        if span.is_empty() {
            return Ok(());
        }
        let x0 = left_x + span.start as f32 * cell_width;
        let x1 = left_x + span.end as f32 * cell_width;
        let y0 = top_y;
        let y1 = top_y + cell_height;
        let r = if rounded { corner_radius(cell_width, cell_height) } else { 0. };
        let fill = |layers: &mut TripleLayerQuadAllocator, x: f32, y: f32, w: f32, h: f32| {
            if w > 0. && h > 0. {
                self.filled_rectangle(layers, layer, euclid::rect(x, y, w, h), color)?
                    .set_hsv(hsv);
            }
            anyhow::Ok(())
        };
        if r < 1. {
            return fill(layers, x0, y0, x1 - x0, cell_height);
        }

        let corners = row_corners(span, &neighbours.0, &neighbours.1);
        // The middle, the sides between the corners, then the corners.
        fill(layers, x0 + r, y0, x1 - x0 - 2. * r, cell_height)?;
        fill(layers, x0, y0 + r, r, cell_height - 2. * r)?;
        fill(layers, x1 - r, y0 + r, r, cell_height - 2. * r)?;
        let size = euclid::size2(r, r);
        let corner_at = [(x0, y0), (x1 - r, y0), (x0, y1 - r), (x1 - r, y1 - r)];
        let rounded = [
            TOP_LEFT_ROUNDED_CORNER,
            TOP_RIGHT_ROUNDED_CORNER,
            BOTTOM_LEFT_ROUNDED_CORNER,
            BOTTOM_RIGHT_ROUNDED_CORNER,
        ];
        for (corner, (&(x, y), shape)) in corner_at.iter().zip(rounded).enumerate() {
            if corners.round[corner] {
                let mut quad = self.poly_quad(layers, layer, euclid::point2(x, y), shape, 0, size, color)?;
                quad.set_grayscale();
                quad.set_hsv(hsv);
            } else {
                fill(layers, x, y, r, r)?;
            }
        }
        // Each inward curve sits in the square beside the span whose corner
        // touching the span is the vertex: the wedge in that corner of it.
        let inward_at = [
            ((x0 - r, y0), TOP_RIGHT_ROUNDED_CORNER_MASK),
            ((x1, y0), TOP_LEFT_ROUNDED_CORNER_MASK),
            ((x0 - r, y1 - r), BOTTOM_RIGHT_ROUNDED_CORNER_MASK),
            ((x1, y1 - r), BOTTOM_LEFT_ROUNDED_CORNER_MASK),
        ];
        for (&inward, &((x, y), wedge)) in corners.inward.iter().zip(&inward_at) {
            if inward {
                let mut quad = self.poly_quad(layers, layer, euclid::point2(x, y), wedge, 0, size, color)?;
                quad.set_grayscale();
                quad.set_hsv(hsv);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No pane edge anywhere near.
    const OPEN: (f32, f32, f32, f32) = (f32::MIN, f32::MIN, f32::MAX, f32::MAX);
    /// The whole shape is drawn: nothing runs on past it.
    const WHOLE: (bool, bool) = (false, false);

    fn round(c: RowCorners) -> [bool; 4] {
        c.round
    }

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01
    }

    #[test]
    fn the_outline_of_one_row_goes_round_its_rounded_box() {
        // Cells of 10x20 from the origin, columns 1..5: a box 40 wide at x 10.
        let outlines = Outline::of_rows(&[1..5], (0., 0.), (10., 20.), 4., 0., OPEN, WHOLE);
        assert_eq!(outlines.len(), 1);
        let outline = &outlines[0];
        // A 40x20 box with corners of 4: the straight parts plus a circle.
        let expected = 2. * (40. - 8.) + 2. * (20. - 8.) + 2. * std::f32::consts::PI * 4.;
        assert!((outline.length() - expected).abs() < 0.01, "{}", outline.length());
        // It starts in the middle of the left side and goes up first.
        assert!(close(outline.point_at(0.), (10., 10.)));
        assert!(close(outline.point_at(6.), (10., 4.)));
        // Lights down both sides meet in the middle of the right side,
        // exactly half way round a symmetric box.
        assert!((outline.meet - expected / 2.).abs() < 0.01);
        assert!(close(outline.point_at(outline.meet), (50., 10.)));
        // Closed: going on past the end comes round again.
        assert!(close(outline.point_at(outline.length() + 6.), (10., 4.)));
        assert_eq!(outline.x_range, (10., 50.));
    }

    #[test]
    fn a_staircase_is_one_outline_and_rows_apart_are_two() {
        let staircase = Outline::of_rows(&[5..10, 0..12, 0..7], (0., 0.), (10., 20.), 4., 0., OPEN, WHOLE);
        assert_eq!(staircase.len(), 1);
        // Every point of it is on the shape's edge, inside its bounds.
        let outline = &staircase[0];
        let mut along = 0.;
        while along < outline.length() {
            let (x, y) = outline.point_at(along);
            assert!((0. ..=120.).contains(&x) && (0. ..=60.).contains(&y), "{},{}", x, y);
            along += 1.;
        }
        assert!(close(outline.point_at(outline.meet), (70., 50.)));

        // The second row ends before the first starts: they do not touch.
        let apart = Outline::of_rows(&[8..12, 0..3], (0., 0.), (10., 20.), 4., 0., OPEN, WHOLE);
        assert_eq!(apart.len(), 2);
        // An empty row in between splits a selection too.
        assert_eq!(Outline::of_rows(&[0..4, 0..0, 0..4], (0., 0.), (10., 20.), 4., 0., OPEN, WHOLE).len(), 2);
    }

    #[test]
    fn an_inset_outline_runs_inside_the_shape() {
        let outline = &Outline::of_rows(&[5..10, 0..12, 0..7], (0., 0.), (10., 20.), 4., 1., OPEN, WHOLE)[0];
        assert!(close(outline.point_at(0.), (51., 10.)));
        assert!(close(outline.point_at(outline.meet), (69., 50.)));
        // A point on every straight part is one pixel in from the edge: the
        // top of the first row is now at y 1.
        let top = (0..outline.length() as usize)
            .map(|along| outline.point_at(along as f32).1)
            .fold(f32::MAX, f32::min);
        assert!((top - 1.).abs() < 0.01, "{}", top);
    }

    #[test]
    fn a_row_alone_is_rounded_all_round() {
        let c = row_corners(&(3..9), &(0..0), &(0..0));
        assert_eq!(round(c), [true; 4]);
        assert_eq!(c.inward, [false; 4]);
    }

    #[test]
    fn rows_of_a_staircase_meet_with_inward_curves() {
        // First row from column 5, the next from 0 to 12, the last to 7:
        //      ┌─────┐
        //   ┌──┘     └─┐
        //   └──────┐ ┌─┘  (the last row ends before the middle one)
        let first = 5..10;
        let middle = 0..12;
        let last = 0..7;
        let c = row_corners(&first, &(0..0), &middle);
        assert_eq!(round(c), [true, true, false, false]);
        // The middle row reaches past both ends below.
        assert_eq!(c.inward, [false, false, true, true]);

        let c = row_corners(&middle, &first, &last);
        // Above, it reaches past the first row: its own top corners are
        // the outer ones. Below, it shares the left edge with the last row
        // and reaches past it on the right.
        assert_eq!(round(c), [true, true, false, true]);
        assert_eq!(c.inward, [false; 4]);

        let c = row_corners(&last, &middle, &(0..0));
        assert_eq!(round(c), [false, false, true, true]);
        assert_eq!(c.inward, [false, true, false, false]);
    }

    #[test]
    fn rows_that_only_touch_at_a_corner_are_two_rounded_pieces() {
        // From column 10 of one row to column 9 of the next.
        let first = 10..80;
        let second = 0..10;
        let c = row_corners(&first, &(0..0), &second);
        assert_eq!(round(c), [true; 4]);
        assert_eq!(c.inward, [false; 4]);
        let c = row_corners(&second, &first, &(0..0));
        assert_eq!(round(c), [true; 4]);
        assert_eq!(c.inward, [false; 4]);
        assert_eq!(Outline::of_rows(&[first, second], (0., 0.), (10., 20.), 4., 0., OPEN, WHOLE).len(), 2);
    }

    #[test]
    fn a_classic_selection_is_one_staircase() {
        // From column 30 to the end, two whole rows, then up to column 12.
        let rows = [30..80, 0..80, 0..80, 0..12];
        let c = row_corners(&rows[0], &(0..0), &rows[1]);
        assert_eq!(round(c), [true, true, false, false]);
        assert_eq!(c.inward, [false, false, true, false]);
        let c = row_corners(&rows[3], &rows[2], &(0..0));
        assert_eq!(round(c), [false, false, true, true]);
        assert_eq!(c.inward, [false, true, false, false]);
        assert_eq!(Outline::of_rows(&rows, (0., 0.), (10., 20.), 4., 0., OPEN, WHOLE).len(), 1);
    }

    #[test]
    fn a_straight_edge_is_no_corner() {
        let c = row_corners(&(2..8), &(2..8), &(2..8));
        assert_eq!(c, RowCorners::default());
    }

    #[test]
    fn corners_fit_a_one_cell_step() {
        assert_eq!(corner_radius(8., 17.), 4.);
        assert_eq!(corner_radius(17., 34.), 8.);
        assert_eq!(corner_radius(5., 30.), 2.);
    }

    #[test]
    fn an_outline_cut_to_the_pane_runs_inside_its_edges() {
        // A pane of 8x3 cells of 10x20; the line one pixel out from the
        // block, cut two pixels in from the pane's edges as the light is.
        let outline =
            &Outline::of_rows(&[0..8, 0..8, 0..3], (0., 0.), (10., 20.), 4., -1., (2., 2., 78., 58.), WHOLE)[0];
        let mut along = 0.;
        while along < outline.length() {
            let (x, y) = outline.point_at(along);
            assert!((1. ..=79.).contains(&x) && (1. ..=59.).contains(&y), "{},{}", x, y);
            along += 1.;
        }
        // Along the pane's left edge it is inside the block (from the middle
        // of the first row's part inside the pane); where the last row ends,
        // short of the edge, it is out past the block as usual.
        assert!(close(outline.point_at(0.), (1., 11.)));
        assert!(close(outline.point_at(outline.meet), (31., 49.)));
    }

    #[test]
    fn a_row_with_nothing_inside_the_pane_is_left_out() {
        // The first row is all above the top: the outline is the second's.
        let outlines = Outline::of_rows(&[0..4, 0..4], (0., 0.), (10., 20.), 4., 0., (-100., 25., 100., 100.), WHOLE);
        assert_eq!(outlines.len(), 1);
        let top = (0..outlines[0].length() as usize)
            .map(|along| outlines[0].point_at(along as f32).1)
            .fold(f32::MAX, f32::min);
        assert!((top - 25.).abs() < 0.01, "{}", top);
        // Nothing of either left: no outline at all.
        assert!(Outline::of_rows(&[0..4, 0..4], (0., 0.), (10., 20.), 4., 0., (50., 0., 100., 100.), WHOLE).is_empty());
    }

    #[test]
    fn the_selected_span_is_cut_to_the_columns_drawn() {
        use crate::selection::SelectionCoordinate;
        // Made backwards, from column 5 of row 12 up to column 3 of row 10.
        let selection = SelectionRange {
            start: SelectionCoordinate::x_y(5, 12),
            end: SelectionCoordinate::x_y(3, 10),
        }
        .normalize();
        assert_eq!(selected_span(&selection, 10, false, 80), 3..80);
        assert_eq!(selected_span(&selection, 11, false, 80), 0..80);
        assert_eq!(selected_span(&selection, 12, false, 80), 0..6);
        assert_eq!(selected_span(&selection, 13, false, 80), 0..0);
        // Uncut, it runs to the end of the row.
        assert_eq!(selected_span(&selection, 11, false, usize::MAX), 0..usize::MAX);
        // A selection starting past the columns drawn shows nothing there.
        let wide = SelectionRange {
            start: SelectionCoordinate::x_y(90, 10),
            end: SelectionCoordinate::x_y(95, 10),
        };
        assert!(selected_span(&wide, 10, false, 80).is_empty());
    }

    #[test]
    fn a_shape_cut_off_by_the_screen_has_square_corners_there() {
        // A 40x20 box with corners of 4, running on above it: square at the
        // top, rounded at the bottom.
        let cut_top = &Outline::of_rows(&[1..5], (0., 0.), (10., 20.), 4., 0., OPEN, (true, false))[0];
        let quarter_saved = 2. * 4. - std::f32::consts::FRAC_PI_2 * 4.;
        let whole_box = 2. * (40. + 20.);
        assert!((cut_top.length() - (whole_box - 2. * quarter_saved)).abs() < 0.01, "{}", cut_top.length());
        // The corner itself is on it.
        assert!((0..cut_top.length() as usize).any(|along| close(cut_top.point_at(along as f32), (10., 0.))));
        // Running on both ways, the box's own four sides.
        let both = &Outline::of_rows(&[1..5], (0., 0.), (10., 20.), 4., 0., OPEN, (true, true))[0];
        assert!((both.length() - whole_box).abs() < 0.01, "{}", both.length());
    }

    #[test]
    fn rows_beside_a_double_width_row_meet_it_straight() {
        let (own, above, below) = (2..8, 0..10, 4..6);
        assert_eq!(drawn_neighbours(&own, (&above, false), (&below, false), false), (0..10, 4..6));
        // The row itself double: its own span both ways, a plain box.
        assert_eq!(drawn_neighbours(&own, (&above, false), (&below, false), true), (2..8, 2..8));
        // The neighbour below double: straight on that side only.
        assert_eq!(drawn_neighbours(&own, (&above, false), (&below, true), false), (0..10, 2..8));
        // An empty row has no shape to draw against anything.
        assert_eq!(drawn_neighbours(&(0..0), (&above, false), (&below, false), false), (0..0, 0..0));
    }
}
