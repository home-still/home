//! The shape a highlight is drawn as.
//!
//! A highlight arrives as one rectangle per text line
//! ([`hs_common::quote::line_rects`]), and the lines of a paragraph set
//! tight give rectangles that overlap: a word's box is as tall as its
//! font, which is taller than the step from one baseline to the next.
//! Drawn one by one, each rectangle's edge runs through the text of the
//! line beside it, as a strike-through would, and the overlap is tinted
//! twice.
//!
//! So the rectangles are drawn as the one region they cover together:
//! tinted once, with a line around the outside of the whole. [`Shape`] is
//! that region, in the units the rectangles came in, and both things that
//! draw a highlight (the page image and the viewer) draw it from here.

use serde::Serialize;

/// The region a set of rectangles covers together, ready to draw.
///
/// Every coordinate is one of the rectangles' own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Shape {
    /// Rectangles `[x0, y0, x1, y1]` that cover the region between them
    /// and do not overlap: a tint laid on each lands once everywhere.
    pub(crate) fill: Vec<[u16; 4]>,
    /// The region's edge as straight lines `[x0, y0, x1, y1]`, each level
    /// (`y0 == y1`) or upright (`x0 == x1`). Where rectangles meet or
    /// overlap there is no line: only the outside of the whole has one.
    pub(crate) outline: Vec<[u16; 4]>,
}

impl Shape {
    /// The region `rects` cover. A rectangle with no area covers nothing.
    pub(crate) fn of(rects: &[[u16; 4]]) -> Self {
        let grid = Grid::of(rects);
        Self {
            fill: grid.fill(),
            outline: grid.outline(),
        }
    }
}

/// The page cut along every edge of every rectangle, and which of the
/// cells that leaves are inside one. Cell `column` of row `row` lies
/// between `xs[column]` and `xs[column + 1]`, `ys[row]` and `ys[row + 1]`.
struct Grid {
    xs: Vec<u16>,
    ys: Vec<u16>,
    covered: Vec<Vec<bool>>,
}

impl Grid {
    fn of(rects: &[[u16; 4]]) -> Self {
        let rects: Vec<[u16; 4]> = rects
            .iter()
            .copied()
            .filter(|&[x0, y0, x1, y1]| x0 < x1 && y0 < y1)
            .collect();
        let xs = cuts(rects.iter().flat_map(|rect| [rect[0], rect[2]]));
        let ys = cuts(rects.iter().flat_map(|rect| [rect[1], rect[3]]));
        let columns = xs.len().saturating_sub(1);
        let rows = ys.len().saturating_sub(1);
        let mut covered = vec![vec![false; columns]; rows];
        for [x0, y0, x1, y1] in rects {
            // Every edge is a cut, so each search finds its place.
            let (Ok(left), Ok(right), Ok(top), Ok(bottom)) = (
                xs.binary_search(&x0),
                xs.binary_search(&x1),
                ys.binary_search(&y0),
                ys.binary_search(&y1),
            ) else {
                continue;
            };
            for row in covered.get_mut(top..bottom).unwrap_or_default() {
                if let Some(cells) = row.get_mut(left..right) {
                    cells.fill(true);
                }
            }
        }
        Self { xs, ys, covered }
    }

    /// Whether a cell is inside a rectangle. A cell off the grid, on any
    /// side, is not.
    fn is_covered(&self, row: Option<usize>, column: Option<usize>) -> bool {
        row.zip(column)
            .and_then(|(row, column)| self.covered.get(row)?.get(column).copied())
            .unwrap_or(false)
    }

    fn fill(&self) -> Vec<[u16; 4]> {
        let mut fill: Vec<[u16; 4]> = Vec::new();
        for (row, band) in self.ys.windows(2).enumerate() {
            let &[top, bottom] = band else { continue };
            let covered = |column| self.is_covered(Some(row), Some(column));
            for (left, right) in runs(&self.xs, covered) {
                // The same stretch of the row above grows down by this
                // row, so a rectangle nothing cut across comes out whole.
                let above = fill
                    .iter_mut()
                    .find(|above| above[0] == left && above[2] == right && above[3] == top);
                match above {
                    Some(above) => above[3] = bottom,
                    None => fill.push([left, top, right, bottom]),
                }
            }
        }
        fill
    }

    fn outline(&self) -> Vec<[u16; 4]> {
        let mut outline = Vec::new();
        // The region's edge runs along a cut wherever exactly one of the
        // two cells the cut parts is covered.
        for (cut, &y) in self.ys.iter().enumerate() {
            let edge = |column| {
                self.is_covered(cut.checked_sub(1), Some(column))
                    != self.is_covered(Some(cut), Some(column))
            };
            outline.extend(
                runs(&self.xs, edge)
                    .into_iter()
                    .map(|(from, to)| [from, y, to, y]),
            );
        }
        for (cut, &x) in self.xs.iter().enumerate() {
            let edge = |row| {
                self.is_covered(Some(row), cut.checked_sub(1))
                    != self.is_covered(Some(row), Some(cut))
            };
            outline.extend(
                runs(&self.ys, edge)
                    .into_iter()
                    .map(|(from, to)| [x, from, x, to]),
            );
        }
        outline
    }
}

/// The distinct values of `coordinates`, in order.
fn cuts(coordinates: impl Iterator<Item = u16>) -> Vec<u16> {
    let mut cuts: Vec<u16> = coordinates.collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

/// The stretches of a line cut at `cuts` where `inside` holds, each from
/// the cut it starts at to the cut it ends at. `inside(piece)` is asked
/// about the piece between cut `piece` and the next.
fn runs(cuts: &[u16], inside: impl Fn(usize) -> bool) -> Vec<(u16, u16)> {
    let mut runs: Vec<(u16, u16)> = Vec::new();
    for (piece, ends) in cuts.windows(2).enumerate() {
        let &[start, end] = ends else { continue };
        if !inside(piece) {
            continue;
        }
        match runs.last_mut() {
            // The piece before this one was inside too.
            Some(run) if run.1 == start => run.1 = end,
            _ => runs.push((start, end)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(rects: &[[u16; 4]]) -> u64 {
        rects
            .iter()
            .map(|&[x0, y0, x1, y1]| u64::from(x1 - x0) * u64::from(y1 - y0))
            .sum()
    }

    fn overlap(a: [u16; 4], b: [u16; 4]) -> bool {
        a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
    }

    fn assert_disjoint(fill: &[[u16; 4]]) {
        for (index, &a) in fill.iter().enumerate() {
            for &b in &fill[index + 1..] {
                assert!(!overlap(a, b), "{a:?} and {b:?} overlap in {fill:?}");
            }
        }
    }

    #[test]
    fn one_rectangle_is_its_own_shape() {
        let shape = Shape::of(&[[10, 20, 30, 60]]);
        assert_eq!(shape.fill, [[10, 20, 30, 60]]);
        assert_eq!(
            shape.outline,
            [
                [10, 20, 30, 20],
                [10, 60, 30, 60],
                [10, 20, 10, 60],
                [30, 20, 30, 60],
            ]
        );
    }

    #[test]
    fn nothing_and_rectangles_with_no_area_have_no_shape() {
        assert_eq!(Shape::of(&[]), Shape::default());
        let flat = [[10, 20, 30, 20], [10, 20, 10, 60], [30, 20, 10, 60]];
        assert_eq!(Shape::of(&flat), Shape::default());
    }

    /// Two lines of a paragraph set tight: the second rectangle starts
    /// above where the first ends. No line of the outline runs through
    /// either, and the overlap is covered once.
    #[test]
    fn overlapping_lines_are_one_region_with_one_edge() {
        let indented = [1_000, 1_000, 5_000, 1_200];
        let full = [500, 1_150, 6_000, 1_350];
        let shape = Shape::of(&[indented, full]);

        assert_eq!(
            shape.fill,
            [[1_000, 1_000, 5_000, 1_150], [500, 1_150, 6_000, 1_350]]
        );
        assert_eq!(
            shape.outline,
            [
                [1_000, 1_000, 5_000, 1_000],
                // The step from the indented line out to the full one,
                // on each side: nothing between 1,000 and 5,000.
                [500, 1_150, 1_000, 1_150],
                [5_000, 1_150, 6_000, 1_150],
                [500, 1_350, 6_000, 1_350],
                [500, 1_150, 500, 1_350],
                [1_000, 1_000, 1_000, 1_150],
                [5_000, 1_000, 5_000, 1_150],
                [6_000, 1_150, 6_000, 1_350],
            ]
        );
    }

    #[test]
    fn rectangles_apart_keep_an_edge_each() {
        let left_column = [100, 100, 400, 200];
        let right_column = [600, 100, 900, 200];
        let shape = Shape::of(&[left_column, right_column]);
        assert_eq!(shape.fill, [left_column, right_column]);
        assert_eq!(shape.outline.len(), 8, "{:?}", shape.outline);
    }

    #[test]
    fn rectangles_that_only_touch_join() {
        let shape = Shape::of(&[[0, 0, 10, 10], [10, 0, 20, 10], [0, 10, 20, 20]]);
        assert_eq!(shape.fill, [[0, 0, 20, 20]]);
        assert_eq!(
            shape.outline,
            [
                [0, 0, 20, 0],
                [0, 20, 20, 20],
                [0, 0, 0, 20],
                [20, 0, 20, 20]
            ]
        );
    }

    #[test]
    fn a_cross_is_covered_once_and_edged_all_the_way_round() {
        let upright = [20, 0, 40, 60];
        let level = [0, 20, 60, 40];
        let shape = Shape::of(&[upright, level]);

        assert_eq!(
            shape.fill,
            [[20, 0, 40, 20], [0, 20, 60, 40], [20, 40, 40, 60]]
        );
        assert_disjoint(&shape.fill);
        // Both arms, less the square they share.
        assert_eq!(area(&shape.fill), 20 * 60 + 60 * 20 - 20 * 20);
        assert_eq!(shape.outline.len(), 12, "{:?}", shape.outline);
        // The outline is as long as the cross's edge: twelve sides of 20.
        let length: u64 = shape
            .outline
            .iter()
            .map(|&[x0, y0, x1, y1]| u64::from(x1 - x0) + u64::from(y1 - y0))
            .sum();
        assert_eq!(length, 12 * 20);
    }

    /// Whatever the rectangles, the fill is the area they cover (counted
    /// here point by point) with no part covered twice.
    #[test]
    fn the_fill_is_the_area_covered_however_the_rectangles_lie() {
        let rects = [
            [0, 0, 7, 5],
            [3, 2, 12, 9],
            [3, 2, 12, 9],
            [5, 4, 6, 20],
            [15, 15, 18, 16],
            [11, 8, 16, 16],
        ];
        let mut covered = 0_u64;
        for y in 0..24_u16 {
            for x in 0..24_u16 {
                let inside = |&[x0, y0, x1, y1]: &[u16; 4]| x0 <= x && x < x1 && y0 <= y && y < y1;
                covered += u64::from(rects.iter().any(inside));
            }
        }
        let shape = Shape::of(&rects);
        assert_disjoint(&shape.fill);
        assert_eq!(area(&shape.fill), covered);
    }
}
