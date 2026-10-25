// SPDX-License-Identifier: GPL-3.0-or-later
//! Where text lands on the sheet (Phase 9).
//!
//! KiCad's API does not report the extent of schematic text, so the area a piece of text covers is
//! estimated from its string, size, angle, and justification. The estimate is a little generous:
//! the stroke font is about 0.9 of the text height wide per character on average (digits and capitals
//! are wider, lowercase narrower), and a line is about 1.2 text heights tall.

use serde::Serialize;

use crate::geometry::{Dir, Nm, Orientation, Point, Rect};

/// The text height KiCad uses unless a field says otherwise.
pub const DEFAULT_SIZE: Nm = Nm(1_270_000);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HAlign {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VAlign {
    Top,
    #[default]
    Center,
    Bottom,
}

/// The area `text` covers when anchored at `anchor`, turned `angle_deg` counter-clockwise.
pub fn text_box(text: &str, anchor: Point, size: Nm, angle_deg: u16, h: HAlign, v: VAlign) -> Rect {
    let lines = text.lines().count().max(1) as i64;
    let chars = text.lines().map(|l| l.chars().count()).max().unwrap_or(1).max(1) as i64;
    let width = chars * size.0 * 9 / 10;
    let height = lines * size.0 * 6 / 5;
    let (x0, x1) = match h {
        HAlign::Left => (0, width),
        HAlign::Center => (-width / 2, width / 2),
        HAlign::Right => (-width, 0),
    };
    // y grows downward: text anchored at its bottom sits above the anchor.
    let (y0, y1) = match v {
        VAlign::Top => (0, height),
        VAlign::Center => (-height / 2, height / 2),
        VAlign::Bottom => (-height, 0),
    };
    let turn = Orientation {
        rotation_deg: angle_deg % 360,
        ..Default::default()
    };
    let corner = |x: i64, y: i64| anchor + turn.apply(Point::new(Nm(x), Nm(y))).unwrap_or(Point::new(Nm(x), Nm(y)));
    Rect::from_points([corner(x0, y0), corner(x1, y0), corner(x0, y1), corner(x1, y1)]).expect("four corners")
}

/// The area a label covers: its text runs from the anchor in `direction`, sitting beside the wire
/// it hangs on (above a horizontal run, to the side of a vertical one).
pub fn label_box(text: &str, anchor: Point, size: Nm, direction: Dir) -> Rect {
    match direction {
        Dir::Right => text_box(text, anchor, size, 0, HAlign::Left, VAlign::Bottom),
        Dir::Left => text_box(text, anchor, size, 0, HAlign::Right, VAlign::Bottom),
        Dir::Up => text_box(text, anchor, size, 90, HAlign::Left, VAlign::Bottom),
        Dir::Down => text_box(text, anchor, size, 270, HAlign::Left, VAlign::Bottom),
    }
}

/// The same box, a hair smaller, so that text touching a wire or another box edge is not an overlap.
pub fn shrunk(r: Rect) -> Rect {
    shrunk_by(r, Nm(150_000))
}

/// The box smaller by `by` on every side (unchanged when that would leave nothing).
pub fn shrunk_by(r: Rect, by: Nm) -> Rect {
    if r.width().0 <= 2 * by.0 || r.height().0 <= 2 * by.0 {
        return r;
    }
    r.inflate(Nm(-by.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn justification_decides_which_side_of_the_anchor_the_text_is_on() {
        let at = Point::new(Nm(10_000_000), Nm(10_000_000));
        let left = text_box("ABCD", at, DEFAULT_SIZE, 0, HAlign::Left, VAlign::Center);
        let right = text_box("ABCD", at, DEFAULT_SIZE, 0, HAlign::Right, VAlign::Center);
        assert_eq!(left.min.x, at.x);
        assert_eq!(right.max.x, at.x);
        assert_eq!(left.width(), right.width());
    }

    #[test]
    fn a_quarter_turn_swaps_width_and_height_and_runs_upward() {
        let at = Point::new(Nm(0), Nm(0));
        let flat = text_box("ABCD", at, DEFAULT_SIZE, 0, HAlign::Left, VAlign::Bottom);
        let up = text_box("ABCD", at, DEFAULT_SIZE, 90, HAlign::Left, VAlign::Bottom);
        assert_eq!((up.width(), up.height()), (flat.height(), flat.width()));
        assert!(up.min.y < at.y && up.max.y <= at.y, "text at 90 degrees reads upward");
    }

    #[test]
    fn longer_text_is_wider_and_lines_stack() {
        let at = Point::default();
        let one = text_box("AB", at, DEFAULT_SIZE, 0, HAlign::Left, VAlign::Top);
        let two = text_box("ABCD", at, DEFAULT_SIZE, 0, HAlign::Left, VAlign::Top);
        assert!(two.width() > one.width());
        let lines = text_box("AB\nCD", at, DEFAULT_SIZE, 0, HAlign::Left, VAlign::Top);
        assert!(lines.height() > one.height());
    }
}
