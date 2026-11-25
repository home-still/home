//! The order the lines of a page are read in.
//!
//! Text detection finds a page's lines and where each one is, not which
//! follows which. Sorting them top to bottom reads straight across the
//! columns of a two-column page, so a sentence that runs down one column is
//! no longer a run of consecutive words, and a quote of it cannot be found
//! on the page. [`reading_order`] cuts the page at its column gutters
//! instead: a recursive X-Y cut (Nagy, Seth & Viswanathan 1992,
//! DOI 10.1109/2.144436) made on the lines' bounding boxes rather than on
//! pixels (Ha, Haralick & Phillips 1995, DOI 10.1109/ICDAR.1995.602059).
//!
//! 1. Find the gutter: the vertical strip with at least
//!    [`MIN_COLUMN_LINES`] lines wholly on each side of it that the fewest
//!    lines cross.
//! 2. No line crosses it: read everything left of it, then everything
//!    right of it, each by these same rules.
//! 3. Lines cross it (a title or a caption set across both columns): they
//!    cut the page into horizontal bands. Read the bands from the top,
//!    each by these same rules, and each crossing line between the two
//!    bands it separates.
//! 4. There is no gutter: read row by row from the top, and left to right
//!    within a row.

/// A line's box in page pixels, `[x0, y0, x1, y1]`, from the top-left corner.
pub type LineBox = [f32; 4];

/// Fewest lines on each side of a gutter. Two lines set side by side are a
/// table row or a heading with its page number, not two columns.
pub const MIN_COLUMN_LINES: usize = 3;

/// Narrowest gutter, as a fraction of the page's median line height.
/// Detected boxes are padded, so the white between two columns measures
/// well under its printed width; the space between two words of one line
/// is narrower still.
pub const MIN_GUTTER_FRACTION_OF_LINE_HEIGHT: f32 = 0.2;

/// Two lines are on one row when they overlap vertically by at least this
/// fraction of the shorter one's height.
pub const SAME_ROW_MIN_VERTICAL_OVERLAP: f32 = 0.5;

fn height(line: &LineBox) -> f32 {
    line[3] - line[1]
}

fn centre_y(line: &LineBox) -> f32 {
    (line[1] + line[3]) / 2.0
}

/// The indices of `lines` in the order a reader takes them.
pub fn reading_order(lines: &[LineBox]) -> Vec<usize> {
    let mut heights: Vec<f32> = lines.iter().map(height).collect();
    heights.sort_by(f32::total_cmp);
    let Some(median_height) = heights.get(heights.len() / 2) else {
        return Vec::new();
    };
    let gutter_width = MIN_GUTTER_FRACTION_OF_LINE_HEIGHT * median_height;

    let mut order = Vec::with_capacity(lines.len());
    read(lines, (0..lines.len()).collect(), gutter_width, &mut order);
    order
}

/// A vertical strip and the lines on each side of it.
struct Gutter {
    left: Vec<usize>,
    right: Vec<usize>,
    crossing: Vec<usize>,
}

impl Gutter {
    /// Split `members` at the strip `gutter_width` wide that starts at `x`.
    fn at(lines: &[LineBox], members: &[usize], x: f32, gutter_width: f32) -> Self {
        let mut gutter = Self {
            left: Vec::new(),
            right: Vec::new(),
            crossing: Vec::new(),
        };
        for &member in members {
            let [x0, _, x1, _] = lines[member];
            if x1 <= x {
                gutter.left.push(member);
            } else if x0 >= x + gutter_width {
                gutter.right.push(member);
            } else {
                gutter.crossing.push(member);
            }
        }
        gutter
    }

    fn separates_columns(&self) -> bool {
        self.left.len() >= MIN_COLUMN_LINES && self.right.len() >= MIN_COLUMN_LINES
    }

    /// The smaller of its two sides: of two gutters crossed by as many
    /// lines, the one that splits the page more evenly is the columns'.
    fn smaller_side(&self) -> usize {
        self.left.len().min(self.right.len())
    }
}

/// The gutter of `members`, if they are set in columns. A gutter starts
/// where a line ends, so the right edges are every strip worth trying.
fn find_gutter(lines: &[LineBox], members: &[usize], gutter_width: f32) -> Option<Gutter> {
    members
        .iter()
        .map(|&member| Gutter::at(lines, members, lines[member][2], gutter_width))
        .filter(Gutter::separates_columns)
        .min_by(|a, b| {
            a.crossing
                .len()
                .cmp(&b.crossing.len())
                .then(b.smaller_side().cmp(&a.smaller_side()))
        })
}

/// Append `members` to `order` as they are read.
fn read(lines: &[LineBox], members: Vec<usize>, gutter_width: f32, order: &mut Vec<usize>) {
    let Some(mut gutter) = find_gutter(lines, &members, gutter_width) else {
        order.extend(by_rows(lines, members));
        return;
    };
    if gutter.crossing.is_empty() {
        read(lines, gutter.left, gutter_width, order);
        read(lines, gutter.right, gutter_width, order);
        return;
    }

    // Each side holds fewer lines than `members`, and so does every band
    // (the crossing lines are in none), so the recursion ends.
    gutter
        .crossing
        .sort_by(|&a, &b| centre_y(&lines[a]).total_cmp(&centre_y(&lines[b])));
    let mut bands = vec![Vec::new(); gutter.crossing.len() + 1];
    for member in gutter.left.into_iter().chain(gutter.right) {
        let crossing_lines_above = gutter
            .crossing
            .iter()
            .filter(|&&crossing| centre_y(&lines[crossing]) <= centre_y(&lines[member]))
            .count();
        bands[crossing_lines_above].push(member);
    }
    let mut crossing = gutter.crossing.into_iter();
    for band in bands {
        read(lines, band, gutter_width, order);
        order.extend(crossing.next());
    }
}

/// Whether `a` and `b` sit on one row of text.
fn on_one_row(a: &LineBox, b: &LineBox) -> bool {
    let overlap = a[3].min(b[3]) - a[1].max(b[1]);
    overlap >= SAME_ROW_MIN_VERTICAL_OVERLAP * height(a).min(height(b))
}

/// `members` row by row from the top, left to right within a row.
fn by_rows(lines: &[LineBox], mut members: Vec<usize>) -> Vec<usize> {
    members.sort_by(|&a, &b| centre_y(&lines[a]).total_cmp(&centre_y(&lines[b])));
    let mut order = Vec::with_capacity(members.len());
    let mut row: Vec<usize> = Vec::new();
    for member in members {
        if let Some(&first) = row.first() {
            if !on_one_row(&lines[first], &lines[member]) {
                order.extend(in_x(lines, &mut row));
            }
        }
        row.push(member);
    }
    order.extend(in_x(lines, &mut row));
    order
}

/// Take the lines of `row`, leftmost first.
fn in_x(lines: &[LineBox], row: &mut Vec<usize>) -> Vec<usize> {
    row.sort_by(|&a, &b| lines[a][0].total_cmp(&lines[b][0]));
    std::mem::take(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Height of a test line and the pitch from one line to the next.
    const LINE_HEIGHT: f32 = 20.0;
    const LINE_PITCH: f32 = 30.0;

    /// A line from `x0` to `x1` on row `row` of the page.
    fn line(x0: f32, x1: f32, row: usize) -> LineBox {
        let y0 = row as f32 * LINE_PITCH;
        [x0, y0, x1, y0 + LINE_HEIGHT]
    }

    /// `rows` lines of a column from `x0` to `x1`, starting at row `first`.
    fn column(x0: f32, x1: f32, first: usize, rows: usize) -> Vec<LineBox> {
        (first..first + rows).map(|row| line(x0, x1, row)).collect()
    }

    /// Read `lines` after shuffling them, and name each by its index in
    /// the unshuffled list: the order detection reports lines in must not
    /// matter.
    fn read_shuffled(lines: &[LineBox]) -> Vec<usize> {
        const STRIDE: usize = 7;
        assert_ne!(
            lines.len() % STRIDE,
            0,
            "the stride must not divide the count"
        );
        let shuffled_from: Vec<usize> = (0..lines.len())
            .map(|i| (i * STRIDE) % lines.len())
            .collect();
        let shuffled: Vec<LineBox> = shuffled_from.iter().map(|&i| lines[i]).collect();
        reading_order(&shuffled)
            .into_iter()
            .map(|i| shuffled_from[i])
            .collect()
    }

    #[test]
    fn a_single_column_is_read_top_to_bottom() {
        let mut page = column(100.0, 900.0, 0, 10);
        // A paragraph's short last line and an indented first line.
        page[4] = line(100.0, 300.0, 4);
        page[5] = line(140.0, 900.0, 5);
        assert_eq!(read_shuffled(&page), (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn two_columns_are_read_one_after_the_other() {
        let mut page = column(100.0, 480.0, 0, 6);
        page.extend(column(500.0, 880.0, 0, 6));
        assert_eq!(read_shuffled(&page), (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn a_title_across_both_columns_is_read_before_them_and_a_footer_after() {
        let mut page = vec![line(150.0, 830.0, 0), line(300.0, 680.0, 1)];
        page.extend(column(100.0, 480.0, 2, 5));
        page.extend(column(500.0, 880.0, 2, 5));
        page.push(line(470.0, 510.0, 8));
        assert_eq!(read_shuffled(&page), (0..13).collect::<Vec<_>>());
    }

    #[test]
    fn a_caption_across_the_page_separates_the_columns_above_it_from_those_below() {
        let mut page = column(100.0, 480.0, 0, 4);
        page.extend(column(500.0, 880.0, 0, 4));
        page.push(line(100.0, 880.0, 4));
        page.extend(column(100.0, 480.0, 5, 3));
        page.extend(column(500.0, 880.0, 5, 3));
        assert_eq!(read_shuffled(&page), (0..15).collect::<Vec<_>>());
    }

    #[test]
    fn a_column_shorter_than_its_neighbour_is_still_a_column() {
        let mut page = column(100.0, 480.0, 0, 9);
        page.extend(column(500.0, 880.0, 0, 3));
        assert_eq!(read_shuffled(&page), (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn one_row_detected_as_two_boxes_is_read_left_to_right() {
        let mut page = column(100.0, 900.0, 0, 3);
        // The same row twice over: a heading and its page number.
        page.push(line(100.0, 400.0, 3));
        page.push(line(800.0, 900.0, 3));
        page.extend(column(100.0, 900.0, 4, 4));
        assert_eq!(read_shuffled(&page), (0..9).collect::<Vec<_>>());
    }

    #[test]
    fn a_gutter_narrower_than_the_padding_allows_is_not_a_gutter() {
        // Two "columns" a hair apart: one row of each is one line of text.
        let gap = MIN_GUTTER_FRACTION_OF_LINE_HEIGHT * LINE_HEIGHT / 2.0;
        let rows = 4;
        let page: Vec<LineBox> = (0..rows)
            .flat_map(|row| [line(100.0, 500.0, row), line(500.0 + gap, 900.0, row)])
            .collect();
        assert_eq!(read_shuffled(&page), (0..2 * rows).collect::<Vec<_>>());
    }

    #[test]
    fn a_page_without_lines_has_no_order() {
        assert!(reading_order(&[]).is_empty());
    }
}
