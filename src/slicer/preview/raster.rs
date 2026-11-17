//! A tiny z-buffered triangle rasterizer and an RGBA PNG writer, so previews need no GPU and no
//! image crates. Drawing happens at `ss`× the output size and is box-filtered down, which is
//! all the anti-aliasing a preview needs.

use std::io::Write;

pub type Rgb = [f32; 3];

/// A screen-space vertex: pixel position (before supersampling), depth (larger is farther) and
/// two values interpolated across the triangle for the shader.
#[derive(Debug, Clone, Copy)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub depth: f32,
    pub u: f32,
    pub v: f32,
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    ss: u32,
    depth: Vec<f32>,
    color: Vec<[f32; 4]>,
}

impl Frame {
    pub fn new(width: u32, height: u32, ss: u32) -> Self {
        let n = (width * ss * height * ss) as usize;
        Self {
            width,
            height,
            ss,
            depth: vec![f32::INFINITY; n],
            color: vec![[0.0; 4]; n],
        }
    }

    /// Fills a triangle, keeping the nearest surface per sample. `shade(u, v)` gives its colour.
    pub fn triangle(&mut self, t: [Vertex; 3], shade: &impl Fn(f32, f32) -> Rgb) {
        let s = self.ss as f32;
        let p: [[f32; 2]; 3] = t.map(|v| [v.x * s, v.y * s]);
        let area = edge(p[0], p[1], p[2]);
        if area.abs() < 1e-6 {
            return;
        }
        let sw = (self.width * self.ss) as i32;
        let sh = (self.height * self.ss) as i32;
        let min_x = p
            .iter()
            .map(|q| q[0])
            .fold(f32::INFINITY, f32::min)
            .floor()
            .max(0.0) as i32;
        let max_x = (p.iter().map(|q| q[0]).fold(f32::NEG_INFINITY, f32::max).ceil() as i32).min(sw - 1);
        let min_y = p
            .iter()
            .map(|q| q[1])
            .fold(f32::INFINITY, f32::min)
            .floor()
            .max(0.0) as i32;
        let max_y = (p.iter().map(|q| q[1]).fold(f32::NEG_INFINITY, f32::max).ceil() as i32).min(sh - 1);
        if min_x > max_x || min_y > max_y {
            return;
        }
        // Each edge function is linear in x along a row, so solve for the covered span instead
        // of testing the whole bounding box (thin diagonal beads have huge boxes).
        let edges = [(p[1], p[2]), (p[2], p[0]), (p[0], p[1])];
        for y in min_y..=max_y {
            let cy = y as f32 + 0.5;
            let (mut lo, mut hi) = (min_x as f32, max_x as f32 + 1.0);
            for (a, b) in edges {
                // edge(a, b, (x, cy)) / area = k·x + m
                let k = -(b[1] - a[1]) / area;
                let m = ((b[0] - a[0]) * (cy - a[1]) + (b[1] - a[1]) * a[0]) / area;
                if k.abs() < 1e-12 {
                    if m < 0.0 {
                        lo = hi;
                    }
                } else if k > 0.0 {
                    lo = lo.max(-m / k);
                } else {
                    hi = hi.min(-m / k);
                }
            }
            if lo >= hi {
                continue;
            }
            let x0 = ((lo - 0.5).floor() as i32).max(min_x);
            let x1 = ((hi - 0.5).ceil() as i32).min(max_x);
            for x in x0..=x1 {
                let c = [x as f32 + 0.5, cy];
                let w0 = edge(p[1], p[2], c) / area;
                let w1 = edge(p[2], p[0], c) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let depth = w0 * t[0].depth + w1 * t[1].depth + w2 * t[2].depth;
                let i = (y * sw + x) as usize;
                if depth >= self.depth[i] {
                    continue;
                }
                self.depth[i] = depth;
                let u = w0 * t[0].u + w1 * t[1].u + w2 * t[2].u;
                let v = w0 * t[0].v + w1 * t[1].v + w2 * t[2].v;
                let [r, g, b] = shade(u, v);
                self.color[i] = [r, g, b, 1.0];
            }
        }
    }

    pub fn quad(&mut self, q: [Vertex; 4], shade: &impl Fn(f32, f32) -> Rgb) {
        self.triangle([q[0], q[1], q[2]], shade);
        self.triangle([q[0], q[2], q[3]], shade);
    }

    /// Whether anything was drawn at all.
    pub fn is_empty(&self) -> bool {
        self.depth.iter().all(|d| d.is_infinite())
    }

    /// Box-filters the samples down to RGBA8 (straight alpha; empty samples are transparent).
    pub fn pixels(&self) -> Vec<u8> {
        let (w, h, ss) = (self.width as usize, self.height as usize, self.ss as usize);
        let sw = w * ss;
        let n = (ss * ss) as f32;
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f32; 4];
                for sy in 0..ss {
                    for sx in 0..ss {
                        let c = self.color[(y * ss + sy) * sw + x * ss + sx];
                        for k in 0..3 {
                            acc[k] += c[k] * c[3];
                        }
                        acc[3] += c[3];
                    }
                }
                let a = acc[3] / n;
                for k in 0..3 {
                    let v = if acc[3] > 0.0 { acc[k] / acc[3] } else { 0.0 };
                    out.push(to_u8(v));
                }
                out.push(to_u8(a));
            }
        }
        out
    }

    pub fn png(&self) -> Vec<u8> {
        encode_png(self.width, self.height, &self.pixels())
    }
}

fn edge(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Encodes 8-bit RGBA pixels as a PNG (every row uses the "Up" filter, which suits renders that
/// are mostly flat colour and transparency).
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), (width * height * 4) as usize);
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(2u8);
        let row = &rgba[y * stride..(y + 1) * stride];
        if y == 0 {
            raw.extend_from_slice(row);
        } else {
            let prev = &rgba[(y - 1) * stride..y * stride];
            raw.extend(row.iter().zip(prev).map(|(a, b)| a.wrapping_sub(*b)));
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&raw).expect("writing to a Vec");
    let idat = z.finish().expect("writing to a Vec");

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, adaptive filters, no interlace
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &idat);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Read;

    /// Decodes what [`encode_png`] writes (checks chunk CRCs), for tests.
    pub fn decode_png(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let (mut i, mut w, mut h, mut idat) = (8, 0, 0, Vec::new());
        while i < png.len() {
            let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
            let kind = &png[i + 4..i + 8];
            let data = &png[i + 8..i + 8 + len];
            let crc = u32::from_be_bytes(png[i + 8 + len..i + 12 + len].try_into().unwrap());
            let mut h2 = crc32fast::Hasher::new();
            h2.update(kind);
            h2.update(data);
            assert_eq!(crc, h2.finalize(), "crc of {kind:?}");
            match kind {
                b"IHDR" => {
                    w = u32::from_be_bytes(data[..4].try_into().unwrap());
                    h = u32::from_be_bytes(data[4..8].try_into().unwrap());
                    assert_eq!(&data[8..], &[8, 6, 0, 0, 0]);
                }
                b"IDAT" => idat.extend_from_slice(data),
                _ => {}
            }
            i += 12 + len;
        }
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(&idat[..])
            .read_to_end(&mut raw)
            .unwrap();
        let stride = w as usize * 4;
        let mut px = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            let row = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
            assert_eq!(row[0], 2);
            for x in 0..stride {
                let up = if y == 0 { 0 } else { px[(y - 1) * stride + x] };
                px[y * stride + x] = row[1 + x].wrapping_add(up);
            }
        }
        (w, h, px)
    }

    #[test]
    fn png_round_trip() {
        let px: Vec<u8> = (0..5 * 3 * 4).map(|i| (i * 37 % 256) as u8).collect();
        assert_eq!(decode_png(&encode_png(5, 3, &px)), (5, 3, px));
    }

    #[test]
    fn nearest_triangle_wins_and_edges_blend() {
        let v = |x, y, depth| Vertex {
            x,
            y,
            depth,
            u: 0.0,
            v: 0.0,
        };
        let mut f = Frame::new(4, 4, 2);
        assert!(f.is_empty());
        let far = [v(0.0, 0.0, 5.0), v(4.0, 0.0, 5.0), v(0.0, 4.0, 5.0)];
        let near = [v(0.0, 0.0, 1.0), v(4.0, 0.0, 1.0), v(0.0, 4.0, 1.0)];
        f.triangle(near, &|_, _| [0.0, 1.0, 0.0]);
        f.triangle(far, &|_, _| [1.0, 0.0, 0.0]);
        let px = f.pixels();
        // Top-left pixel: fully covered, green.
        assert_eq!(&px[..4], &[0, 255, 0, 255]);
        // Bottom-right pixel: outside the triangle, transparent.
        assert_eq!(px[px.len() - 1], 0);
        // A pixel on the diagonal is partly covered.
        // Pixel (x 2, y 1) of a 4-wide frame, alpha byte.
        let diag = (4 + 2) * 4 + 3;
        assert!(px[diag] > 0 && px[diag] < 255, "{}", px[diag]);
    }
}
