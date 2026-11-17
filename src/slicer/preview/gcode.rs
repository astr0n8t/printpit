//! Reads the extruded toolpath out of OrcaSlicer's G-code, one line at a time.
//!
//! What it understands: `G0`/`G1` moves, `G2`/`G3` arcs (`I`/`J` centre form, cut into short
//! chords), `G90`/`G91`, `M82`/`M83`, `G92`, tool changes `T<n>`, and the comments Orca writes:
//! `;TYPE:<feature>`, `;WIDTH:`, `;HEIGHT:` and the `; key = value` config block at the end.
//! A move is an extrusion when E grows and the nozzle moves sideways; wipes (negative E),
//! retracts and travel are skipped. Everything else (macros, `M`-codes) is ignored.

use std::io::BufRead;

/// What a stretch of extrusion is, from `;TYPE:`. Start/end G-code (`Custom`, and anything before
/// the first `;TYPE:`) is left out of every picture: purge lines would only spoil the framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Model,
    /// Sparse infill, internal solid infill and internal bridges: always covered by walls and top
    /// surfaces (Orca names exposed infill `Top surface`, `Bridge`, …), so pictures skip it.
    Interior,
    Support,
    /// Skirt and brim.
    Adhesion,
    /// Prime/wipe tower.
    Tower,
    Custom,
}

impl Feature {
    fn from_type(t: &str) -> Self {
        let t = t.trim().to_ascii_lowercase();
        if t.contains("support") {
            Feature::Support
        } else if t.contains("skirt") || t.contains("brim") {
            Feature::Adhesion
        } else if t.contains("tower") {
            Feature::Tower
        } else if t == "sparse infill" || t.starts_with("internal ") {
            Feature::Interior
        } else if t == "custom" || t.is_empty() {
            Feature::Custom
        } else {
            Feature::Model
        }
    }
}

/// One straight piece of extrusion: a bead from `a` to `b` whose top is at `z`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub z: f32,
    pub width: f32,
    pub height: f32,
    pub feature: Feature,
    pub tool: u8,
}

pub enum Event<'a> {
    Segment(Segment),
    /// A `; key = value` line from the config block.
    Config(&'a str, &'a str),
}

const DEFAULT_WIDTH: f32 = 0.45;
const DEFAULT_HEIGHT: f32 = 0.2;
/// Longest chord when an arc is cut into straight pieces, in mm.
const ARC_CHORD: f32 = 0.5;

struct State {
    pos: [f32; 4],
    abs_xyz: bool,
    abs_e: bool,
    feature: Feature,
    width: f32,
    height: f32,
    tool: u8,
    in_config: bool,
}

/// Calls `f` for every extruded segment and config entry in the file.
pub fn walk(reader: impl BufRead, mut f: impl FnMut(Event)) -> std::io::Result<()> {
    let mut st = State {
        pos: [0.0; 4],
        abs_xyz: true,
        abs_e: true,
        feature: Feature::Custom,
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        tool: 0,
        in_config: false,
    };
    let mut reader = reader;
    let mut buf = Vec::with_capacity(256);
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        // G-code is ASCII; a line that isn't UTF-8 can't be anything we read.
        let Ok(line) = std::str::from_utf8(&buf) else {
            continue;
        };
        line_event(&mut st, line.trim(), &mut f);
    }
}

fn line_event(st: &mut State, line: &str, f: &mut impl FnMut(Event)) {
    if let Some(comment) = line.strip_prefix(';') {
        comment_event(st, comment, f);
        return;
    }
    let code = line.split(';').next().unwrap_or("").trim();
    let mut words = code.split_ascii_whitespace();
    let Some(cmd) = words.next() else {
        return;
    };
    match cmd.to_ascii_uppercase().as_str() {
        "G0" | "G1" | "G00" | "G01" => linear(st, words, f),
        "G2" | "G3" | "G02" | "G03" => arc(st, cmd.ends_with('2'), words, f),
        "G90" => st.abs_xyz = true,
        "G91" => st.abs_xyz = false,
        "M82" => st.abs_e = true,
        "M83" => st.abs_e = false,
        "G92" => {
            for w in words {
                if let Some((axis, v)) = param(w) {
                    st.pos[axis] = v;
                }
            }
        }
        t if t.len() > 1 && t.starts_with('T') => {
            if let Ok(n) = t[1..].parse::<u8>() {
                st.tool = n;
            }
        }
        _ => {}
    }
}

fn comment_event(st: &mut State, comment: &str, f: &mut impl FnMut(Event)) {
    if let Some(t) = comment.strip_prefix("TYPE:") {
        st.feature = Feature::from_type(t);
    } else if let Some(w) = comment.strip_prefix("WIDTH:") {
        st.width = positive(w).unwrap_or(st.width);
    } else if let Some(h) = comment.strip_prefix("HEIGHT:") {
        st.height = positive(h).unwrap_or(st.height);
    } else {
        let c = comment.trim();
        if c == "CONFIG_BLOCK_START" {
            st.in_config = true;
        } else if c == "CONFIG_BLOCK_END" {
            st.in_config = false;
        } else if st.in_config
            && let Some((k, v)) = c.split_once(" = ")
        {
            f(Event::Config(k.trim(), v.trim()));
        }
    }
}

fn positive(s: &str) -> Option<f32> {
    s.trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0 && *v < 50.0)
}

/// `X12.5` → (0, 12.5). Axis indexes: X Y Z E. Anything else (F, S, …) is `None`.
fn param(word: &str) -> Option<(usize, f32)> {
    let mut chars = word.chars();
    let axis = match chars.next()?.to_ascii_uppercase() {
        'X' => 0,
        'Y' => 1,
        'Z' => 2,
        'E' => 3,
        _ => return None,
    };
    let v: f32 = chars.as_str().parse().ok()?;
    v.is_finite().then_some((axis, v))
}

fn target<'a>(st: &State, words: impl Iterator<Item = &'a str>) -> ([f32; 4], [f32; 2]) {
    let mut to = st.pos;
    let mut ij = [0.0f32; 2];
    for w in words {
        let upper = w.chars().next().map(|c| c.to_ascii_uppercase());
        if let Some(c @ ('I' | 'J')) = upper {
            if let Ok(v) = w[1..].parse::<f32>()
                && v.is_finite()
            {
                ij[usize::from(c == 'J')] = v;
            }
            continue;
        }
        if let Some((axis, v)) = param(w) {
            // Like Klipper: G91 makes E relative too, whatever M82 said.
            let absolute = if axis == 3 {
                st.abs_e && st.abs_xyz
            } else {
                st.abs_xyz
            };
            to[axis] = if absolute { v } else { st.pos[axis] + v };
        }
    }
    (to, ij)
}

fn linear<'a>(st: &mut State, words: impl Iterator<Item = &'a str>, f: &mut impl FnMut(Event)) {
    let (to, _) = target(st, words);
    emit(
        st,
        [st.pos[0], st.pos[1]],
        [to[0], to[1]],
        to[2],
        to[3] - st.pos[3],
        f,
    );
    st.pos = to;
}

fn arc<'a>(st: &mut State, clockwise: bool, words: impl Iterator<Item = &'a str>, f: &mut impl FnMut(Event)) {
    let (to, [i, j]) = target(st, words);
    let start = [st.pos[0], st.pos[1]];
    let centre = [start[0] + i, start[1] + j];
    let r = i.hypot(j);
    let de = to[3] - st.pos[3];
    if !(1e-4..=10_000.0).contains(&r) {
        emit(st, start, [to[0], to[1]], to[2], de, f);
        st.pos = to;
        return;
    }
    let a0 = (start[1] - centre[1]).atan2(start[0] - centre[0]);
    let a1 = (to[1] - centre[1]).atan2(to[0] - centre[0]);
    let tau = std::f32::consts::TAU;
    let mut sweep = a1 - a0;
    if clockwise {
        if sweep >= -1e-6 {
            sweep -= tau;
        }
    } else if sweep <= 1e-6 {
        sweep += tau;
    }
    let steps = ((sweep.abs() * r / ARC_CHORD).ceil() as usize).clamp(1, 720);
    let mut prev = start;
    for k in 1..=steps {
        let p = if k == steps {
            [to[0], to[1]]
        } else {
            let a = a0 + sweep * k as f32 / steps as f32;
            [centre[0] + r * a.cos(), centre[1] + r * a.sin()]
        };
        emit(st, prev, p, to[2], de / steps as f32, f);
        prev = p;
    }
    st.pos = to;
}

fn emit(st: &State, a: [f32; 2], b: [f32; 2], z: f32, de: f32, f: &mut impl FnMut(Event)) {
    if de <= 1e-6 || st.feature == Feature::Custom {
        return;
    }
    if (b[0] - a[0]).hypot(b[1] - a[1]) < 1e-4 {
        return;
    }
    f(Event::Segment(Segment {
        a,
        b,
        z,
        width: st.width,
        height: st.height,
        feature: st.feature,
        tool: st.tool,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(text: &str) -> Vec<Segment> {
        let mut out = Vec::new();
        walk(text.as_bytes(), |e| {
            if let Event::Segment(s) = e {
                out.push(s)
            }
        })
        .unwrap();
        out
    }

    #[test]
    fn relative_extrusion_and_features() {
        let g = "G90\nM83\nG1 X1 Y1 E5\n;TYPE:Outer wall\n;WIDTH:0.42\n;HEIGHT:0.2\nG1 Z0.2\n\
                 G1 X10 Y1 E.5\nG1 X10 Y10 E-.8 ; retract\nG1 X12 Y10 E-.2 ; wipe\n\
                 G0 X0 Y0\n;TYPE:Support\nG1 X5 Y0 E.1\n";
        let s = segments(g);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[0].a, [1.0, 1.0]);
        assert_eq!(s[0].b, [10.0, 1.0]);
        assert_eq!(s[0].z, 0.2);
        assert_eq!(s[0].width, 0.42);
        assert_eq!(s[0].feature, Feature::Model);
        assert_eq!(s[1].feature, Feature::Support);
        for (t, f) in [
            ("Sparse infill", Feature::Interior),
            ("Internal solid infill", Feature::Interior),
            ("Internal Bridge", Feature::Interior),
            ("Bridge", Feature::Model),
            ("Top surface", Feature::Model),
            ("Gap infill", Feature::Model),
            ("Brim", Feature::Adhesion),
            ("Skirt", Feature::Adhesion),
            ("Prime tower", Feature::Tower),
            ("Support interface", Feature::Support),
            ("Custom", Feature::Custom),
        ] {
            assert_eq!(Feature::from_type(t), f, "{t}");
        }
    }

    #[test]
    fn absolute_extrusion_and_g92() {
        let g = ";TYPE:Inner wall\nM82\nG1 X0 Y0 E1\nG1 X5 Y0 E2\nG1 X5 Y5 E2\nG92 E0\nG1 X0 Y5 E0.5\n\
                 G91\nG1 X1 E0.1\n";
        let s = segments(g);
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!(s[2].a, [0.0, 5.0]);
        assert_eq!(s[2].b, [1.0, 5.0]);
    }

    #[test]
    fn arcs_become_chords() {
        // A half circle of radius 5 around (5, 0), counter-clockwise from (0,0) to (10,0).
        let g = ";TYPE:Outer wall\nM83\nG1 X0 Y0\nG3 X10 Y0 I5 J0 E1\n";
        let s = segments(g);
        assert!(s.len() > 20, "{}", s.len());
        assert_eq!(s.last().unwrap().b, [10.0, 0.0]);
        // Counter-clockwise from the left goes through the bottom (y < 0).
        assert!(s.iter().all(|seg| seg.b[1] <= 1e-3), "{s:?}");
        let cw = segments(";TYPE:Outer wall\nM83\nG1 X0 Y0\nG2 X10 Y0 I5 J0 E1\n");
        assert!(cw.iter().all(|seg| seg.b[1] >= -1e-3));
    }

    #[test]
    fn start_gcode_and_config() {
        let g = "M83\nG1 X0 Y0 E10 ; purge line before any TYPE\nG1 X50 E5\n;TYPE:Custom\nG1 X0 E5\n\
                 ; CONFIG_BLOCK_START\n; thumbnails = 48x48/PNG,300x300/PNG\n; printable_area = 0x0,250x0\n\
                 ; CONFIG_BLOCK_END\n; not_config = 1\n";
        let mut config = Vec::new();
        let mut n = 0;
        walk(g.as_bytes(), |e| match e {
            Event::Segment(_) => n += 1,
            Event::Config(k, v) => config.push((k.to_string(), v.to_string())),
        })
        .unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            config,
            [
                ("thumbnails".to_string(), "48x48/PNG,300x300/PNG".to_string()),
                ("printable_area".to_string(), "0x0,250x0".to_string()),
            ]
        );
    }

    #[test]
    fn tools_and_garbage() {
        let g = ";TYPE:Outer wall\nM83\nT1\nG1 X1 E1\nG1 Xnan Y1 E1\nG1 X2 Ezz\n\u{ff}\nT\n";
        let s = segments(g);
        assert_eq!(s.len(), 2);
        assert!(s.iter().all(|seg| seg.tool == 1));
    }
}
