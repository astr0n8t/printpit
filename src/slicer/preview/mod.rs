//! Pictures of what was sliced, drawn from the G-code itself (PLAN §15.11).
//!
//! OrcaSlicer's CLI writes no thumbnails (§15.10, still so in 2.5.1), so printpit renders them: it reads the
//! extruded toolpath ([`gcode`]), draws every bead as a little box with a software z-buffer
//! ([`raster`]), and
//! - injects Moonraker-style `; thumbnail begin` blocks (the sizes the machine profile's
//!   `thumbnails` setting asks for) into the G-code before it's uploaded, so the Files tab,
//!   Mainsail and KlipperScreen show it;
//! - saves two larger pictures for the job panel: the whole plate from the front (`plate.png`)
//!   and from straight above (`top.png`), with supports, brims and copies where Orca put them.
//!
//! It never fails a job: if anything goes wrong the G-code is uploaded as it came. It reads the
//! file twice (bounds, then drawing) so memory stays at the size of the pictures, however big
//! the print is.

pub mod gcode;
pub mod raster;

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use base64::Engine;

use super::Storage;
use gcode::{Event, Feature, Segment};
use raster::{Frame, Rgb, Vertex};

/// Thumbnail sizes when the profile doesn't say (Orca's own default).
const DEFAULT_THUMBNAILS: [(u32, u32); 2] = [(48, 48), (300, 300)];
const MAX_THUMBNAILS: usize = 4;
const MAX_THUMBNAIL_SIDE: u32 = 1024;
const PLATE_SIZE: (u32, u32) = (960, 640);
const TOP_MAX: u32 = 720;
/// Orca's default filament colour, for G-code that doesn't name one.
const DEFAULT_COLOR: Rgb = [0.949, 0.459, 0.306];
/// Bigger G-code than this isn't drawn (a few minutes of parsing on a Pi; not worth it).
const MAX_GCODE_BYTES: u64 = 1024 * 1024 * 1024;

/// The views printpit keeps for the job panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Plate,
    Top,
}

impl View {
    pub const ALL: [View; 2] = [View::Plate, View::Top];

    pub fn file_name(self) -> &'static str {
        match self {
            View::Plate => "plate.png",
            View::Top => "top.png",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.file_name() == name)
    }
}

#[derive(Debug, Default)]
pub struct Rendered {
    /// `(width, height, png)` in the order the profile lists them.
    pub thumbnails: Vec<(u32, u32, Vec<u8>)>,
    pub plate: Option<Vec<u8>>,
    pub top: Option<Vec<u8>>,
}

/// What the config block at the end of the file says about how to draw it.
#[derive(Debug, Clone, PartialEq)]
struct Settings {
    thumbnails: Vec<(u32, u32)>,
    bed: Option<Vec<[f32; 2]>>,
    colors: Vec<Rgb>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            thumbnails: DEFAULT_THUMBNAILS.to_vec(),
            bed: None,
            colors: Vec::new(),
        }
    }
}

impl Settings {
    fn apply(&mut self, key: &str, value: &str) {
        match key {
            "thumbnails" => {
                let t = parse_thumbnails(value);
                if !t.is_empty() {
                    self.thumbnails = t;
                }
            }
            "printable_area" => self.bed = parse_bed(value),
            "filament_colour" => self.colors = value.split(';').filter_map(parse_color).collect(),
            _ => {}
        }
    }

    fn color(&self, tool: u8) -> Rgb {
        self.colors
            .get(usize::from(tool))
            .copied()
            .unwrap_or(DEFAULT_COLOR)
    }
}

/// `48x48/PNG,300x300/PNG` (Orca) or `96x96, 300x300` (older PrusaSlicer style). The format is
/// ignored: printpit always writes PNG, which every reader understands.
fn parse_thumbnails(value: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for part in value.split(',') {
        let size = part.split('/').next().unwrap_or("").trim();
        let Some((w, h)) = size.split_once('x') else {
            continue;
        };
        let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) else {
            continue;
        };
        let ok = |n: u32| (8..=MAX_THUMBNAIL_SIDE).contains(&n);
        if ok(w) && ok(h) && !out.contains(&(w, h)) && out.len() < MAX_THUMBNAILS {
            out.push((w, h));
        }
    }
    out
}

/// `0x0,350x0,350x350,0x350`: the bed outline, in printer coordinates.
fn parse_bed(value: &str) -> Option<Vec<[f32; 2]>> {
    let mut pts = Vec::new();
    for p in value.split(',') {
        let (x, y) = p.trim().split_once('x')?;
        let (x, y) = (x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?);
        if !(x.is_finite() && y.is_finite() && x.abs() < 10_000.0 && y.abs() < 10_000.0) {
            return None;
        }
        pts.push([x, y]);
    }
    (pts.len() >= 3 && pts.len() <= 256).then_some(pts)
}

/// `#F2754E` → linear-ish 0..1 RGB, kept away from black so a black filament still shows on a
/// dark page (and white from vanishing into a light one).
fn parse_color(s: &str) -> Option<Rgb> {
    let hex = s.trim().strip_prefix('#')?;
    if hex.len() < 6 || !hex.is_ascii() {
        return None;
    }
    let ch = |i: usize| {
        u8::from_str_radix(&hex[i..i + 2], 16)
            .ok()
            .map(|v| f32::from(v) / 255.0)
    };
    let mut c = [ch(0)?, ch(2)?, ch(4)?];
    let luma = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let (lo, hi) = (0.22, 0.85);
    if luma < lo {
        let t = (lo - luma) / (1.0 - luma);
        c = c.map(|v| v + (1.0 - v) * t);
    } else if luma > hi {
        c = c.map(|v| v * hi / luma);
    }
    Some(c)
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
    min: [f32; 3],
    max: [f32; 3],
}

impl Bounds {
    const EMPTY: Bounds = Bounds {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };

    fn is_empty(&self) -> bool {
        self.min[0] > self.max[0]
    }

    fn add(&mut self, p: [f32; 3]) {
        for (k, v) in p.into_iter().enumerate() {
            self.min[k] = self.min[k].min(v);
            self.max[k] = self.max[k].max(v);
        }
    }

    fn add_segment(&mut self, s: &Segment) {
        let r = s.width / 2.0;
        for p in [s.a, s.b] {
            self.add([p[0] - r, p[1] - r, (s.z - s.height).max(0.0)]);
            self.add([p[0] + r, p[1] + r, s.z]);
        }
    }

    fn corners(&self) -> impl Iterator<Item = [f32; 3]> + '_ {
        (0..8).map(|i| {
            [
                if i & 1 == 0 { self.min[0] } else { self.max[0] },
                if i & 2 == 0 { self.min[1] } else { self.max[1] },
                if i & 4 == 0 { self.min[2] } else { self.max[2] },
            ]
        })
    }
}

/// An orthographic camera: either from the front-left, looking down 30°, or straight down.
#[derive(Debug, Clone, Copy)]
struct Camera {
    top: bool,
    right: [f32; 2],
    fwd: [f32; 2],
    sin: f32,
    cos: f32,
    scale: f32,
    off: [f32; 2],
}

impl Camera {
    fn iso() -> Self {
        // The camera stands in front of the bed, a little to the left, 30° above it.
        let az = (-110.0f32).to_radians();
        let pitch = 30.0f32.to_radians();
        Self {
            top: false,
            right: [-az.sin(), az.cos()],
            fwd: [-az.cos(), -az.sin()],
            sin: pitch.sin(),
            cos: pitch.cos(),
            scale: 1.0,
            off: [0.0; 2],
        }
    }

    fn top() -> Self {
        Self {
            top: true,
            right: [1.0, 0.0],
            fwd: [0.0, 1.0],
            sin: 1.0,
            cos: 0.0,
            scale: 1.0,
            off: [0.0; 2],
        }
    }

    /// (horizontal, up, depth) before scaling.
    fn view(&self, p: [f32; 3]) -> [f32; 3] {
        let h = p[0] * self.right[0] + p[1] * self.right[1];
        let f = p[0] * self.fwd[0] + p[1] * self.fwd[1];
        if self.top {
            [h, f, -p[2]]
        } else {
            [h, p[2] * self.cos + f * self.sin, f * self.cos - p[2] * self.sin]
        }
    }

    fn vertex(&self, p: [f32; 3], u: f32, v: f32) -> Vertex {
        let [h, up, depth] = self.view(p);
        Vertex {
            x: self.off[0] + h * self.scale,
            y: self.off[1] - up * self.scale,
            depth,
            u,
            v,
        }
    }

    /// Scales and centres the camera so `points` fill `w`×`h` less a margin on each side.
    fn fit(mut self, points: impl Iterator<Item = [f32; 3]>, w: u32, h: u32, margin: f32) -> Self {
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        for p in points {
            let v = self.view(p);
            for k in 0..2 {
                lo[k] = lo[k].min(v[k]);
                hi[k] = hi[k].max(v[k]);
            }
        }
        let (w, h) = (w as f32, h as f32);
        let dx = (hi[0] - lo[0]).max(1e-3);
        let dy = (hi[1] - lo[1]).max(1e-3);
        self.scale = (w * (1.0 - 2.0 * margin) / dx).min(h * (1.0 - 2.0 * margin) / dy);
        self.off = [
            w / 2.0 - (lo[0] + hi[0]) / 2.0 * self.scale,
            h / 2.0 + (lo[1] + hi[1]) / 2.0 * self.scale,
        ];
        self
    }

    /// Unit vector towards the light: over the viewer's left shoulder.
    fn light(&self) -> [f32; 3] {
        let to_cam = [-self.fwd[0] * self.cos, -self.fwd[1] * self.cos, self.sin];
        let l = [
            to_cam[0] - self.right[0] * 0.5,
            to_cam[1] - self.right[1] * 0.5,
            to_cam[2] + 0.6,
        ];
        let n = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
        l.map(|v| v / n)
    }
}

/// Rounded bead profile across a line's width (or up its side): darker at the seams. `amp` fades
/// it out for beads only a few samples wide, which would otherwise draw moiré.
fn bead(t: f32, amp: f32) -> f32 {
    1.0 - amp + amp * (std::f32::consts::PI * t.clamp(0.0, 1.0)).sin()
}

/// How much bead shading a bead `samples` wide gets.
fn bead_amp(samples: f32) -> f32 {
    0.22 * ((samples - 2.0) / 4.0).clamp(0.0, 1.0)
}

fn lit(light: [f32; 3], n: [f32; 3]) -> f32 {
    0.38 + 0.62 * (light[0] * n[0] + light[1] * n[1] + light[2] * n[2]).max(0.0)
}

fn scaled(c: Rgb, k: f32) -> Rgb {
    c.map(|v| v * k)
}

/// Supports are drawn paler than the part, so they stand out in the plate view.
fn feature_color(base: Rgb, f: Feature) -> Rgb {
    match f {
        Feature::Support => [0, 1, 2].map(|k| base[k] * 0.35 + 0.78 * 0.65),
        _ => base,
    }
}

struct Target {
    frame: Frame,
    /// Samples per millimetre (output pixels × supersampling).
    density: f32,
    cam: Camera,
    light: [f32; 3],
    /// Height of the tallest bead, for the top view's height shading.
    max_z: f32,
    /// Only the part itself, for thumbnails.
    model_only: bool,
}

impl Target {
    fn new(w: u32, h: u32, ss: u32, cam: Camera, max_z: f32, model_only: bool) -> Self {
        Self {
            frame: Frame::new(w, h, ss),
            density: cam.scale * ss as f32,
            light: cam.light(),
            cam,
            max_z: max_z.max(0.01),
            model_only,
        }
    }

    fn bed(&mut self, bed: &[[f32; 2]]) {
        let cam = self.cam;
        // Grid lines about one output pixel wide, every 10 mm.
        let line = 0.6 / cam.scale;
        let shade = move |x: f32, y: f32| -> Rgb {
            let on = |v: f32| (v - (v / 10.0).round() * 10.0).abs() < line;
            if on(x) || on(y) {
                [0.36, 0.39, 0.36]
            } else {
                [0.22, 0.24, 0.22]
            }
        };
        let z = -0.05;
        let v = |p: [f32; 2]| cam.vertex([p[0], p[1], z], p[0], p[1]);
        for i in 1..bed.len() - 1 {
            self.frame.triangle([v(bed[0]), v(bed[i]), v(bed[i + 1])], &shade);
        }
    }

    fn segment(&mut self, s: &Segment, base: Rgb) {
        if s.feature == Feature::Interior || (self.model_only && s.feature != Feature::Model) {
            return;
        }
        let color = feature_color(base, s.feature);
        let dx = s.b[0] - s.a[0];
        let dy = s.b[1] - s.a[1];
        let len = dx.hypot(dy);
        let d = [dx / len, dy / len];
        let n = [-d[1], d[0]];
        let r = s.width / 2.0;
        let a = [s.a[0] - d[0] * r, s.a[1] - d[1] * r];
        let b = [s.b[0] + d[0] * r, s.b[1] + d[1] * r];
        let off = |p: [f32; 2], k: f32| [p[0] + n[0] * k, p[1] + n[1] * k];
        let cam = self.cam;
        let v = |p: [f32; 2], z: f32, t: f32| cam.vertex([p[0], p[1], z], t, 0.0);

        let top_amp = bead_amp(s.width * self.density * cam.sin);
        let top = if cam.top {
            scaled(color, 0.55 + 0.45 * (s.z / self.max_z).clamp(0.0, 1.0))
        } else {
            scaled(color, lit(self.light, [0.0, 0.0, 1.0]))
        };
        self.frame.quad(
            [
                v(off(a, r), s.z, 0.0),
                v(off(b, r), s.z, 0.0),
                v(off(b, -r), s.z, 1.0),
                v(off(a, -r), s.z, 1.0),
            ],
            &|t, _| scaled(top, bead(t, top_amp)),
        );
        if cam.top {
            return;
        }
        // The side of the bead that faces the camera.
        let facing = -(n[0] * cam.fwd[0] + n[1] * cam.fwd[1]);
        if facing.abs() < 0.02 {
            return;
        }
        let k = if facing > 0.0 { r } else { -r };
        let sn = if facing > 0.0 { n } else { [-n[0], -n[1]] };
        let side = scaled(color, lit(self.light, [sn[0], sn[1], 0.0]));
        let side_amp = bead_amp(s.height * self.density * cam.cos);
        let bottom = s.z - s.height;
        self.frame.quad(
            [
                v(off(a, k), bottom, 0.0),
                v(off(b, k), bottom, 0.0),
                v(off(b, k), s.z, 1.0),
                v(off(a, k), s.z, 1.0),
            ],
            &|t, _| scaled(side, bead(t, side_amp)),
        );
    }
}

struct Survey {
    settings: Settings,
    model: Bounds,
    all: Bounds,
}

fn survey(path: &Path) -> std::io::Result<Survey> {
    let mut s = Survey {
        settings: Settings::default(),
        model: Bounds::EMPTY,
        all: Bounds::EMPTY,
    };
    gcode::walk(
        BufReader::with_capacity(1 << 16, File::open(path)?),
        |e| match e {
            Event::Segment(seg) => {
                s.all.add_segment(&seg);
                if matches!(seg.feature, Feature::Model | Feature::Interior) {
                    s.model.add_segment(&seg);
                }
            }
            Event::Config(k, v) => s.settings.apply(k, v),
        },
    )?;
    Ok(s)
}

/// Draws the thumbnails and the plate pictures for a G-code file. `None` when there's nothing
/// to draw (no extrusion found, or the file is too large to bother).
pub fn render(path: &Path) -> std::io::Result<Option<Rendered>> {
    if std::fs::metadata(path)?.len() > MAX_GCODE_BYTES {
        return Ok(None);
    }
    let Survey { settings, model, all } = survey(path)?;
    if all.is_empty() {
        return Ok(None);
    }
    // A print with no part lines (all support, say) still gets thumbnails of what's there.
    let (thumb_bounds, model_only) = if model.is_empty() {
        (all, false)
    } else {
        (model, true)
    };

    let mut targets: Vec<Target> = settings
        .thumbnails
        .iter()
        .map(|&(w, h)| {
            let ss = if w.max(h) <= 96 { 4 } else { 3 };
            let cam = Camera::iso().fit(thumb_bounds.corners(), w, h, 0.06);
            Target::new(w, h, ss, cam, all.max[2], model_only)
        })
        .collect();

    let bed = settings.bed.clone();
    // The 3D view frames what was printed with a margin of bed around it (the top view shows
    // the whole bed): a small part on a big bed would otherwise be a speck.
    let mut plate_pts: Vec<[f32; 3]> = all.corners().collect();
    let pad = ((all.max[0] - all.min[0]).max(all.max[1] - all.min[1]) * 0.25).max(15.0);
    let mut around = Bounds {
        min: [all.min[0] - pad, all.min[1] - pad, 0.0],
        max: [all.max[0] + pad, all.max[1] + pad, 0.0],
    };
    if let Some(b) = &bed {
        let mut bb = Bounds::EMPTY;
        b.iter().for_each(|p| bb.add([p[0], p[1], 0.0]));
        for k in 0..2 {
            around.min[k] = around.min[k].max(bb.min[k].min(all.min[k]));
            around.max[k] = around.max[k].min(bb.max[k].max(all.max[k]));
        }
    }
    plate_pts.extend(around.corners());
    let (pw, ph) = PLATE_SIZE;
    let plate_cam = Camera::iso().fit(plate_pts.iter().copied(), pw, ph, 0.04);
    let mut plate = Target::new(pw, ph, 2, plate_cam, all.max[2], false);

    let flat = Bounds {
        min: [all.min[0], all.min[1], 0.0],
        max: [all.max[0], all.max[1], 0.0],
    };
    let top_pts: Vec<[f32; 3]> = match &bed {
        Some(b) => b.iter().map(|p| [p[0], p[1], 0.0]).collect(),
        None => flat.corners().collect(),
    };
    let (tw, th) = top_size(&top_pts);
    // The bed fills the picture edge to edge (it has its own border in the page).
    let top_margin = if bed.is_some() { 0.0 } else { 0.03 };
    let top_cam = Camera::top().fit(top_pts.iter().copied(), tw, th, top_margin);
    let mut top = Target::new(tw, th, 2, top_cam, all.max[2], false);

    if let Some(b) = &bed {
        plate.bed(b);
        top.bed(b);
    }
    gcode::walk(BufReader::with_capacity(1 << 16, File::open(path)?), |e| {
        if let Event::Segment(seg) = e {
            let base = settings.color(seg.tool);
            for t in targets.iter_mut() {
                t.segment(&seg, base);
            }
            plate.segment(&seg, base);
            top.segment(&seg, base);
        }
    })?;

    Ok(Some(Rendered {
        thumbnails: targets
            .iter()
            .map(|t| (t.frame.width, t.frame.height, t.frame.png()))
            .collect(),
        plate: Some(plate.frame.png()),
        top: Some(top.frame.png()),
    }))
}

/// The top view keeps the bed's aspect ratio within `TOP_MAX`² (but not thinner than 1:3).
fn top_size(pts: &[[f32; 3]]) -> (u32, u32) {
    let span = |k: usize| {
        let (lo, hi) = pts
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), p| {
                (lo.min(p[k]), hi.max(p[k]))
            });
        (hi - lo).max(1.0)
    };
    let aspect = (span(0) / span(1)).clamp(1.0 / 3.0, 3.0);
    let m = TOP_MAX as f32;
    if aspect >= 1.0 {
        (TOP_MAX, (m / aspect).round() as u32)
    } else {
        ((m * aspect).round() as u32, TOP_MAX)
    }
}

/// Draws a freshly sliced job's pictures: saves `plate.png`/`top.png` next to its G-code and puts
/// thumbnails into the G-code. Returns the G-code's new size if it changed. Never fails the job:
/// problems are logged and the G-code is left as the slicer wrote it.
///
/// The drawing writes only to fresh temporary names; the files are moved into place here, after
/// it finishes. So a cancelled job (this future dropped) never has a stale drawing land on top of
/// a newer slice.
pub async fn decorate(storage: &Storage, job: i64) -> Option<u64> {
    let st = storage.clone();
    let started = std::time::Instant::now();
    let drawn = match tokio::task::spawn_blocking(move || draw_files(&st, job)).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => {
            tracing::warn!(job, error = %e, "couldn't draw slice previews");
            return None;
        }
        Err(e) => {
            tracing::warn!(job, error = %e, "slice preview task failed");
            return None;
        }
    };
    for (tmp, dest) in &drawn.moves {
        if let Err(e) = tokio::fs::rename(tmp, dest).await {
            tracing::warn!(job, error = %e, "couldn't store a slice preview");
            for (tmp, _) in &drawn.moves {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            return None;
        }
    }
    tracing::info!(
        job,
        ms = started.elapsed().as_millis() as u64,
        "drew slice previews"
    );
    drawn.gcode_size
}

#[derive(Default)]
struct Drawn {
    /// (temporary file, where it goes), the G-code last.
    moves: Vec<(std::path::PathBuf, std::path::PathBuf)>,
    gcode_size: Option<u64>,
}

fn draw_files(storage: &Storage, job: i64) -> std::io::Result<Drawn> {
    let gcode = storage.output_path(job);
    let mut drawn = Drawn::default();
    let Some(r) = render(&gcode)? else {
        return Ok(drawn);
    };
    let tmp = |name: &str| {
        let tag = hex::encode(crate::crypto::random_bytes::<6>());
        storage.job_dir(job).join(format!(".{name}.{tag}"))
    };
    let res = (|| {
        for (view, png) in [(View::Plate, &r.plate), (View::Top, &r.top)] {
            if let Some(png) = png {
                let t = tmp(view.file_name());
                drawn.moves.push((t.clone(), storage.preview_path(job, view)));
                storage.create_blocking(&t)?.write_all(png)?;
            }
        }
        if r.thumbnails.is_empty() {
            return Ok(());
        }
        let t = tmp("out.gcode");
        drawn.moves.push((t.clone(), gcode.clone()));
        let mut out = std::io::BufWriter::new(storage.create_blocking(&t)?);
        let injected = inject(&gcode, &mut out, thumbnail_block(&r.thumbnails).as_bytes())?;
        let file = out.into_inner().map_err(|e| e.into_error())?;
        if injected {
            file.sync_all()?;
            drawn.gcode_size = Some(file.metadata()?.len());
        } else {
            drop(file);
            drawn.moves.pop();
            std::fs::remove_file(&t)?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => Ok(drawn),
        Err(e) => {
            for (t, _) in &drawn.moves {
                let _ = std::fs::remove_file(t);
            }
            Err(e)
        }
    }
}

/// The `; THUMBNAIL_BLOCK_START … END` comment block in the format Moonraker, Mainsail and
/// KlipperScreen read (PrusaSlicer's, which Orca also writes): base64 in lines of 78.
pub fn thumbnail_block(thumbs: &[(u32, u32, Vec<u8>)]) -> String {
    let mut out = String::from("; THUMBNAIL_BLOCK_START\n");
    for (w, h, png) in thumbs {
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        out.push_str(&format!(";\n; thumbnail begin {w}x{h} {}\n", b64.len()));
        for chunk in b64.as_bytes().chunks(78) {
            out.push_str("; ");
            out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
            out.push('\n');
        }
        out.push_str("; thumbnail end\n");
    }
    out.push_str(";\n; THUMBNAIL_BLOCK_END\n");
    out
}

/// How far into the file to look for the header (and for thumbnails that are already there).
const HEAD_BYTES: usize = 1024 * 1024;

/// Copies `src` to `dst` with `block` inserted after Orca's `; HEADER_BLOCK_END` line (or at the
/// top). Returns `false`, writing nothing, if the file already has thumbnails: a slicer that
/// writes its own (a later Orca) knows best.
pub fn inject(src: &Path, dst: &mut impl Write, block: &[u8]) -> std::io::Result<bool> {
    let mut f = File::open(src)?;
    let mut head = Vec::with_capacity(HEAD_BYTES);
    (&mut f).take(HEAD_BYTES as u64).read_to_end(&mut head)?;
    if find(&head, b"; thumbnail begin").is_some() || find(&head, b"; thumbnail_").is_some() {
        return Ok(false);
    }
    const MARK: &[u8] = b"; HEADER_BLOCK_END\n";
    let at = find(&head[..head.len().min(64 * 1024)], MARK).map_or(0, |i| i + MARK.len());
    dst.write_all(&head[..at])?;
    if at > 0 {
        dst.write_all(b"\n")?;
    }
    dst.write_all(block)?;
    dst.write_all(&head[at..])?;
    std::io::copy(&mut f, dst)?;
    Ok(true)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use raster::tests::decode_png;

    /// A 10 mm hollow square tower, 2 mm tall, on a 100 mm bed, plus a support line.
    pub(crate) fn tower() -> String {
        let mut g = String::from(
            "; HEADER_BLOCK_START\n; generated by OrcaSlicer 2.4.2\n; HEADER_BLOCK_END\n\nM83\nG1 X0 Y0 E5\n",
        );
        for layer in 1..=10 {
            let z = layer as f32 * 0.2;
            g.push_str(&format!(
                ";LAYER_CHANGE\n;Z:{z}\n;HEIGHT:0.2\nG1 Z{z}\n;TYPE:Outer wall\n;WIDTH:0.45\n"
            ));
            g.push_str("G0 X45 Y45\nG1 X55 Y45 E.5\nG1 X55 Y55 E.5\nG1 X45 Y55 E.5\nG1 X45 Y45 E.5\n");
            if layer == 1 {
                g.push_str(";TYPE:Support\nG0 X20 Y20\nG1 X30 Y20 E.5\n");
            }
        }
        g.push_str(
            "; CONFIG_BLOCK_START\n; thumbnails = 32x32/PNG, 120x80/QOI\n; printable_area = 0x0,100x0,100x100,0x100\n\
             ; filament_colour = #00AE42\n; CONFIG_BLOCK_END\n",
        );
        g
    }

    fn write(dir: &Path, text: &str) -> std::path::PathBuf {
        let p = dir.join("out.gcode");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn settings() {
        assert_eq!(parse_thumbnails("48x48/PNG,300x300/PNG"), [(48, 48), (300, 300)]);
        assert_eq!(
            parse_thumbnails("96x96, 300x300, 96x96, 2x2, 5000x10, junk"),
            [(96, 96), (300, 300)]
        );
        assert!(parse_thumbnails("").is_empty());
        assert_eq!(parse_bed("0x0,350x0,350x350,0x350").unwrap().len(), 4);
        assert_eq!(parse_bed("0x0,1x1"), None);
        assert_eq!(parse_bed("0x0,1x1,ax2"), None);
        assert_eq!(parse_color("#3366cc"), Some([0.2, 0.4, 0.8]));
        let black = parse_color("#000000").unwrap();
        assert!(black[0] > 0.2, "black is lifted: {black:?}");
        let white = parse_color("#FFFFFF").unwrap();
        assert!(white[0] < 0.9, "white is dimmed: {white:?}");
        assert_eq!(parse_color("red"), None);
        assert_eq!(parse_color("#12"), None);
        assert_eq!(parse_color("#ééé"), None);
    }

    #[test]
    fn renders_thumbnails_and_views() {
        let dir = tempfile::tempdir().unwrap();
        let r = render(&write(dir.path(), &tower())).unwrap().unwrap();
        let sizes: Vec<_> = r.thumbnails.iter().map(|(w, h, _)| (*w, *h)).collect();
        assert_eq!(sizes, [(32, 32), (120, 80)]);
        for (w, h, png) in &r.thumbnails {
            let (pw, ph, px) = decode_png(png);
            assert_eq!((pw, ph), (*w, *h));
            // Transparent corners, and a green part filling a good share of the picture.
            assert_eq!(px[3], 0, "corner is transparent");
            let drawn: Vec<&[u8]> = px.chunks(4).filter(|p| p[3] == 255).collect();
            assert!(drawn.len() as u32 > pw * ph / 8, "{} of {}", drawn.len(), pw * ph);
            assert!(drawn.iter().all(|p| p[1] > p[0] && p[1] > p[2]), "all green");
        }
        let (w, h, plate) = decode_png(r.plate.as_ref().unwrap());
        assert_eq!((w, h), PLATE_SIZE);
        // The bed fills a good part of the plate view; it's grey.
        let opaque = plate.chunks(4).filter(|p| p[3] == 255).count();
        assert!(opaque > (w * h / 4) as usize, "{opaque}");
        let (w, h, top) = decode_png(r.top.as_ref().unwrap());
        assert_eq!((w, h), (TOP_MAX, TOP_MAX));
        // Straight down on a square bed filling the view: the centre is the part, the support
        // at (25, 20) is paler than the part.
        let at = |x: f32, y: f32| {
            let px = x / 100.0 * w as f32;
            let py = (1.0 - y / 100.0) * h as f32;
            let i = ((py as u32 * w + px as u32) * 4) as usize;
            [top[i], top[i + 1], top[i + 2], top[i + 3]]
        };
        let wall = at(45.0, 50.0);
        let support = at(25.0, 20.0);
        let bed = at(83.0, 83.0);
        assert!(wall[1] > 100 && wall[0] < 60, "wall {wall:?}");
        assert!(support[0] > wall[0] + 60, "support {support:?} vs wall {wall:?}");
        assert!(bed[0] < 110 && bed[0] == bed[2], "bed {bed:?}");
    }

    #[test]
    fn nothing_to_draw() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            render(&write(dir.path(), "G28\nM83\nG1 X1 E1\n"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn support_only_print_still_gets_thumbnails() {
        let dir = tempfile::tempdir().unwrap();
        let r = render(&write(dir.path(), ";TYPE:Support\nM83\nG1 X10 E1\nG1 Y10 E1\n"))
            .unwrap()
            .unwrap();
        assert_eq!(r.thumbnails.len(), DEFAULT_THUMBNAILS.len());
        let (_, _, px) = decode_png(&r.thumbnails[0].2);
        assert!(px.chunks(4).any(|p| p[3] > 0));
    }

    /// Moonraker's own reading of a thumbnail block (metadata.py, PrusaSlicer parser), redone
    /// here so the format can't drift from what it accepts.
    fn moonraker_thumbnails(header: &str) -> Vec<(u32, u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut rest = header;
        while let Some(start) = rest.find("; thumbnail begin") {
            let after = &rest[start + "; thumbnail begin".len() + 1..];
            let end = after.find("; thumbnail end").expect("end");
            let body = after[..end].replace("; ", "");
            let lines: Vec<&str> = body.split('\n').collect();
            let info: Vec<u32> = lines[0]
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .map(|s| s.parse().unwrap())
                .collect();
            assert_eq!(info.len(), 3, "{:?}", lines[0]);
            let data: String = lines[1..lines.len() - 1].concat();
            assert_eq!(data.len() as u32, info[2], "length matches");
            let png = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
            out.push((info[0], info[1], png));
            rest = &after[end..];
        }
        out
    }

    #[test]
    fn injects_after_the_header_in_moonrakers_format() {
        let dir = tempfile::tempdir().unwrap();
        let src = write(dir.path(), &tower());
        let r = render(&src).unwrap().unwrap();
        let block = thumbnail_block(&r.thumbnails);
        let mut out = Vec::new();
        assert!(inject(&src, &mut out, block.as_bytes()).unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("; HEADER_BLOCK_START\n; generated by OrcaSlicer 2.4.2\n; HEADER_BLOCK_END\n\n; THUMBNAIL_BLOCK_START\n"));
        assert!(
            text.ends_with(&tower()[tower().find("\nM83").unwrap()..]),
            "the rest is unchanged"
        );
        let found = moonraker_thumbnails(&text);
        assert_eq!(found, r.thumbnails);
        for line in block.lines().filter(|l| !l.is_empty()) {
            assert!(line.starts_with(';') && line.len() <= 80, "{line}");
        }

        // Already has thumbnails: left alone.
        let again = write(dir.path(), &text);
        let mut out = Vec::new();
        assert!(!inject(&again, &mut out, block.as_bytes()).unwrap());
        assert!(out.is_empty());

        // No header: the block goes first.
        let bare = write(dir.path(), "G28\n");
        let mut out = Vec::new();
        assert!(inject(&bare, &mut out, b"; X\n").unwrap());
        assert_eq!(out, b"; X\nG28\n");
    }
}
