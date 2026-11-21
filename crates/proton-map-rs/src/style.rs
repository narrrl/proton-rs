//! Colours and line widths of the map.

/// A straight (not premultiplied) sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// An opaque colour from `0xRRGGBB`.
    pub const fn hex(rgb: u32) -> Self {
        Self {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
            a: 255,
        }
    }

    /// The same colour at `alpha` (0-255).
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    pub(crate) fn skia(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(self.r, self.g, self.b, self.a)
    }
}

/// How the map is drawn. Widths are in logical pixels and get multiplied by
/// the render scale.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    /// Tells styles apart in a [`crate::TileCache`]; give each custom style
    /// its own.
    pub id: u32,
    /// Sea colours from the coast outwards: shallow water, then deeper than
    /// 200, 1000, 2000, 3000, 4000, 5000 and 6000 m.
    pub water: [Rgba; 8],
    /// Lakes and reservoirs.
    pub lake: Rgba,
    pub river: Rgba,
    pub land: Rgba,
    /// Built-up areas, drawn over the land.
    pub urban: Rgba,
    /// The shoreline of the land.
    pub coast: Rgba,
    pub coast_width: f32,
    /// Borders between countries.
    pub border: Rgba,
    pub border_width: f32,
    /// Borders of states and provinces, drawn dashed.
    pub state_border: Rgba,
    pub state_border_width: f32,
    /// Dash and gap length of state borders.
    pub state_dash: [f32; 2],
    pub label: Rgba,
    pub label_halo: Rgba,
    /// Width of the halo stroke around each glyph.
    pub halo_width: f32,
    /// The dot marking a place.
    pub dot: Rgba,
    pub dot_ring: Rgba,
}

impl Style {
    /// Muted violet daylight map.
    pub fn light() -> Self {
        Self {
            id: 1,
            water: [
                Rgba::hex(0xDED6F1),
                Rgba::hex(0xDAD2EF),
                Rgba::hex(0xD7CEED),
                Rgba::hex(0xD4CAEB),
                Rgba::hex(0xD1C6E9),
                Rgba::hex(0xCEC2E7),
                Rgba::hex(0xCBBFE5),
                Rgba::hex(0xC8BCE3),
            ],
            lake: Rgba::hex(0xDED6F1),
            river: Rgba::hex(0xD3CAEE),
            land: Rgba::hex(0xF5F2FA),
            urban: Rgba::hex(0xEBE6F3),
            coast: Rgba::hex(0xC6BBE4),
            coast_width: 0.6,
            border: Rgba::hex(0x9A8EB2),
            border_width: 1.0,
            state_border: Rgba::hex(0xD3CCE3),
            state_border_width: 0.8,
            state_dash: [3.0, 2.5],
            label: Rgba::hex(0x3B3452),
            label_halo: Rgba::hex(0xF5F2FA).with_alpha(235),
            halo_width: 2.6,
            dot: Rgba::hex(0x6C5F92),
            dot_ring: Rgba::hex(0xF5F2FA),
        }
    }

    /// Muted violet night map.
    pub fn dark() -> Self {
        Self {
            id: 2,
            water: [
                Rgba::hex(0x1F1935),
                Rgba::hex(0x1C1731),
                Rgba::hex(0x1A152E),
                Rgba::hex(0x18132B),
                Rgba::hex(0x161228),
                Rgba::hex(0x141025),
                Rgba::hex(0x120F22),
                Rgba::hex(0x110D20),
            ],
            lake: Rgba::hex(0x1F1935),
            river: Rgba::hex(0x211B3A),
            land: Rgba::hex(0x2E2942),
            urban: Rgba::hex(0x363049),
            coast: Rgba::hex(0x403859),
            coast_width: 0.6,
            border: Rgba::hex(0x6A5F82),
            border_width: 1.0,
            state_border: Rgba::hex(0x4D4564),
            state_border_width: 0.8,
            state_dash: [3.0, 2.5],
            label: Rgba::hex(0xDCD6EA),
            label_halo: Rgba::hex(0x1B1630).with_alpha(220),
            halo_width: 2.6,
            dot: Rgba::hex(0xB7ACD2),
            dot_ring: Rgba::hex(0x1B1630),
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self::light()
    }
}
