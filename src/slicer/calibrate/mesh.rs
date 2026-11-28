//! Calibration models as triangle meshes, written as binary STL.
//!
//! Every part is a convex solid (boxes, wedges, the bars of seven-segment digits). Parts may
//! overlap: Orca slices a mesh's closed shells and unions them per layer (checked with Orca 2.5.1),
//! so a label can sink a little into the face it stands on instead of having to meet it exactly.

/// A point in millimetres.
pub type P = [f64; 3];

#[derive(Debug, Default, Clone)]
pub struct Mesh {
    tris: Vec<[P; 3]>,
}

impl Mesh {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn triangles(&self) -> usize {
        self.tris.len()
    }

    /// The axis-aligned bounds, `(min, max)`.
    pub fn bounds(&self) -> (P, P) {
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for p in self.tris.iter().flatten() {
            for i in 0..3 {
                lo[i] = lo[i].min(p[i]);
                hi[i] = hi[i].max(p[i]);
            }
        }
        (lo, hi)
    }

    /// A convex solid from its corners and faces (each face a list of corner indexes, in order
    /// around the face, either way round). Faces are fanned into triangles and turned to face
    /// outwards, which is well defined because the solid is convex.
    pub fn convex(&mut self, pts: &[P], faces: &[&[usize]]) {
        let n = pts.len() as f64;
        let c = pts.iter().fold([0.0; 3], |a, p| add(a, scale(*p, 1.0 / n)));
        for f in faces {
            for i in 1..f.len().saturating_sub(1) {
                let (a, b, d) = (pts[f[0]], pts[f[i]], pts[f[i + 1]]);
                let normal = cross(sub(b, a), sub(d, a));
                let mid = scale(add(add(a, b), d), 1.0 / 3.0);
                if dot(normal, sub(mid, c)) >= 0.0 {
                    self.tris.push([a, b, d]);
                } else {
                    self.tris.push([a, d, b]);
                }
            }
        }
    }

    /// A box between two corners.
    pub fn cuboid(&mut self, lo: P, hi: P) {
        let v = |x: usize, y: usize, z: usize| {
            [
                if x == 0 { lo[0] } else { hi[0] },
                if y == 0 { lo[1] } else { hi[1] },
                if z == 0 { lo[2] } else { hi[2] },
            ]
        };
        let pts: Vec<P> = (0..8).map(|i| v(i >> 2 & 1, i >> 1 & 1, i & 1)).collect();
        self.convex(
            &pts,
            &[
                &[0, 2, 6, 4],
                &[1, 5, 7, 3],
                &[0, 4, 5, 1],
                &[2, 3, 7, 6],
                &[0, 1, 3, 2],
                &[4, 6, 7, 5],
            ],
        );
    }

    /// A convex polygon in the XZ plane (`(x, z)` corners, in order), extruded from `y0` to `y1`.
    pub fn prism_y(&mut self, xz: &[(f64, f64)], y0: f64, y1: f64) {
        let n = xz.len();
        let mut pts: Vec<P> = xz.iter().map(|&(x, z)| [x, y0, z]).collect();
        pts.extend(xz.iter().map(|&(x, z)| [x, y1, z]));
        let front: Vec<usize> = (0..n).collect();
        let back: Vec<usize> = (n..2 * n).collect();
        let sides: Vec<[usize; 4]> = (0..n).map(|i| [i, (i + 1) % n, (i + 1) % n + n, i + n]).collect();
        let mut faces: Vec<&[usize]> = vec![&front, &back];
        faces.extend(sides.iter().map(|s| &s[..]));
        self.convex(&pts, &faces);
    }

    /// A convex polygon in the XY plane (`(x, y)` corners, in order), extruded from `z0` to `z1`.
    pub fn prism_z(&mut self, xy: &[(f64, f64)], z0: f64, z1: f64) {
        let n = xy.len();
        let mut pts: Vec<P> = xy.iter().map(|&(x, y)| [x, y, z0]).collect();
        pts.extend(xy.iter().map(|&(x, y)| [x, y, z1]));
        let bottom: Vec<usize> = (0..n).collect();
        let top: Vec<usize> = (n..2 * n).collect();
        let sides: Vec<[usize; 4]> = (0..n).map(|i| [i, (i + 1) % n, (i + 1) % n + n, i + n]).collect();
        let mut faces: Vec<&[usize]> = vec![&bottom, &top];
        faces.extend(sides.iter().map(|s| &s[..]));
        self.convex(&pts, &faces);
    }

    /// Seven-segment text standing out of a vertical face at `y = face` (towards −y, `depth`
    /// deep, sunk `sink` into the face). `x`/`z` is the bottom-left of the first character.
    pub fn text_front(&mut self, s: &str, x: f64, z: f64, face: f64, glyph: Glyph, depth: f64) {
        let sink = glyph.sink;
        for bar in glyph.bars(s) {
            self.cuboid(
                [x + bar.0, face - depth, z + bar.1],
                [x + bar.2, face + sink, z + bar.3],
            );
        }
    }

    /// Seven-segment text standing `height` above a top face at `z = top`, reading along +x with
    /// its bottom towards −y. `x`/`y` is the bottom-left of the first character.
    pub fn text_top(&mut self, s: &str, x: f64, y: f64, top: f64, glyph: Glyph, height: f64) {
        let sink = glyph.sink;
        for bar in glyph.bars(s) {
            self.cuboid(
                [x + bar.0, y + bar.1, top - sink],
                [x + bar.2, y + bar.3, top + height],
            );
        }
    }

    /// Binary STL.
    pub fn to_stl(&self, title: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(84 + self.tris.len() * 50);
        let mut head = [b' '; 80];
        let t = title.as_bytes();
        head[..t.len().min(80)].copy_from_slice(&t[..t.len().min(80)]);
        out.extend_from_slice(&head);
        out.extend_from_slice(&(self.tris.len() as u32).to_le_bytes());
        for t in &self.tris {
            let n = normalize(cross(sub(t[1], t[0]), sub(t[2], t[0])));
            for v in std::iter::once(n).chain(t.iter().copied()) {
                for c in v {
                    out.extend_from_slice(&(c as f32).to_le_bytes());
                }
            }
            out.extend_from_slice(&[0, 0]);
        }
        out
    }
}

/// The size of seven-segment characters.
#[derive(Debug, Clone, Copy)]
pub struct Glyph {
    pub width: f64,
    pub height: f64,
    pub stroke: f64,
    /// Space between characters.
    pub gap: f64,
    /// How far each bar sinks into the surface it stands on.
    pub sink: f64,
}

impl Glyph {
    /// Width of `s` set in this glyph.
    pub fn measure(&self, s: &str) -> f64 {
        let mut w = 0.0;
        for (i, c) in s.chars().enumerate() {
            if i > 0 {
                w += self.gap;
            }
            w += self.advance(c);
        }
        w
    }

    fn advance(&self, c: char) -> f64 {
        match c {
            '.' => self.stroke,
            '-' | '+' => self.width * 0.8,
            _ => self.width,
        }
    }

    /// The bars of `s`, as `(u0, v0, u1, v1)` from the text's bottom-left corner.
    fn bars(&self, s: &str) -> Vec<(f64, f64, f64, f64)> {
        let (w, h, t) = (self.width, self.height, self.stroke);
        let mid = h / 2.0;
        let mut out = Vec::new();
        let mut u = 0.0;
        for (i, c) in s.chars().enumerate() {
            if i > 0 {
                u += self.gap;
            }
            let adv = self.advance(c);
            let seg = |s: char| -> (f64, f64, f64, f64) {
                match s {
                    'a' => (0.0, h - t, w, h),
                    'b' => (w - t, mid, w, h),
                    'c' => (w - t, 0.0, w, mid),
                    'd' => (0.0, 0.0, w, t),
                    'e' => (0.0, 0.0, t, mid),
                    'f' => (0.0, mid, t, h),
                    _ => (0.0, mid - t / 2.0, w, mid + t / 2.0),
                }
            };
            let segs: &[char] = match c {
                '0' => &['a', 'b', 'c', 'd', 'e', 'f'],
                '1' => &['b', 'c'],
                '2' => &['a', 'b', 'g', 'e', 'd'],
                '3' => &['a', 'b', 'g', 'c', 'd'],
                '4' => &['f', 'g', 'b', 'c'],
                '5' => &['a', 'f', 'g', 'c', 'd'],
                '6' => &['a', 'f', 'g', 'e', 'c', 'd'],
                '7' => &['a', 'b', 'c'],
                '8' => &['a', 'b', 'c', 'd', 'e', 'f', 'g'],
                '9' => &['a', 'b', 'c', 'd', 'f', 'g'],
                _ => &[],
            };
            for s in segs {
                let (u0, v0, u1, v1) = seg(*s);
                out.push((u + u0, v0, u + u1, v1));
            }
            match c {
                '-' => out.push((u, mid - t / 2.0, u + adv, mid + t / 2.0)),
                '+' => {
                    out.push((u, mid - t / 2.0, u + adv, mid + t / 2.0));
                    let cx = u + adv / 2.0;
                    out.push((cx - t / 2.0, mid - adv / 2.0, cx + t / 2.0, mid + adv / 2.0));
                }
                '.' => out.push((u, 0.0, u + t, t)),
                _ => {}
            }
            u += adv;
        }
        out
    }
}

fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: P, s: f64) -> P {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dot(a: P, b: P) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: P, b: P) -> P {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(a: P) -> P {
    let l = dot(a, a).sqrt();
    if l > 0.0 { scale(a, 1.0 / l) } else { a }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signed volume (divergence theorem): positive only if every triangle faces outwards.
    fn volume(m: &Mesh) -> f64 {
        m.tris.iter().map(|t| dot(t[0], cross(t[1], t[2])) / 6.0).sum()
    }

    #[test]
    fn a_cuboid_is_closed_and_faces_out() {
        let mut m = Mesh::new();
        m.cuboid([1.0, 2.0, 3.0], [3.0, 5.0, 7.0]);
        assert_eq!(m.triangles(), 12);
        assert!((volume(&m) - 2.0 * 3.0 * 4.0).abs() < 1e-9);
        assert_eq!(m.bounds(), ([1.0, 2.0, 3.0], [3.0, 5.0, 7.0]));
    }

    #[test]
    fn a_wedge_faces_out() {
        let mut m = Mesh::new();
        // A right triangle with legs 2 (x) and 3 (z), 4 deep.
        m.prism_y(&[(0.0, 0.0), (0.0, 3.0), (-2.0, 3.0)], 0.0, 4.0);
        assert_eq!(m.triangles(), 8);
        assert!((volume(&m) - 0.5 * 2.0 * 3.0 * 4.0).abs() < 1e-9);
    }

    #[test]
    fn an_octagon_faces_out() {
        let mut m = Mesh::new();
        let oct: Vec<(f64, f64)> = (0..8)
            .map(|i| {
                let a = std::f64::consts::PI / 4.0 * i as f64;
                (a.cos(), a.sin())
            })
            .collect();
        m.prism_z(&oct, 0.0, 2.0);
        // A regular octagon of circumradius 1 has area 2√2.
        assert!((volume(&m) - 2.0 * 2.0f64.sqrt() * 2.0).abs() < 1e-9);
    }

    #[test]
    fn digits_are_bars() {
        let g = Glyph {
            width: 3.0,
            height: 5.0,
            stroke: 1.0,
            gap: 0.5,
            sink: 0.2,
        };
        assert_eq!(g.bars("8").len(), 7);
        assert_eq!(g.bars("1").len(), 2);
        assert_eq!(g.bars("+5").len(), 2 + 5);
        assert_eq!(g.bars("-").len(), 1);
        assert!((g.measure("215") - (3.0 * 3.0 + 2.0 * 0.5)).abs() < 1e-9);
        let mut m = Mesh::new();
        m.text_front("10", 0.0, 0.0, 0.0, g, 0.8);
        let (lo, hi) = m.bounds();
        assert!((lo[1] + 0.8).abs() < 1e-9 && (hi[1] - 0.2).abs() < 1e-9);
        assert!((hi[0] - 6.5).abs() < 1e-9 && (hi[2] - 5.0).abs() < 1e-9);
    }

    #[test]
    fn stl_layout() {
        let mut m = Mesh::new();
        m.cuboid([0.0; 3], [1.0; 3]);
        let stl = m.to_stl("printpit test");
        assert_eq!(stl.len(), 84 + 12 * 50);
        assert!(stl.starts_with(b"printpit test "));
        assert_eq!(u32::from_le_bytes(stl[80..84].try_into().unwrap()), 12);
    }
}
