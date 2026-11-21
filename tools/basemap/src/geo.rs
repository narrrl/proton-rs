//! Planar geometry in Web Mercator world units (0..1 across the map):
//! simplification and clipping to tile boxes.

/// A polygon: outer rings and holes alike, filled even-odd. Rings don't
/// repeat their first point at the end.
pub type Rings = Vec<Vec<[f64; 2]>>;

/// Lines whose points carry their distance along the line as a third value.
pub type Lines = Vec<Vec<[f64; 3]>>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl BBox {
    pub fn expand(self, by: f64) -> Self {
        Self {
            x0: self.x0 - by,
            y0: self.y0 - by,
            x1: self.x1 + by,
            y1: self.y1 + by,
        }
    }

    pub fn of_points<'a>(points: impl IntoIterator<Item = &'a [f64; 2]>) -> Option<Self> {
        let mut iter = points.into_iter();
        let first = iter.next()?;
        let mut b = Self {
            x0: first[0],
            y0: first[1],
            x1: first[0],
            y1: first[1],
        };
        for p in iter {
            b.x0 = b.x0.min(p[0]);
            b.y0 = b.y0.min(p[1]);
            b.x1 = b.x1.max(p[0]);
            b.y1 = b.y1.max(p[1]);
        }
        Some(b)
    }

    pub fn inside(&self, other: &BBox) -> bool {
        self.x0 >= other.x0 && self.y0 >= other.y0 && self.x1 <= other.x1 && self.y1 <= other.y1
    }

    pub fn disjoint(&self, other: &BBox) -> bool {
        self.x1 < other.x0 || other.x1 < self.x0 || self.y1 < other.y0 || other.y1 < self.y0
    }
}

/// Twice the signed area of a ring.
pub fn area2(ring: &[[f64; 2]]) -> f64 {
    let mut sum = 0.0;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    sum
}

fn segment_distance2(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (ex, ey) = (a[0] + t * dx - p[0], a[1] + t * dy - p[1]);
    ex * ex + ey * ey
}

/// Douglas-Peucker: the points of `line` that keep it within `tolerance`.
pub fn simplify(line: &[[f64; 2]], tolerance: f64) -> Vec<[f64; 2]> {
    if line.len() < 3 {
        return line.to_vec();
    }
    let mut keep = vec![false; line.len()];
    keep[0] = true;
    keep[line.len() - 1] = true;
    let tol2 = tolerance * tolerance;
    let mut stack = vec![(0, line.len() - 1)];
    while let Some((first, last)) = stack.pop() {
        let mut worst = 0.0;
        let mut at = 0;
        for i in first + 1..last {
            let d = segment_distance2(line[i], line[first], line[last]);
            if d > worst {
                worst = d;
                at = i;
            }
        }
        if worst > tol2 {
            keep[at] = true;
            stack.push((first, at));
            stack.push((at, last));
        }
    }
    line.iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(*p))
        .collect()
}

/// Simplify a closed ring: split at its first point and the point farthest
/// from it so both halves have distinct ends.
pub fn simplify_ring(ring: &[[f64; 2]], tolerance: f64) -> Vec<[f64; 2]> {
    if ring.len() < 4 {
        return ring.to_vec();
    }
    let start = ring[0];
    let far = (1..ring.len())
        .max_by(|&a, &b| {
            let da = (ring[a][0] - start[0]).powi(2) + (ring[a][1] - start[1]).powi(2);
            let db = (ring[b][0] - start[0]).powi(2) + (ring[b][1] - start[1]).powi(2);
            da.total_cmp(&db)
        })
        .unwrap_or(1);
    let mut first = simplify(&ring[..=far], tolerance);
    let mut second: Vec<[f64; 2]> = ring[far..].to_vec();
    second.push(start);
    let second = simplify(&second, tolerance);
    first.pop();
    first.extend_from_slice(&second[..second.len() - 1]);
    first
}

/// Clip a ring to `b` (Sutherland-Hodgman). Where the ring leaves the box
/// the result runs along the box's edge, which lies in the tile's buffer and
/// is never drawn as a line.
pub fn clip_ring(ring: &[[f64; 2]], b: &BBox) -> Vec<[f64; 2]> {
    let mut points = ring.to_vec();
    // Edges: x >= x0, x <= x1, y >= y0, y <= y1.
    for edge in 0..4 {
        if points.is_empty() {
            break;
        }
        let inside = |p: &[f64; 2]| match edge {
            0 => p[0] >= b.x0,
            1 => p[0] <= b.x1,
            2 => p[1] >= b.y0,
            _ => p[1] <= b.y1,
        };
        let cross = |p: [f64; 2], q: [f64; 2]| -> [f64; 2] {
            match edge {
                0 | 1 => {
                    let x = if edge == 0 { b.x0 } else { b.x1 };
                    let t = (x - p[0]) / (q[0] - p[0]);
                    [x, p[1] + t * (q[1] - p[1])]
                }
                _ => {
                    let y = if edge == 2 { b.y0 } else { b.y1 };
                    let t = (y - p[1]) / (q[1] - p[1]);
                    [p[0] + t * (q[0] - p[0]), y]
                }
            }
        };
        let mut out = Vec::with_capacity(points.len() + 4);
        let mut previous = *points.last().expect("non-empty");
        let mut previous_in = inside(&previous);
        for &p in &points {
            let p_in = inside(&p);
            if p_in {
                if !previous_in {
                    out.push(cross(previous, p));
                }
                out.push(p);
            } else if previous_in {
                out.push(cross(previous, p));
            }
            previous = p;
            previous_in = p_in;
        }
        points = out;
    }
    points
}

/// Clip lines to `b`, splitting them where they leave it.
pub fn clip_lines(lines: &[Vec<[f64; 3]>], b: &BBox) -> Lines {
    let mut out: Lines = Vec::new();
    for line in lines {
        let mut current: Vec<[f64; 3]> = Vec::new();
        for pair in line.windows(2) {
            let (p, q) = (pair[0], pair[1]);
            // Liang-Barsky.
            let (dx, dy) = (q[0] - p[0], q[1] - p[1]);
            let mut t0: f64 = 0.0;
            let mut t1: f64 = 1.0;
            let mut visible = true;
            for (pk, qk) in [
                (-dx, p[0] - b.x0),
                (dx, b.x1 - p[0]),
                (-dy, p[1] - b.y0),
                (dy, b.y1 - p[1]),
            ] {
                if pk == 0.0 {
                    if qk < 0.0 {
                        visible = false;
                        break;
                    }
                } else {
                    let r = qk / pk;
                    if pk < 0.0 {
                        t0 = t0.max(r);
                    } else {
                        t1 = t1.min(r);
                    }
                }
            }
            if !visible || t0 > t1 {
                if current.len() >= 2 {
                    out.push(std::mem::take(&mut current));
                }
                current.clear();
                continue;
            }
            let at = |t: f64| [p[0] + t * dx, p[1] + t * dy, p[2] + t * (q[2] - p[2])];
            let start = at(t0);
            if current.is_empty() {
                current.push(start);
            }
            current.push(at(t1));
            if t1 < 1.0 {
                out.push(std::mem::take(&mut current));
            }
        }
        if current.len() >= 2 {
            out.push(current);
        }
    }
    out
}

/// Number the points of a line with their distance along it.
pub fn with_distance(line: &[[f64; 2]]) -> Vec<[f64; 3]> {
    let mut d = 0.0;
    let mut out = Vec::with_capacity(line.len());
    for (i, p) in line.iter().enumerate() {
        if i > 0 {
            let q = line[i - 1];
            d += ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2)).sqrt();
        }
        out.push([p[0], p[1], d]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplify_keeps_corners_and_drops_noise() {
        let line = [[0.0, 0.0], [1.0, 0.01], [2.0, 0.0], [2.0, 2.0]];
        assert_eq!(
            simplify(&line, 0.1),
            vec![[0.0, 0.0], [2.0, 0.0], [2.0, 2.0]]
        );
        let ring = [[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        assert_eq!(simplify_ring(&ring, 0.1).len(), 4);
    }

    #[test]
    fn a_ring_around_the_box_clips_to_the_box() {
        let ring = [[-1.0, -1.0], [3.0, -1.0], [3.0, 3.0], [-1.0, 3.0]];
        let b = BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
        };
        let clipped = clip_ring(&ring, &b);
        assert!((area2(&clipped).abs() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn lines_split_where_they_leave_the_box() {
        let line = with_distance(&[[-1.0, 0.5], [0.5, 0.5], [0.5, 2.0], [0.7, 0.5]]);
        let b = BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
        };
        let parts = clip_lines(&[line], &b);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0][0], [0.0, 0.5, 1.0]);
        assert_eq!(parts[0].last().unwrap()[..2], [0.5, 1.0]);
        assert_eq!(parts[1].last().unwrap()[..2], [0.7, 0.5]);
    }
}
