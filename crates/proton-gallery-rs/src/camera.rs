//! The Places map camera, shared by every frontend so panning, zooming,
//! fitting and the choice of basemap tiles behave the same everywhere.
//!
//! World coordinates are the Web Mercator square from
//! [`places::project`](crate::places::project): `(0, 0)` top-left, `(1, 1)`
//! bottom-right. At zoom 1 the whole world fits the shorter side of the view.

use crate::places::World;

pub const MIN_ZOOM: f64 = 1.0;
pub const MAX_ZOOM: f64 = 8192.0;

/// Size of the map surface in px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: f64,
    pub height: f64,
}

impl Viewport {
    pub fn new(width: f64, height: f64) -> Self {
        Self {
            width: width.max(1.0),
            height: height.max(1.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub center: World,
    pub zoom: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            center: World { x: 0.5, y: 0.5 },
            zoom: MIN_ZOOM,
        }
    }
}

/// One basemap tile to draw: tile `(level, x, y)` with its top-left corner
/// at `(left, top)` px in the view, `size` px wide and high.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileView {
    pub level: u8,
    pub x: u32,
    pub y: u32,
    pub left: f64,
    pub top: f64,
    pub size: f64,
}

impl Camera {
    /// A camera at `center`, with the centre kept inside the world and the
    /// zoom inside [`MIN_ZOOM`]..=[`MAX_ZOOM`].
    pub fn new(center: World, zoom: f64) -> Self {
        Self {
            center: World {
                x: center.x.clamp(0.0, 1.0),
                y: center.y.clamp(0.0, 1.0),
            },
            zoom: zoom.clamp(MIN_ZOOM, MAX_ZOOM),
        }
    }

    /// Px per world unit.
    pub fn scale(self, view: Viewport) -> f64 {
        view.width.min(view.height) * self.zoom
    }

    pub fn to_screen(self, point: World, view: Viewport) -> (f64, f64) {
        let scale = self.scale(view);
        (
            view.width * 0.5 + (point.x - self.center.x) * scale,
            view.height * 0.5 + (point.y - self.center.y) * scale,
        )
    }

    pub fn to_world(self, x: f64, y: f64, view: Viewport) -> World {
        let scale = self.scale(view);
        World {
            x: self.center.x + (x - view.width * 0.5) / scale,
            y: self.center.y + (y - view.height * 0.5) / scale,
        }
    }

    /// Zoom by `factor` around the view centre.
    pub fn zoomed(self, factor: f64) -> Self {
        Self::new(self.center, self.zoom * factor)
    }

    /// Zoom by `factor` keeping the world point under `(x, y)` px in place,
    /// for wheel and pinch zoom.
    pub fn zoom_at(self, factor: f64, x: f64, y: f64, view: Viewport) -> Self {
        let anchor = self.to_world(x, y, view);
        let zoomed = self.zoomed(factor);
        let scale = zoomed.scale(view);
        Self::new(
            World {
                x: anchor.x - (x - view.width * 0.5) / scale,
                y: anchor.y - (y - view.height * 0.5) / scale,
            },
            zoomed.zoom,
        )
    }

    /// Move the map with the pointer: `(dx, dy)` px of drag.
    pub fn panned(self, dx: f64, dy: f64, view: Viewport) -> Self {
        let scale = self.scale(view);
        Self::new(
            World {
                x: self.center.x - dx / scale,
                y: self.center.y - dy / scale,
            },
            self.zoom,
        )
    }

    /// The camera that shows all `points` with a margin, zoomed in no
    /// further than `max_zoom`. No points gives the whole world.
    pub fn fit(points: impl IntoIterator<Item = World>, view: Viewport, max_zoom: f64) -> Self {
        let mut x = (f64::INFINITY, f64::NEG_INFINITY);
        let mut y = x;
        for point in points {
            x = (x.0.min(point.x), x.1.max(point.x));
            y = (y.0.min(point.y), y.1.max(point.y));
        }
        if !x.0.is_finite() {
            return Self::default();
        }
        let short = view.width.min(view.height);
        let span = ((x.1 - x.0) * short / view.width).max((y.1 - y.0) * short / view.height);
        Self::new(
            World {
                x: (x.0 + x.1) * 0.5,
                y: (y.0 + y.1) * 0.5,
            },
            (0.72 / span).min(max_zoom),
        )
    }

    /// The basemap level whose tiles of `tile_px` px come closest to their
    /// own size at this zoom, so a tile is drawn between about 0.71 and
    /// 1.41 times its size. Past `max_level` tiles are stretched instead.
    pub fn tile_level(self, view: Viewport, tile_px: f64, max_level: u8) -> u8 {
        let level = (self.scale(view) / tile_px).log2().round();
        level.clamp(0.0, f64::from(max_level)) as u8
    }

    /// The tiles covering the view, in row order.
    pub fn tiles(self, view: Viewport, tile_px: f64, max_level: u8) -> Vec<TileView> {
        let level = self.tile_level(view, tile_px, max_level);
        let count = 1u32 << level;
        let size = self.scale(view) / f64::from(count);
        let (left, top) = self.to_screen(World { x: 0.0, y: 0.0 }, view);
        let range = |origin: f64, extent: f64| {
            let first = ((-origin) / size).floor().max(0.0) as u32;
            let last = ((extent - origin) / size).ceil().min(f64::from(count)) as u32;
            first..last.max(first)
        };
        let columns = range(left, view.width);
        range(top, view.height)
            .flat_map(|y| {
                columns.clone().map(move |x| TileView {
                    level,
                    x,
                    y,
                    left: left + f64::from(x) * size,
                    top: top + f64::from(y) * size,
                    size,
                })
            })
            .collect()
    }

    /// Length in metres of `px` px at the view centre, for a scale bar.
    pub fn metres(self, px: f64, view: Viewport) -> f64 {
        const EQUATOR_M: f64 = 40_075_016.686;
        let latitude = (std::f64::consts::PI * (1.0 - 2.0 * self.center.y))
            .sinh()
            .atan();
        px / self.scale(view) * EQUATOR_M * latitude.cos()
    }
}

/// A round scale bar length no longer than `max_px`: `(metres, px)`.
pub fn scale_bar(camera: Camera, view: Viewport, max_px: f64) -> (f64, f64) {
    let most = camera.metres(max_px, view);
    let power = 10f64.powf(most.log10().floor());
    let metres = [5.0, 2.0, 1.0]
        .into_iter()
        .map(|step| step * power)
        .find(|&m| m <= most)
        .unwrap_or(power);
    (metres, max_px * metres / most)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::places::project;

    const VIEW: Viewport = Viewport {
        width: 1200.0,
        height: 800.0,
    };

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn screen_and_world_round_trip() {
        let camera = Camera::new(World { x: 0.3, y: 0.6 }, 37.0);
        let point = World { x: 0.31, y: 0.59 };
        let (x, y) = camera.to_screen(point, VIEW);
        let back = camera.to_world(x, y, VIEW);
        assert!(close(back.x, point.x) && close(back.y, point.y));
    }

    #[test]
    fn zooming_at_a_point_keeps_it_under_the_pointer() {
        let camera = Camera::new(World { x: 0.5, y: 0.4 }, 3.0);
        let before = camera.to_world(900.0, 100.0, VIEW);
        let zoomed = camera.zoom_at(2.5, 900.0, 100.0, VIEW);
        let after = zoomed.to_world(900.0, 100.0, VIEW);
        assert!(close(zoomed.zoom, 7.5));
        assert!(close(before.x, after.x) && close(before.y, after.y));
    }

    #[test]
    fn zoom_and_centre_stay_in_range() {
        let camera = Camera::default().zoomed(0.1);
        assert_eq!(camera.zoom, MIN_ZOOM);
        let camera = Camera::default().zoomed(1e9).panned(1e12, -1e12, VIEW);
        assert_eq!(camera.zoom, MAX_ZOOM);
        assert_eq!(camera.center, World { x: 0.0, y: 1.0 });
    }

    #[test]
    fn fitting_shows_every_point() {
        let points = [project(51.2, 4.4), project(48.8, 2.3), project(52.5, 13.4)];
        let camera = Camera::fit(points, VIEW, MAX_ZOOM);
        for point in points {
            let (x, y) = camera.to_screen(point, VIEW);
            assert!((0.0..=VIEW.width).contains(&x) && (0.0..=VIEW.height).contains(&y));
        }
        assert!(camera.zoom > 4.0);
        assert_eq!(Camera::fit([], VIEW, MAX_ZOOM), Camera::default());
        assert_eq!(Camera::fit([points[0]], VIEW, 64.0).zoom, 64.0);
    }

    #[test]
    fn tiles_cover_the_view_without_gaps() {
        for zoom in [1.0, 1.7, 5.0, 300.0, 8192.0] {
            let camera = Camera::new(World { x: 0.52, y: 0.33 }, zoom);
            let tiles = camera.tiles(VIEW, 512.0, 14);
            assert!(!tiles.is_empty());
            let level = tiles[0].level;
            assert!(tiles.iter().all(|t| t.level == level));
            // Every sampled pixel inside the world lies in exactly one tile.
            for sy in (0..800).step_by(37) {
                for sx in (0..1200).step_by(37) {
                    let (x, y) = (f64::from(sx), f64::from(sy));
                    let world = camera.to_world(x, y, VIEW);
                    if !(0.0..1.0).contains(&world.x) || !(0.0..1.0).contains(&world.y) {
                        continue;
                    }
                    let hits = tiles
                        .iter()
                        .filter(|t| {
                            (t.left..t.left + t.size).contains(&x)
                                && (t.top..t.top + t.size).contains(&y)
                        })
                        .count();
                    assert_eq!(hits, 1, "zoom {zoom} at {x},{y}");
                }
            }
            // No tile lies fully outside the view.
            assert!(tiles.iter().all(|t| t.left < VIEW.width
                && t.top < VIEW.height
                && t.left + t.size > 0.0
                && t.top + t.size > 0.0));
            if level < 14 {
                let ratio = tiles[0].size / 512.0;
                assert!((0.70..=1.42).contains(&ratio), "zoom {zoom}: {ratio}");
            }
        }
    }

    #[test]
    fn whole_world_at_the_start() {
        // 800 px of world is closer to two 512 px tiles than to one.
        let tiles = Camera::default().tiles(VIEW, 512.0, 14);
        assert_eq!(tiles.len(), 4);
        assert_eq!(
            (tiles[0].left, tiles[0].top, tiles[0].size),
            (200.0, 0.0, 400.0)
        );
        let small = Viewport::new(600.0, 500.0);
        assert_eq!(Camera::default().tiles(small, 512.0, 14).len(), 1);
    }

    #[test]
    fn scale_bar_is_round_and_fits() {
        let camera = Camera::new(project(51.2, 4.4), 2000.0);
        let (metres, px) = scale_bar(camera, VIEW, 120.0);
        assert!(px <= 120.0 && px > 40.0);
        let digits = metres / 10f64.powf(metres.log10().floor());
        assert!([1.0, 2.0, 5.0].iter().any(|d| close(*d, digits)));
        // At the equator, zoom 1 on an 800 px view: 1 px is about 50 km.
        let metres = Camera::default().metres(1.0, VIEW);
        assert!((metres - 50_094.0).abs() < 1.0);
    }
}
