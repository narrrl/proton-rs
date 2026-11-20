//! Shared justified-row layout adapted from `pdfs-gui/src/app/pages/photos.rs`.
//! Copyright (c) 2026 Nils Pukropp. MIT; see third-party/NILS-PUKROPP-MIT.txt.
//! Pure geometry: bounded input, no decoding, filesystem, or network work.
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridRow {
    pub start: usize,
    pub tiles: Vec<TileSize>,
}

pub fn tile_ratio(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value.clamp(0.5, 2.5)
    } else {
        1.0
    }
}

/// All units are pixels. Closed rows fill the width exactly; a short final row
/// keeps the target height. A section/day should be planned independently.
pub fn plan_justified(ratios: &[f64], width: u32, target: u32, gap: u32) -> Result<Vec<GridRow>> {
    if ratios.len() > 2048 {
        return Err(Error::Invalid("grid geometry exceeds bounds".into()));
    }
    check_geometry(width, target, gap)?;
    let ratios: Vec<f64> = ratios.iter().map(|r| tile_ratio(*r)).collect();
    let mut rows = Vec::new();
    plan_rows(&ratios, width, target, gap, |start, widths, height| {
        rows.push(GridRow {
            start,
            tiles: widths
                .iter()
                .map(|&width| TileSize { width, height })
                .collect(),
        })
    });
    Ok(rows)
}

/// The most items [`plan_sections`] takes: as many as a view outline holds.
pub const SECTIONS_MAX_ITEMS: usize = 1_000_000;

/// Every row of a whole view, in flat arrays so a large view crosses to the
/// app in one piece.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlannedGrid {
    /// The first item of each row.
    pub row_starts: Vec<u32>,
    /// The height of each row.
    pub row_heights: Vec<u32>,
    /// The width of each item, in item order.
    pub widths: Vec<u32>,
}

/// Plan every row of `ratios`, each section on its own. `sections` are the
/// items where a section starts, ascending; the first section starts at item
/// 0 whether or not it is listed. Gives the same rows as [`plan_justified`]
/// over each section.
pub fn plan_sections(
    ratios: &[f64],
    sections: &[u32],
    width: u32,
    target: u32,
    gap: u32,
) -> Result<PlannedGrid> {
    if ratios.len() > SECTIONS_MAX_ITEMS || sections.len() > ratios.len() + 1 {
        return Err(Error::Invalid("grid geometry exceeds bounds".into()));
    }
    check_geometry(width, target, gap)?;
    if sections.windows(2).any(|pair| pair[0] >= pair[1])
        || sections
            .last()
            .is_some_and(|&last| last as usize > ratios.len())
    {
        return Err(Error::Invalid("grid sections are out of order".into()));
    }
    let ratios: Vec<f64> = ratios.iter().map(|r| tile_ratio(*r)).collect();
    let mut grid = PlannedGrid {
        widths: Vec::with_capacity(ratios.len()),
        ..PlannedGrid::default()
    };
    let mut bounds: Vec<usize> = sections
        .iter()
        .map(|&s| s as usize)
        .filter(|&s| s > 0)
        .collect();
    bounds.insert(0, 0);
    bounds.push(ratios.len());
    for pair in bounds.windows(2) {
        let (from, until) = (pair[0], pair[1]);
        if from == until {
            continue;
        }
        plan_rows(
            &ratios[from..until],
            width,
            target,
            gap,
            |start, widths, height| {
                grid.row_starts.push((from + start) as u32);
                grid.row_heights.push(height);
                grid.widths.extend_from_slice(widths);
            },
        );
    }
    Ok(grid)
}

fn check_geometry(width: u32, target: u32, gap: u32) -> Result<()> {
    if width == 0 || width > 32_768 || target == 0 || target > 8_192 || gap > 256 {
        return Err(Error::Invalid("grid geometry exceeds bounds".into()));
    }
    Ok(())
}

/// The justified rows of `ratios` (already passed through [`tile_ratio`]):
/// calls `row` with each row's first item, tile widths and height.
fn plan_rows(
    ratios: &[f64],
    width: u32,
    target: u32,
    gap: u32,
    mut row_out: impl FnMut(usize, &[u32], u32),
) {
    // Even at a one-pixel viewport, gaps cannot consume the entire row.
    let max_tiles = (((width as u64 + gap as u64) / (gap as u64 + 1)) as usize).clamp(1, 64);
    let fit = |row: &[f64]| {
        (f64::from(width) - f64::from(gap) * (row.len() - 1) as f64).max(1.0)
            / row.iter().sum::<f64>()
    };
    let target = f64::from(target);
    let mut widths = Vec::with_capacity(max_tiles);
    let mut start = 0;
    while start < ratios.len() {
        let mut end = start + 1;
        while end < ratios.len() && end - start < max_tiles && fit(&ratios[start..end]) > target {
            end += 1;
        }
        let full =
            fit(&ratios[start..end]) <= target || (end - start == max_tiles && end < ratios.len());
        if full
            && end - start > 1
            && fit(&ratios[start..end - 1]) - target < target - fit(&ratios[start..end])
        {
            end -= 1;
        }
        let row = &ratios[start..end];
        let height = if full { fit(row) } else { target.min(fit(row)) };
        let tile_height = (height.round() as u32).max(1);
        let total: f64 = row.iter().sum();
        let available = width - gap * (row.len() as u32 - 1);
        let content = if full {
            available
        } else {
            (total * height).round().max(row.len() as f64) as u32
        }
        .min(available);
        let mut done = 0.0;
        let mut left = 0;
        widths.clear();
        for (index, ratio) in row.iter().enumerate() {
            done += ratio;
            // Reserve one pixel for every remaining tile. Cumulative
            // rounding prevents gaps/overflow at fractional screen densities.
            let right = ((f64::from(content) * done / total).round() as u32)
                .clamp(left + 1, content - (row.len() - index - 1) as u32);
            widths.push(right - left);
            left = right;
        }
        row_out(start, &widths, tile_height);
        start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_plan_like_each_section_on_its_own() {
        let ratios: Vec<_> = (0..5000)
            .map(|i| [1.5, 0.67, 1.0, 1.78, 0.75, 1.33][i % 6] + (i % 11) as f64 * 0.01)
            .collect();
        let sections = [0u32, 7, 9, 2600, 4999];
        let grid = plan_sections(&ratios, &sections, 1080, 300, 4).unwrap();
        let mut expected = PlannedGrid::default();
        let mut bounds: Vec<usize> = sections.iter().map(|&s| s as usize).collect();
        bounds.push(ratios.len());
        for pair in bounds.windows(2) {
            let mut start = pair[0];
            // plan_justified takes at most 2048 items; its rows only look
            // ahead, so pieces keep all but their last row.
            while start < pair[1] {
                let end = pair[1].min(start + 2048);
                let rows = plan_justified(&ratios[start..end], 1080, 300, 4).unwrap();
                let keep = if end == pair[1] {
                    rows.len()
                } else {
                    rows.len() - 1
                };
                for row in &rows[..keep] {
                    expected.row_starts.push((start + row.start) as u32);
                    expected.row_heights.push(row.tiles[0].height);
                    expected.widths.extend(row.tiles.iter().map(|t| t.width));
                }
                let last = &rows[keep - 1];
                start += last.start + last.tiles.len();
            }
        }
        assert_eq!(grid, expected);
        assert_eq!(grid.widths.len(), ratios.len());
        assert!(plan_sections(&ratios, &[5, 3], 1080, 300, 4).is_err());
        assert!(plan_sections(&ratios, &[5001], 1080, 300, 4).is_err());
    }
    #[test]
    fn rows_preserve_order_and_never_overflow_even_at_tiny_viewports() {
        let ratios: Vec<_> = (0..177)
            .map(|i| [1.5, 0.67, 1.0, 1.78, 0.01, 100.0, f64::NAN][i % 7])
            .collect();
        for width in [1, 2, 4, 320, 412, 800, 1440, 3072] {
            for target in [96, 140, 420] {
                for gap in [0, 3, 9] {
                    let rows = plan_justified(&ratios, width, target, gap).unwrap();
                    let mut seen = 0;
                    for (index, row) in rows.iter().enumerate() {
                        assert_eq!(row.start, seen);
                        seen += row.tiles.len();
                        let span = row.tiles.iter().map(|t| t.width).sum::<u32>()
                            + gap * (row.tiles.len() as u32 - 1);
                        assert!(span <= width, "{span} > {width}");
                        if index + 1 < rows.len() {
                            assert_eq!(span, width);
                        }
                        assert!(
                            row.tiles
                                .iter()
                                .all(|t| t.width > 0 && t.height == row.tiles[0].height)
                        );
                    }
                    assert_eq!(seen, ratios.len());
                }
            }
        }
    }
    #[test]
    fn portraits_stay_narrower_and_sparse_last_rows_are_not_stretched() {
        let rows = plan_justified(&[1.5, 0.75], 1920, 180, 3).unwrap();
        assert_eq!(
            rows[0].tiles,
            vec![
                TileSize {
                    width: 270,
                    height: 180
                },
                TileSize {
                    width: 135,
                    height: 180
                }
            ]
        );
        assert_eq!(tile_ratio(f64::INFINITY), 1.0);
        assert_eq!(tile_ratio(-1.0), 1.0);
        assert!(plan_justified(&[], 0, 180, 3).is_err());
    }
}
