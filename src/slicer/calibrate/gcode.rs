//! Turns Orca's G-code for a calibration model into the test: per-band commands (temperature,
//! pressure advance, input shaper, corner velocity), per-band retraction lengths, per-cell flow,
//! per-band outer wall speeds, and the pressure advance line test's bars.
//!
//! Orca's CLI has no calibration mode (its GUI changes these values while it writes the G-code),
//! so printpit edits the G-code afterwards. It reads Orca's own markers: `;Z:` after each layer
//! change and `;TYPE:` before each kind of extrusion. Start and end G-code (`;TYPE:Custom`, or
//! anything before the first `;TYPE:`) is never changed. Extrusion may be relative or absolute
//! (`M82`/`M83`, `G90`/`G91`, `G92 E`); a rewritten absolute E stays consistent with the rest.

use std::io::{BufRead, Write};

/// What to do to the G-code.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// One value per band of `band` mm, from the bottom; a command goes in at each layer that
    /// starts a new band.
    Command {
        command: Command,
        band: f64,
        values: Vec<f64>,
        /// Lines that go in once, before the first band's command.
        first: Vec<String>,
    },
    /// Extrusion inside each cell (model coordinates, `(x0, y0, x1, y1)`) is scaled by its factor.
    /// `center` is the centre of the model's XY bounds: Orca moves the model to the middle of the
    /// bed, so the offset comes from the extrusion's own bounds.
    Flow {
        cells: Vec<[f64; 4]>,
        factors: Vec<f64>,
        center: (f64, f64),
        /// Match cells by x alone (a belt printer's G-code is in its tilted frame, where only the
        /// axis it tilts about, x, is the model's).
        x_only: bool,
    },
    /// Every retraction (and the matching unretraction) uses its band's length. Bands start at
    /// `base` mm (below it, the first band's).
    Retraction { base: f64, band: f64, values: Vec<f64> },
    /// The outer wall prints at its band's speed. With `area` (the filament's cross-section, mm²)
    /// the values are volumetric (mm³/s) and each move's feed rate comes from its own extrusion
    /// per mm; without, they're mm/s.
    Speed {
        band: f64,
        values: Vec<f64>,
        area: Option<f64>,
    },
    /// The pressure advance line and pattern tests: each part (a bar, a chevron) is a union of
    /// convex polygons in model coordinates, as for [`Plan::Flow`]'s cells, and gets its value
    /// before its first move. With `cut`, a part's moves are also cut into slow, fast, slow pieces.
    PressureAdvanceParts {
        parts: Vec<Part>,
        values: Vec<f64>,
        cut: Option<Cut>,
        center: (f64, f64),
        /// What a part is called in the G-code's comments (`line`, `chevron`).
        name: &'static str,
    },
}

/// A test piece's part as a union of convex polygons (model coordinates, corners in order).
pub type Part = Vec<Vec<(f64, f64)>>;

/// Where the pressure advance lines speed up and slow down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cut {
    /// Model x where the fast middle starts and ends.
    pub fast_from: f64,
    pub fast_to: f64,
    /// Slow and fast speeds, mm/s.
    pub slow: f64,
    pub fast: f64,
}

/// Whether `p` is inside the convex polygon `poly`, or within `margin` of it.
pub fn in_convex(poly: &[(f64, f64)], p: (f64, f64), margin: f64) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let area: f64 = (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            a.0 * b.1 - b.0 * a.1
        })
        .sum();
    let sign = if area >= 0.0 { 1.0 } else { -1.0 };
    (0..n).all(|i| {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        let (ex, ey) = (b.0 - a.0, b.1 - a.1);
        let len = (ex * ex + ey * ey).sqrt();
        len < 1e-12 || sign * (ex * (p.1 - a.1) - ey * (p.0 - a.0)) / len >= -margin
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `M104 S<t>` (no wait, as Orca's GUI does).
    Temperature,
    /// `SET_PRESSURE_ADVANCE ADVANCE=<v>` (Klipper).
    PressureAdvance,
    /// `SET_INPUT_SHAPER SHAPER_FREQ_X=<v> SHAPER_FREQ_Y=<v>`.
    ShaperFrequency,
    /// `SET_INPUT_SHAPER DAMPING_RATIO_X=<v> DAMPING_RATIO_Y=<v>`.
    ShaperDamping,
    /// `SET_VELOCITY_LIMIT SQUARE_CORNER_VELOCITY=<v>`.
    CornerVelocity,
}

impl Command {
    /// The command for `v` (Klipper's, as Orca's GCodeWriter writes them).
    pub fn line(self, v: f64) -> String {
        match self {
            Command::Temperature => format!("M104 S{}", fmt(v, 0)),
            Command::PressureAdvance => format!("SET_PRESSURE_ADVANCE ADVANCE={}", fmt(v, 4)),
            Command::ShaperFrequency => format!("SET_INPUT_SHAPER SHAPER_FREQ_X={v:.2} SHAPER_FREQ_Y={v:.2}"),
            Command::ShaperDamping => {
                format!("SET_INPUT_SHAPER DAMPING_RATIO_X={v:.3} DAMPING_RATIO_Y={v:.3}")
            }
            Command::CornerVelocity => format!("SET_VELOCITY_LIMIT SQUARE_CORNER_VELOCITY={}", fmt(v, 2)),
        }
    }
}

/// What the rewrite did (for the log, and to refuse G-code it couldn't work with).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub layers: usize,
    pub commands: usize,
    pub moves: usize,
}

/// Which band a layer whose top is at `z` belongs to: band `i` holds `(base + i·band, base + (i+1)·band]`.
pub fn band_of(z: f64, base: f64, band: f64, count: usize) -> usize {
    if count == 0 || z <= base + 1e-3 * band {
        return 0;
    }
    // Orca's layer heights are a little inexact (0.199951), so a layer ending within 1 µm per mm
    // of band of a boundary belongs below it.
    let i = ((z - base) / band - 1e-3).ceil() as i64 - 1;
    i.clamp(0, count as i64 - 1) as usize
}

/// A number as G-code wants it: up to `places` decimals, no trailing zeros.
pub fn fmt(v: f64, places: usize) -> String {
    let s = format!("{v:.places$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        &s
    };
    if s == "-0" { "0".into() } else { s.to_string() }
}

/// A parsed motion line: the code part's words, and the comment kept as it was.
struct Words<'a> {
    code: &'a str,
    comment: &'a str,
}

impl<'a> Words<'a> {
    fn split(line: &'a str) -> Self {
        match line.find(';') {
            Some(i) => Self {
                code: &line[..i],
                comment: &line[i..],
            },
            None => Self {
                code: line,
                comment: "",
            },
        }
    }

    fn command(&self) -> &'a str {
        self.code.split_whitespace().next().unwrap_or("")
    }

    fn get(&self, letter: char) -> Option<f64> {
        self.code
            .split_whitespace()
            .skip(1)
            .find(|w| w.starts_with(letter))
            .and_then(|w| w[1..].parse().ok())
    }

    fn has(&self, letter: char) -> bool {
        self.code
            .split_whitespace()
            .skip(1)
            .any(|w| w.starts_with(letter))
    }

    /// The line with `letter` set to `value` (replaced, or added after the last word).
    fn with(&self, sets: &[(char, String)]) -> String {
        let mut words: Vec<String> = self.code.split_whitespace().map(String::from).collect();
        for (letter, value) in sets {
            match words.iter_mut().skip(1).find(|w| w.starts_with(*letter)) {
                Some(w) => *w = format!("{letter}{value}"),
                None => words.push(format!("{letter}{value}")),
            }
        }
        let mut out = words.join(" ");
        if !self.comment.is_empty() {
            out.push(' ');
            out.push_str(self.comment);
        }
        out
    }
}

/// Where the print head is and how E counts.
struct Machine {
    pos: [f64; 3],
    absolute: bool,
    absolute_e: bool,
    /// The E position the original G-code believes in, and the one the printer will be at.
    e_old: f64,
    e_new: f64,
    /// The feed rate the original G-code has in force, and the one the printer has.
    f_old: Option<f64>,
    f_new: Option<f64>,
}

impl Machine {
    fn e_relative(&self) -> bool {
        !self.absolute || !self.absolute_e
    }
}

/// Kinds of extrusion that aren't the test piece itself.
fn is_piece(kind: Option<&str>) -> bool {
    kind.is_some_and(|k| !matches!(k, "Custom" | "Skirt" | "Brim") && !k.starts_with("Support"))
}

/// What a first pass over the G-code finds about the test piece.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Pre {
    /// Its extrusion's XY bounds (start/end G-code, skirt, brim and supports left out).
    bounds: Option<[f64; 4]>,
    /// The bottom of its first layer (`;Z:` minus `;HEIGHT:`): on a belt printer it stands on a
    /// support wedge, so its bands start there rather than at 0.
    base: Option<f64>,
}

fn prepass(input: impl BufRead) -> std::io::Result<Pre> {
    let mut m = Machine {
        pos: [0.0; 3],
        absolute: true,
        absolute_e: true,
        e_old: 0.0,
        e_new: 0.0,
        f_old: None,
        f_new: None,
    };
    let mut kind: Option<String> = None;
    let mut pre = Pre::default();
    let (mut z, mut height) = (0.0f64, None::<f64>);
    for line in input.lines() {
        let line = line?;
        if let Some(t) = line.strip_prefix(";TYPE:") {
            kind = Some(t.trim().to_string());
            continue;
        }
        if let Some(v) = line.strip_prefix(";Z:") {
            z = v.trim().parse().unwrap_or(z);
            height = None;
            continue;
        }
        if let Some(v) = line.strip_prefix(";HEIGHT:") {
            height = height.or(v.trim().parse().ok());
            continue;
        }
        let w = Words::split(&line);
        let Some(delta) = motion(&mut m, &w) else {
            continue;
        };
        if is_piece(kind.as_deref()) && delta.e > 0.0 && delta.moved_xy {
            pre.base.get_or_insert(z - height.unwrap_or(0.0));
            let r = pre.bounds.get_or_insert([f64::MAX, f64::MAX, f64::MIN, f64::MIN]);
            for p in [delta.from, m.pos] {
                r[0] = r[0].min(p[0]);
                r[1] = r[1].min(p[1]);
                r[2] = r[2].max(p[0]);
                r[3] = r[3].max(p[1]);
            }
        }
    }
    Ok(pre)
}

struct Delta {
    from: [f64; 3],
    e: f64,
    moved_xy: bool,
    pure_e: bool,
}

/// Follows a line's effect on the position and modes. For a motion line, returns the move (with
/// the E delta the original G-code meant). Updates only the original-side E; callers set `e_new`.
fn motion(m: &mut Machine, w: &Words) -> Option<Delta> {
    match w.command() {
        "G90" => m.absolute = true,
        "G91" => m.absolute = false,
        "M82" => m.absolute_e = true,
        "M83" => m.absolute_e = false,
        "G92" => {
            if let Some(e) = w.get('E') {
                m.e_old = e;
                m.e_new = e;
            }
            for (i, l) in ['X', 'Y', 'Z'].into_iter().enumerate() {
                if let Some(v) = w.get(l) {
                    m.pos[i] = v;
                }
            }
        }
        "G0" | "G1" | "G2" | "G3" => {
            let from = m.pos;
            for (i, l) in ['X', 'Y', 'Z'].into_iter().enumerate() {
                if let Some(v) = w.get(l) {
                    m.pos[i] = if m.absolute { v } else { m.pos[i] + v };
                }
            }
            let e = match w.get('E') {
                Some(v) if m.e_relative() => v,
                Some(v) => {
                    let d = v - m.e_old;
                    m.e_old = v;
                    d
                }
                None => 0.0,
            };
            let moved_xy = (m.pos[0] - from[0]).abs() > 1e-9 || (m.pos[1] - from[1]).abs() > 1e-9;
            let pure_e = w.has('E') && !w.has('X') && !w.has('Y') && !w.has('Z');
            return Some(Delta {
                from,
                e,
                moved_xy,
                pure_e,
            });
        }
        _ => {}
    }
    None
}

/// Rewrites `input` into `out` by `plan`.
/// With `from_piece`, bands are measured from the test piece's first layer (a belt printer's,
/// standing on its support wedge) instead of from 0.
pub fn rewrite(
    input: impl BufRead,
    first_pass: impl FnOnce() -> std::io::Result<Box<dyn BufRead>>,
    mut out: impl Write,
    plan: &Plan,
    from_piece: bool,
) -> Result<Summary, String> {
    let io = |e: std::io::Error| format!("couldn't rewrite the G-code: {e}");
    let needs_bounds = matches!(plan, Plan::Flow { .. } | Plan::PressureAdvanceParts { .. });
    let pre = if needs_bounds || from_piece {
        prepass(first_pass().map_err(io)?).map_err(io)?
    } else {
        Pre::default()
    };
    let offset = match plan {
        Plan::Flow { center, .. } | Plan::PressureAdvanceParts { center, .. } => {
            let b = pre.bounds.ok_or("the G-code has no extrusion to calibrate")?;
            Some(((b[0] + b[2]) / 2.0 - center.0, (b[1] + b[3]) / 2.0 - center.1))
        }
        _ => None,
    };
    let base = if from_piece {
        pre.base.ok_or("the G-code has no extrusion to calibrate")?
    } else {
        0.0
    };
    let mut m = Machine {
        pos: [0.0; 3],
        absolute: true,
        absolute_e: true,
        e_old: 0.0,
        e_new: 0.0,
        f_old: None,
        f_new: None,
    };
    let mut sum = Summary::default();
    // None before the first `;TYPE:`, which is start G-code.
    let mut kind: Option<String> = None;
    let mut z = 0.0f64;
    let mut band: Option<usize> = None;
    // A retraction in force: (what the G-code retracted, what we retracted instead).
    let mut retracted: Option<(f64, f64)> = None;
    for line in input.lines() {
        let line = line.map_err(io)?;
        if let Some(t) = line.strip_prefix(";TYPE:") {
            kind = Some(t.trim().to_string());
            writeln!(out, "{line}").map_err(io)?;
            continue;
        }
        if let Some(v) = line.strip_prefix(";Z:") {
            writeln!(out, "{line}").map_err(io)?;
            z = v.trim().parse::<f64>().map_or(z, |v| v - base);
            sum.layers += 1;
            if let Plan::Command {
                command,
                band: size,
                values,
                first,
            } = plan
            {
                let i = band_of(z, 0.0, *size, values.len());
                if band != Some(i) {
                    if band.is_none() {
                        for l in first {
                            writeln!(out, "{l} ; printpit calibration").map_err(io)?;
                        }
                    }
                    band = Some(i);
                    writeln!(
                        out,
                        "{} ; printpit calibration: band {}",
                        command.line(values[i]),
                        i + 1
                    )
                    .map_err(io)?;
                    sum.commands += 1;
                }
            }
            continue;
        }
        let w = Words::split(&line);
        let printing = kind.as_deref().is_some_and(|k| k != "Custom");
        let f_word = w.get('F');
        let Some(d) = motion(&mut m, &w) else {
            writeln!(out, "{line}").map_err(io)?;
            continue;
        };
        if let Plan::PressureAdvanceParts {
            parts,
            values,
            cut,
            name,
            ..
        } = plan
            && printing
            && d.e > 0.0
            && d.moved_xy
        {
            let (ox, oy) = offset.unwrap_or_default();
            let a = (d.from[0] - ox, d.from[1] - oy);
            let b = (m.pos[0] - ox, m.pos[1] - oy);
            let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            const MARGIN: f64 = 0.6;
            if let Some(i) = parts
                .iter()
                .position(|polys| polys.iter().any(|poly| in_convex(poly, mid, MARGIN)))
            {
                if band != Some(i) {
                    band = Some(i);
                    writeln!(
                        out,
                        "{} ; printpit calibration: {name} {}",
                        Command::PressureAdvance.line(values[i]),
                        i + 1
                    )
                    .map_err(io)?;
                    sum.commands += 1;
                }
                sum.moves += 1;
                if let Some(cut) = cut
                    && w.command() == "G1"
                {
                    if let Some(f) = f_word {
                        m.f_old = Some(f);
                    }
                    // Cut where the speed changes, then one move per piece.
                    let mut cuts = vec![0.0, 1.0];
                    if (b.0 - a.0).abs() > 1e-9 {
                        for x in [cut.fast_from, cut.fast_to] {
                            let t = (x - a.0) / (b.0 - a.0);
                            if t > 1e-6 && t < 1.0 - 1e-6 {
                                cuts.push(t);
                            }
                        }
                    }
                    cuts.sort_by(f64::total_cmp);
                    sum.moves += cuts.len() - 2;
                    for pair in cuts.windows(2) {
                        let (t0, t1) = (pair[0], pair[1]);
                        let end = [
                            d.from[0] + (m.pos[0] - d.from[0]) * t1,
                            d.from[1] + (m.pos[1] - d.from[1]) * t1,
                        ];
                        let xm = a.0 + (b.0 - a.0) * (t0 + t1) / 2.0;
                        let speed = if xm > cut.fast_from && xm < cut.fast_to {
                            cut.fast
                        } else {
                            cut.slow
                        };
                        let f = (speed * 60.0).round();
                        let e = d.e * (t1 - t0);
                        let e_word = if m.e_relative() {
                            fmt(e, 5)
                        } else {
                            m.e_new += e;
                            fmt(m.e_new, 5)
                        };
                        writeln!(
                            out,
                            "G1 X{} Y{} E{} F{}",
                            fmt(end[0], 3),
                            fmt(end[1], 3),
                            e_word,
                            fmt(f, 0)
                        )
                        .map_err(io)?;
                        m.f_new = Some(f);
                    }
                    continue;
                }
            }
        }
        let mut new_e = d.e;
        let mut new_f: Option<f64> = None;
        if printing {
            match plan {
                Plan::Command { .. } | Plan::PressureAdvanceParts { .. } => {}
                Plan::Flow {
                    cells,
                    factors,
                    x_only,
                    ..
                } => {
                    if d.e > 0.0 && d.moved_xy && is_piece(kind.as_deref()) {
                        let (ox, oy) = offset.unwrap_or_default();
                        let x = (d.from[0] + m.pos[0]) / 2.0 - ox;
                        let y = (d.from[1] + m.pos[1]) / 2.0 - oy;
                        const MARGIN: f64 = 1.5;
                        if let Some(i) = cells.iter().position(|c| {
                            x >= c[0] - MARGIN
                                && x <= c[2] + MARGIN
                                && (*x_only || (y >= c[1] - MARGIN && y <= c[3] + MARGIN))
                        }) {
                            new_e = d.e * factors[i];
                            sum.moves += 1;
                        }
                    }
                }
                Plan::Retraction {
                    base,
                    band: size,
                    values,
                } => {
                    if d.pure_e {
                        let len = values[band_of(z, *base, *size, values.len())];
                        if d.e < 0.0 {
                            new_e = -len;
                            retracted = Some((-d.e, len));
                            sum.moves += 1;
                        } else if let Some((orig, ours)) = retracted.take() {
                            // Keeps any extra the profile primes after a retraction.
                            new_e = (ours + (d.e - orig)).max(0.0);
                            sum.moves += 1;
                        }
                    }
                }
                Plan::Speed {
                    band: size,
                    values,
                    area,
                } => {
                    let len = ((m.pos[0] - d.from[0]).powi(2)
                        + (m.pos[1] - d.from[1]).powi(2)
                        + (m.pos[2] - d.from[2]).powi(2))
                    .sqrt();
                    if kind.as_deref() == Some("Outer wall") && d.e > 0.0 && d.moved_xy && len > 1e-3 {
                        let target = values[band_of(z, 0.0, *size, values.len())];
                        let mm_s = match area {
                            Some(area) => {
                                let per_mm = d.e * area / len;
                                (per_mm > 1e-6).then(|| target / per_mm)
                            }
                            None => Some(target),
                        };
                        if let Some(v) = mm_s {
                            new_f = Some((v * 60.0).round());
                            sum.moves += 1;
                        }
                    }
                }
            }
        }
        let mut sets: Vec<(char, String)> = Vec::new();
        if w.has('E') {
            if m.e_relative() {
                if (new_e - d.e).abs() > 1e-12 {
                    sets.push(('E', fmt(new_e, 5)));
                }
            } else {
                m.e_new += new_e;
                if (m.e_new - m.e_old).abs() > 1e-9 {
                    sets.push(('E', fmt(m.e_new, 5)));
                }
            }
        }
        // Feed rates: ours where we set one, else put the original's back before it's used.
        if let Some(f) = f_word {
            m.f_old = Some(f);
        }
        match new_f {
            Some(f) => {
                sets.push(('F', fmt(f, 0)));
                m.f_new = Some(f);
            }
            None => {
                if f_word.is_none() && m.f_new != m.f_old && m.f_old.is_some() {
                    sets.push(('F', fmt(m.f_old.unwrap_or_default(), 3)));
                }
                m.f_new = m.f_old;
            }
        }
        if sets.is_empty() {
            writeln!(out, "{line}").map_err(io)?;
        } else {
            writeln!(out, "{}", w.with(&sets)).map_err(io)?;
        }
    }
    out.flush().map_err(io)?;
    if sum.layers == 0 {
        return Err("the G-code has no layer markers (;Z:), so the test can't be set up".into());
    }
    let wanted = match plan {
        Plan::Command { .. } => sum.commands,
        _ => sum.moves,
    };
    if wanted == 0 {
        return Err("found nothing in the G-code to change for this test".into());
    }
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str, plan: &Plan) -> (String, Summary) {
        let mut out = Vec::new();
        let owned = src.to_string();
        let sum = rewrite(
            src.as_bytes(),
            move || Ok(Box::new(std::io::Cursor::new(owned.into_bytes())) as Box<dyn BufRead>),
            &mut out,
            plan,
            false,
        )
        .unwrap();
        (String::from_utf8(out).unwrap(), sum)
    }

    fn layers(zs: &[f64], body: &str) -> String {
        let mut s = String::from("G28\nM83\nG1 E10 F300 ; purge\n");
        for z in zs {
            s.push_str(&format!(";LAYER_CHANGE\n;Z:{z}\nG1 Z{z}\n{body}"));
        }
        s.push_str(";TYPE:Custom\nG1 E-2\nM104 S0\n");
        s
    }

    #[test]
    fn bands() {
        assert_eq!(band_of(0.2, 0.0, 10.0, 5), 0);
        assert_eq!(band_of(10.0, 0.0, 10.0, 5), 0);
        assert_eq!(band_of(10.2, 0.0, 10.0, 5), 1);
        assert_eq!(band_of(99.0, 0.0, 10.0, 5), 4);
        assert_eq!(band_of(0.2, 0.4, 1.0, 5), 0);
        assert_eq!(band_of(1.6, 0.4, 1.0, 5), 1);
        assert_eq!(fmt(0.0200, 4), "0.02");
        assert_eq!(fmt(215.0, 0), "215");
        assert_eq!(fmt(-0.0, 3), "0");
    }

    #[test]
    fn temperatures_change_once_per_band() {
        let src = layers(&[5.0, 10.0, 15.0, 20.0, 25.0], ";TYPE:Outer wall\nG1 X1 Y1 E.1\n");
        let plan = Plan::Command {
            command: Command::Temperature,
            band: 10.0,
            values: vec![230.0, 225.0, 220.0],
            first: Vec::new(),
        };
        let (out, sum) = run(&src, &plan);
        let cmds: Vec<&str> = out.lines().filter(|l| l.starts_with("M104 S2")).collect();
        assert_eq!(cmds.len(), 3, "{out}");
        assert!(cmds[0].starts_with("M104 S230 ") && cmds[2].starts_with("M104 S220 "));
        // The block above the last band keeps the last value; the end G-code is untouched.
        assert!(out.contains("M104 S0\n"));
        assert_eq!(sum.layers, 5);
        let at = |needle: &str| out.find(needle).unwrap();
        assert!(at(";Z:15\n") < at("S225") && at("S225") < at(";Z:20\n"));
    }

    #[test]
    fn pressure_advance_goes_in_at_the_first_layer() {
        let src = layers(&[0.2, 0.4, 1.2], ";TYPE:Outer wall\nG1 X1 Y1 E.1\n");
        let plan = Plan::Command {
            command: Command::PressureAdvance,
            band: 1.0,
            values: vec![0.0, 0.002],
            first: Vec::new(),
        };
        let (out, _) = run(&src, &plan);
        assert!(out.contains(";Z:0.2\nSET_PRESSURE_ADVANCE ADVANCE=0 ;"), "{out}");
        assert!(
            out.contains(";Z:1.2\nSET_PRESSURE_ADVANCE ADVANCE=0.002 ;"),
            "{out}"
        );
        assert_eq!(out.matches("SET_PRESSURE_ADVANCE").count(), 2);
    }

    #[test]
    fn flow_scales_each_cell() {
        // Two 10 mm cells 10 mm apart; the G-code has them centred on (100, 100).
        let cells = vec![[0.0, 0.0, 10.0, 10.0], [20.0, 0.0, 30.0, 10.0]];
        let src = "M83\n;LAYER_CHANGE\n;Z:0.2\n;TYPE:Skirt\nG1 X0 Y0\nG1 X200 Y0 E5\n\
                   ;TYPE:Outer wall\nG1 X85 Y95 F600\nG1 X95 Y95 E1\nG1 X95 Y105 E1\nG1 E-.8\n\
                   G1 X105 Y95\nG1 E.8\nG1 X115 Y95 E1 ; second\nG1 X115 Y105 E1\n";
        let plan = Plan::Flow {
            cells,
            factors: vec![0.8, 1.1],
            center: (15.0, 5.0),
            x_only: false,
        };
        let (out, sum) = run(src, &plan);
        assert_eq!(sum.moves, 4);
        assert!(out.contains("G1 X95 Y95 E0.8\n"), "{out}");
        assert!(out.contains("G1 X115 Y95 E1.1 ; second\n"), "{out}");
        // Retractions, travel and the skirt are left alone.
        assert!(out.contains("G1 E-.8\n") && out.contains("G1 E.8\n") && out.contains("G1 X200 Y0 E5\n"));
    }

    #[test]
    fn flow_keeps_absolute_e_consistent() {
        let src = "M82\n;Z:0.2\n;TYPE:Top surface\nG92 E0\nG1 X0 Y0\nG1 X10 Y0 E1\nG1 E0.2\nG1 X10 Y10 E1.2\n\
                   G92 E0\nG1 X0 Y10 E1\n";
        let plan = Plan::Flow {
            cells: vec![[0.0, 0.0, 10.0, 10.0]],
            factors: vec![0.5],
            center: (5.0, 5.0),
            x_only: false,
        };
        let (out, _) = run(src, &plan);
        assert!(out.contains("G1 X10 Y0 E0.5\n"), "{out}");
        // A retraction keeps its own delta (−0.8 from 0.5), then +1 is scaled to 0.5.
        assert!(out.contains("G1 E-0.3\n"), "{out}");
        assert!(out.contains("G1 X10 Y10 E0.2\n"), "{out}");
        assert!(out.contains("G1 X0 Y10 E0.5\n"), "{out}");
    }

    #[test]
    fn retraction_lengths_follow_the_bands() {
        let body =
            ";TYPE:Outer wall\nG1 X1 Y1 E.1\nG1 E-.8 F2100\nG1 X40 Y1 F12000\nG1 E.82 F2100\nG1 X41 Y1 E.1\n";
        let src = layers(&[0.2, 0.4, 1.4, 2.4, 3.4], body);
        let plan = Plan::Retraction {
            base: 0.4,
            band: 1.0,
            values: vec![0.0, 0.5, 1.0],
        };
        let (out, sum) = run(&src, &plan);
        assert_eq!(sum.moves, 10);
        let rs: Vec<&str> = out.lines().filter(|l| l.starts_with("G1 E")).collect();
        assert_eq!(
            rs,
            [
                "G1 E10 F300 ; purge", // start G-code, untouched
                "G1 E0 F2100",
                "G1 E0.02 F2100",
                "G1 E0 F2100",
                "G1 E0.02 F2100",
                "G1 E0 F2100",
                "G1 E0.02 F2100",
                "G1 E-0.5 F2100",
                "G1 E0.52 F2100",
                "G1 E-1 F2100",
                "G1 E1.02 F2100",
                "G1 E-2" // end G-code
            ]
        );
    }

    #[test]
    fn speed_sets_feed_rates_and_puts_the_old_one_back() {
        // 10 mm moves extruding 1 mm of 1.75 filament: 2.405 mm³ per mm of travel.
        let area = std::f64::consts::PI * 0.875 * 0.875;
        let body =
            ";TYPE:Outer wall\nG1 F3000\nG1 X10 Y0 E1\nG1 X10 Y10 E1\n;TYPE:Inner wall\nG1 X0 Y10 E1\n";
        let src = format!("M83\n{}", layers(&[0.5, 1.5], body));
        let plan = Plan::Speed {
            band: 1.0,
            values: vec![10.0, 20.0],
            area: Some(area),
        };
        let (out, sum) = run(&src, &plan);
        assert_eq!(sum.moves, 4);
        let f1 = (10.0 / area * 10.0 * 60.0f64).round();
        let f2 = (20.0 / area * 10.0 * 60.0f64).round();
        assert!(out.contains(&format!("G1 X10 Y0 E1 F{f1}\n")), "{out}");
        assert!(out.contains(&format!("G1 X10 Y10 E1 F{f2}\n")), "{out}");
        // The inner wall gets the G-code's own feed rate back.
        assert!(out.contains("G1 X0 Y10 E1 F3000\n"), "{out}");
    }

    #[test]
    fn shaper_commands_and_a_preamble() {
        let src = layers(&[1.0, 2.0, 3.0], ";TYPE:Outer wall\nG1 X1 Y1 E.1\n");
        let plan = Plan::Command {
            command: Command::ShaperDamping,
            band: 2.0,
            values: vec![0.05, 0.1],
            first: vec!["SET_INPUT_SHAPER SHAPER_FREQ_X=48.00 SHAPER_FREQ_Y=52.00".into()],
        };
        let (out, sum) = run(&src, &plan);
        assert_eq!(sum.commands, 2);
        assert!(
            out.contains(
                ";Z:1\nSET_INPUT_SHAPER SHAPER_FREQ_X=48.00 SHAPER_FREQ_Y=52.00 ; printpit calibration\n\
             SET_INPUT_SHAPER DAMPING_RATIO_X=0.050 DAMPING_RATIO_Y=0.050 ; printpit calibration: band 1\n"
            ),
            "{out}"
        );
        assert!(
            out.contains(";Z:3\nSET_INPUT_SHAPER DAMPING_RATIO_X=0.100"),
            "{out}"
        );
        assert_eq!(
            Command::CornerVelocity.line(7.5),
            "SET_VELOCITY_LIMIT SQUARE_CORNER_VELOCITY=7.5"
        );
        assert_eq!(
            Command::ShaperFrequency.line(40.0),
            "SET_INPUT_SHAPER SHAPER_FREQ_X=40.00 SHAPER_FREQ_Y=40.00"
        );
    }

    #[test]
    fn linear_speeds_per_band() {
        let body = ";TYPE:Outer wall\nG1 X10 Y0 E1 F3000\n;TYPE:Inner wall\nG1 X0 Y0 E1\n";
        let src = format!("M83\n{}", layers(&[2.0, 7.0], body));
        let plan = Plan::Speed {
            band: 5.0,
            values: vec![40.0, 50.0],
            area: None,
        };
        let (out, _) = run(&src, &plan);
        assert!(out.contains("G1 X10 Y0 E1 F2400\n"), "{out}");
        assert!(out.contains("G1 X10 Y0 E1 F3000\n"), "{out}");
        assert!(
            out.contains("G1 X0 Y0 E1 F3000\n"),
            "the inner wall keeps the G-code's: {out}"
        );
    }

    #[test]
    fn pa_line_bars_are_cut_into_slow_fast_slow() {
        // Two 80 mm bars, 4 mm apart, centred on (100, 100) in the G-code.
        let rect = |y: f64| vec![vec![(0.0, y), (80.0, y), (80.0, y + 1.0), (0.0, y + 1.0)]];
        let bars = vec![rect(0.0), rect(4.0)];
        let src = "M83\n;Z:0.2\n;TYPE:Outer wall\nG1 X60 Y97.5 F600\nG1 X140 Y97.5 E4\nG1 X140 Y97.8 E.05\n\
                   G1 X60 Y101.5\nG1 X140 Y101.5 E4\n;TYPE:Custom\nG1 X0 Y0 E1\n";
        let plan = Plan::PressureAdvanceParts {
            parts: bars,
            values: vec![0.0, 0.02],
            cut: Some(Cut {
                fast_from: 20.0,
                fast_to: 60.0,
                slow: 20.0,
                fast: 100.0,
            }),
            center: (40.0, 2.5),
            name: "line",
        };
        let (out, sum) = run(src, &plan);
        assert_eq!(sum.commands, 2);
        let lines: Vec<&str> = out.lines().collect();
        let i = lines
            .iter()
            .position(|l| l.starts_with("SET_PRESSURE_ADVANCE ADVANCE=0 "))
            .unwrap();
        assert_eq!(
            &lines[i + 1..i + 4],
            [
                "G1 X80 Y97.5 E1 F1200",
                "G1 X120 Y97.5 E2 F6000",
                "G1 X140 Y97.5 E1 F1200"
            ]
        );
        // A short move at the end of a bar is slow.
        assert_eq!(lines[i + 4], "G1 X140 Y97.8 E0.05 F1200");
        assert!(out.contains("SET_PRESSURE_ADVANCE ADVANCE=0.02 ; printpit calibration: line 2\n"));
        // The travel after a bar gets the G-code's feed rate back; the end G-code is untouched.
        assert!(out.contains("G1 X60 Y101.5 F600\n"), "{out}");
        // The end G-code only gets the feed rate it relied on back.
        assert!(out.contains(";TYPE:Custom\nG1 X0 Y0 E1 F600\n"), "{out}");
    }

    #[test]
    fn belt_bands_start_at_the_piece() {
        // Support wedge layers from 600, the piece from 610 (its first layer 0.2 high).
        let mut src = String::from("M83\n");
        for i in 0..60 {
            let z = 600.2 + i as f64 * 0.2;
            let kind = if z < 610.1 { "Support" } else { "Outer wall" };
            src.push_str(&format!(
                ";LAYER_CHANGE\n;Z:{z:.1}\n;HEIGHT:0.2\n;TYPE:{kind}\nG1 X1 Y{i} E.1\n"
            ));
        }
        let plan = Plan::Command {
            command: Command::Temperature,
            band: 1.0,
            values: vec![230.0, 225.0],
            first: Vec::new(),
        };
        let mut out = Vec::new();
        let owned = src.clone();
        let sum = rewrite(
            src.as_bytes(),
            move || Ok(Box::new(std::io::Cursor::new(owned.into_bytes())) as Box<dyn BufRead>),
            &mut out,
            &plan,
            true,
        )
        .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(sum.commands, 2);
        // The piece's first layer is 610.2 (bottom 610.0); band 2 starts above 611.0.
        assert!(out.contains(";Z:611.2\nM104 S225 ;"), "{out}");
        assert!(out.contains(";Z:600.2\nM104 S230 ;"), "{out}");
    }

    #[test]
    fn flow_by_x_alone() {
        let src = "M83\n;Z:500\n;TYPE:Support\nG1 X0 Y0\nG1 X40 Y0 E1\n;TYPE:Top surface\nG1 X5 Y900 F600\n\
                   G1 X9 Y950 E1\nG1 X25 Y700 E1\nG1 X29 Y720 E1\n";
        let plan = Plan::Flow {
            cells: vec![[0.0, 0.0, 10.0, 10.0], [20.0, 0.0, 30.0, 10.0]],
            factors: vec![0.5, 2.0],
            center: (15.0, 5.0),
            x_only: true,
        };
        let (out, sum) = run(src, &plan);
        assert_eq!(sum.moves, 2);
        assert!(out.contains("G1 X9 Y950 E0.5\n"), "{out}");
        // Between the cells: neither.
        assert!(out.contains("G1 X25 Y700 E1\n"), "{out}");
        assert!(out.contains("G1 X29 Y720 E2\n"), "{out}");
        // Supports aren't the piece.
        assert!(out.contains("G1 X40 Y0 E1\n"));
    }

    #[test]
    fn convex_containment() {
        let sq = [(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)];
        let back: Vec<_> = sq.iter().rev().copied().collect();
        for poly in [&sq[..], &back[..]] {
            assert!(in_convex(poly, (1.0, 1.0), 0.0));
            assert!(!in_convex(poly, (2.5, 1.0), 0.0));
            assert!(in_convex(poly, (2.5, 1.0), 0.6));
            assert!(!in_convex(poly, (2.5, 2.5), 0.4));
        }
    }

    #[test]
    fn pa_chevrons_get_their_values_without_cuts() {
        // Two diamonds side by side, centred on (100, 100) in the G-code.
        let diamond = |cx: f64| vec![vec![(cx - 1.0, 0.0), (cx, -1.0), (cx + 1.0, 0.0), (cx, 1.0)]];
        let plan = Plan::PressureAdvanceParts {
            parts: vec![diamond(0.0), diamond(5.0)],
            values: vec![0.01, 0.02],
            cut: None,
            center: (2.5, 0.0),
            name: "chevron",
        };
        let src = "M83\n;Z:0.2\n;TYPE:Outer wall\nG1 X96.5 Y100 F600\nG1 X98.5 Y100 E.1\nG1 X97.5 Y100.5 E.1\n\
                   G1 X101.5 Y100\nG1 X103.5 Y100 E.1\n;Z:0.4\nG1 X96.5 Y100\nG1 X98.5 Y100 E.1\n";
        let (out, sum) = run(src, &plan);
        assert_eq!(sum.commands, 3);
        assert!(
            out.contains(
                "SET_PRESSURE_ADVANCE ADVANCE=0.01 ; printpit calibration: chevron 1\nG1 X98.5 Y100 E.1\n"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "SET_PRESSURE_ADVANCE ADVANCE=0.02 ; printpit calibration: chevron 2\nG1 X103.5 Y100 E.1\n"
            ),
            "{out}"
        );
        // Moves are left as they were.
        assert!(!out.contains(" F1"), "{out}");
    }

    #[test]
    fn refuses_gcode_without_layers() {
        let mut out = Vec::new();
        let plan = Plan::Command {
            command: Command::Temperature,
            band: 10.0,
            values: vec![200.0],
            first: Vec::new(),
        };
        let r = rewrite(
            "G1 X1 E1\n".as_bytes(),
            || Ok(Box::new(std::io::Cursor::new(Vec::new())) as Box<dyn BufRead>),
            &mut out,
            &plan,
            false,
        );
        assert!(r.unwrap_err().contains("no layer markers"));
    }
}
