//! Place labels: their layout, the greedy choice of which ones show, and the
//! glyph outlines they are drawn from.
//!
//! The basemap builder chooses the labels of each zoom over the whole world
//! with [`select`], using the same font and layout as the renderer, and puts
//! only the winners into the tiles. The renderer then draws every label a
//! tile carries without deciding anything itself, so neighbouring tiles
//! always agree on what shows and a label cut by a tile edge continues
//! seamlessly in the next tile.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ab_glyph::{Font, FontRef, GlyphId, OutlineCurve};
use tiny_skia::{Path, PathBuilder, Transform};

/// Inter Medium, under the SIL Open Font License (see `fonts/Inter-OFL.txt`).
const FONT_BYTES: &[u8] = include_bytes!("../fonts/Inter-Medium.ttf");

/// How far around its tile a tile carries labels, in logical pixels. A label
/// is never wider than this, so one anchored just outside the tile but
/// reaching into it is still drawn.
pub const LABEL_BUFFER_PX: f32 = 160.0;

/// Space kept free around each label, in logical pixels.
pub const LABEL_PADDING_PX: f32 = 7.0;

/// Gap between a place's dot and its name.
const DOT_GAP_PX: f32 = 4.0;

/// The longest name drawn, in characters; longer ones are left out.
pub const MAX_NAME_CHARS: usize = 24;

fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_BYTES).expect("the embedded font parses"))
}

/// The size class of a place: 0 for the largest cities, up to 3 for towns.
/// Sets the text size and the dot.
pub fn text_size(class: u8) -> f32 {
    match class {
        0 => 14.0,
        1 => 13.0,
        2 => 12.0,
        _ => 11.0,
    }
}

/// Radius of the dot marking a place of `class`, in logical pixels.
pub fn dot_radius(class: u8) -> f32 {
    match class {
        0 => 3.0,
        1 => 2.6,
        _ => 2.2,
    }
}

/// Which side of its dot a name is written on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Right = 0,
    Left = 1,
}

impl Side {
    pub fn from_number(n: f64) -> Self {
        if n == 1.0 { Side::Left } else { Side::Right }
    }
}

/// An axis-aligned box in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    fn padded(&self, pad: f32) -> Rect {
        Rect {
            x0: self.x0 - pad,
            y0: self.y0 - pad,
            x1: self.x1 + pad,
            y1: self.y1 + pad,
        }
    }
}

fn scale_for(size: f32) -> f32 {
    size / font().units_per_em().unwrap_or(2048.0)
}

/// The width of `text` at `size` logical pixels per em.
pub fn text_width(text: &str, size: f32) -> f32 {
    let font = font();
    let mut width = 0.0;
    let mut previous: Option<GlyphId> = None;
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(previous) = previous {
            width += font.kern_unscaled(previous, id);
        }
        width += font.h_advance_unscaled(id);
        previous = Some(id);
    }
    width * scale_for(size)
}

/// Where the text of a label starts (left end of the baseline), relative to
/// its anchor, and the label's box (dot and text) relative to its anchor.
pub fn layout(name: &str, class: u8, side: Side) -> ((f32, f32), Rect) {
    let size = text_size(class);
    let width = text_width(name, size);
    let radius = dot_radius(class);
    let font = font();
    let scale = scale_for(size);
    let ascent = font.ascent_unscaled() * scale;
    let descent = font.descent_unscaled() * scale;
    // Centre the cap height on the dot.
    let cap = size * 0.727;
    let baseline = cap / 2.0;
    let x = match side {
        Side::Right => radius + DOT_GAP_PX,
        Side::Left => -(radius + DOT_GAP_PX + width),
    };
    let text_box = Rect {
        x0: x,
        y0: baseline - ascent * 0.85,
        x1: x + width,
        y1: baseline - descent * 0.5,
    };
    let rect = Rect {
        x0: text_box.x0.min(-radius),
        y0: text_box.y0.min(-radius),
        x1: text_box.x1.max(radius),
        y1: text_box.y1.max(radius),
    };
    ((x, baseline), rect)
}

/// A place that may get a label.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Position in logical pixels of one common plane (world pixels at the
    /// zoom being labelled).
    pub x: f32,
    pub y: f32,
    pub name: String,
    pub class: u8,
    /// The side the label must take, for a label kept from the previous
    /// zoom; `None` tries the right, then the left.
    pub side: Option<Side>,
}

/// A label that won its place.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    /// Index into the candidates given to [`select`].
    pub index: usize,
    pub side: Side,
    /// The label's box in the candidates' plane, unpadded.
    pub rect: Rect,
}

/// Choose labels greedily in the order given (most important first): each
/// one shows, to the right of its dot or else to the left, if its padded box
/// stays clear of every label already shown and inside `bounds` (the
/// world map's edges, say). No two chosen boxes overlap.
/// Labels wider than [`LABEL_BUFFER_PX`] never show, so a tile carrying the
/// places within that buffer has every label that reaches into it.
///
/// The labels of one zoom, put first with their side fixed, all show again
/// at the next zoom (distances double, boxes stay the same size), which is
/// how labels stay put while zooming in.
pub fn select(candidates: &[Candidate], bounds: Option<Rect>) -> Vec<Placed> {
    const CELL: f32 = 64.0;
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    let mut placed: Vec<Placed> = Vec::new();
    let cells = |r: &Rect| {
        let (cx0, cy0) = ((r.x0 / CELL).floor() as i32, (r.y0 / CELL).floor() as i32);
        let (cx1, cy1) = ((r.x1 / CELL).floor() as i32, (r.y1 / CELL).floor() as i32);
        (cx0..=cx1).flat_map(move |cx| (cy0..=cy1).map(move |cy| (cx, cy)))
    };
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.name.is_empty() || candidate.name.chars().count() > MAX_NAME_CHARS {
            continue;
        }
        let sides = match candidate.side {
            Some(side) => vec![side],
            None => vec![Side::Right, Side::Left],
        };
        for side in sides {
            let (_, local) = layout(&candidate.name, candidate.class, side);
            if local.x1 - local.x0 > LABEL_BUFFER_PX {
                break;
            }
            let rect = Rect {
                x0: candidate.x + local.x0,
                y0: candidate.y + local.y0,
                x1: candidate.x + local.x1,
                y1: candidate.y + local.y1,
            };
            if bounds.is_some_and(|b| {
                rect.x0 < b.x0 || rect.y0 < b.y0 || rect.x1 > b.x1 || rect.y1 > b.y1
            }) {
                continue;
            }
            let padded = rect.padded(LABEL_PADDING_PX);
            let clear = cells(&padded).all(|cell| {
                grid.get(&cell).is_none_or(|ids| {
                    ids.iter()
                        .all(|&i| !placed[i].rect.padded(LABEL_PADDING_PX).intersects(&rect))
                })
            });
            if clear {
                let id = placed.len();
                for cell in cells(&padded) {
                    grid.entry(cell).or_default().push(id);
                }
                placed.push(Placed { index, side, rect });
                break;
            }
        }
    }
    placed
}

/// The outline of one glyph in font units (y up), cached.
fn glyph_path(id: GlyphId) -> Option<Path> {
    static CACHE: OnceLock<Mutex<HashMap<u16, Option<Path>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(id.0)
        .or_insert_with(|| {
            let outline = font().outline(id)?;
            let mut builder = PathBuilder::new();
            let mut last: Option<ab_glyph::Point> = None;
            for curve in &outline.curves {
                let start = match curve {
                    OutlineCurve::Line(p, _)
                    | OutlineCurve::Quad(p, _, _)
                    | OutlineCurve::Cubic(p, _, _, _) => *p,
                };
                if last != Some(start) {
                    if last.is_some() {
                        builder.close();
                    }
                    builder.move_to(start.x, start.y);
                }
                last = Some(match curve {
                    OutlineCurve::Line(_, p) => {
                        builder.line_to(p.x, p.y);
                        *p
                    }
                    OutlineCurve::Quad(_, c, p) => {
                        builder.quad_to(c.x, c.y, p.x, p.y);
                        *p
                    }
                    OutlineCurve::Cubic(_, c1, c2, p) => {
                        builder.cubic_to(c1.x, c1.y, c2.x, c2.y, p.x, p.y);
                        *p
                    }
                });
            }
            if last.is_some() {
                builder.close();
            }
            builder.finish()
        })
        .clone()
}

/// The outline of `text` at `size`, its baseline starting at `origin`, in
/// the coordinates of `transform`.
pub(crate) fn text_path(
    text: &str,
    size: f32,
    origin: (f32, f32),
    transform: Transform,
) -> Option<Path> {
    let font = font();
    let scale = scale_for(size);
    let mut builder = PathBuilder::new();
    let mut pen = 0.0;
    let mut previous: Option<GlyphId> = None;
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(previous) = previous {
            pen += font.kern_unscaled(previous, id);
        }
        if id.0 != 0
            && let Some(path) = glyph_path(id)
        {
            let glyph =
                Transform::from_row(scale, 0.0, 0.0, -scale, origin.0 + pen * scale, origin.1)
                    .post_concat(transform);
            if let Some(path) = path.transform(glyph) {
                builder.push_path(&path);
            }
        }
        pen += font.h_advance_unscaled(id);
        previous = Some(id);
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wider_text_measures_wider() {
        assert!(text_width("Berlin", 12.0) > text_width("Bonn", 12.0));
        assert!(text_width("Berlin", 14.0) > text_width("Berlin", 12.0));
        let w = text_width("Berlin", 12.0);
        assert!((25.0..50.0).contains(&w), "{w}");
    }

    #[test]
    fn chosen_labels_never_overlap() {
        // A dense cluster: a grid of cities 6 px apart, most must lose.
        let mut candidates = Vec::new();
        for i in 0..40 {
            for j in 0..40 {
                candidates.push(Candidate {
                    x: i as f32 * 6.0,
                    y: j as f32 * 6.0,
                    name: format!("Town {i}-{j}"),
                    class: ((i + j) % 4) as u8,
                    side: None,
                });
            }
        }
        let placed = select(&candidates, None);
        assert!(placed.len() > 3 && placed.len() < candidates.len() / 10);
        for (n, a) in placed.iter().enumerate() {
            for b in &placed[n + 1..] {
                assert!(!a.rect.intersects(&b.rect), "{a:?} overlaps {b:?}");
            }
        }
        // The first candidate always wins, on its preferred side.
        assert_eq!(placed[0].index, 0);
        assert_eq!(placed[0].side, Side::Right);
    }

    #[test]
    fn a_blocked_right_side_falls_back_to_the_left() {
        let candidates = vec![
            Candidate {
                x: 36.0,
                y: 0.0,
                name: "Alpha".into(),
                class: 1,
                side: None,
            },
            Candidate {
                x: 0.0,
                y: 0.0,
                name: "Beta".into(),
                class: 2,
                side: None,
            },
        ];
        let placed = select(&candidates, None);
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[1].side, Side::Left);
    }

    #[test]
    fn text_outlines_exist() {
        let path = text_path("Köln", 12.0, (0.0, 0.0), Transform::identity()).unwrap();
        let b = path.bounds();
        assert!(b.width() > 15.0 && b.height() > 7.0, "{b:?}");
    }
}
