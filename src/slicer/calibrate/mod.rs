//! Calibration prints (PLAN §15.13): the tests OrcaSlicer's GUI offers, made with the CLI the
//! worker runs. The walkthrough's core tests (temperature tower, flow rate in two passes,
//! pressure advance tower, retraction test, max volumetric speed) and optional ones (pressure
//! advance lines, tolerance, cornering, input shaping frequency and damping, VFA).
//!
//! The CLI has no calibration mode: Orca's GUI loads a model, changes some settings and then
//! changes values per layer while it writes the G-code. printpit does the same in three steps:
//!
//! 1. [`Calibration::model`] draws the test piece ([`mesh`], an STL, numbered where that helps).
//!    It's an ordinary slice job's model, so the queue, the job panel, "slice again" and the
//!    upload all work as for any other job.
//! 2. [`Calibration::prepare`] changes the job's copies of the profiles as Orca's GUI does (walls,
//!    infill, brim, layer height, …), just before the queue sends them to the worker.
//! 3. [`Calibration::plan`] says how the G-code is then changed ([`gcode`]): a temperature or
//!    pressure advance per band, retraction lengths per band, flow per numbered block, or feed
//!    rates for a volumetric speed per band.
//!
//! What the user reads off the print becomes [`Change`]s to the filament, machine or process
//! profile ([`Calibration::changes`]): fixed keys ([`RESULT_KEYS`]), values computed here, never
//! user text.

pub mod gcode;
pub mod mesh;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use self::gcode::{Command, Plan};
use self::mesh::{Glyph, Mesh};
use super::Profile;

/// The tests, in the order the walkthrough takes them (Orca's wiki order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Test {
    Temperature,
    FlowCoarse,
    FlowFine,
    PressureAdvance,
    Retraction,
    VolumetricSpeed,
    PressureAdvanceLine,
    Tolerance,
    Cornering,
    ShaperFrequency,
    ShaperDamping,
    Vfa,
}

impl Test {
    /// Core tests first, then the optional ones.
    pub const ALL: [Test; 12] = [
        Test::Temperature,
        Test::FlowCoarse,
        Test::FlowFine,
        Test::PressureAdvance,
        Test::Retraction,
        Test::VolumetricSpeed,
        Test::PressureAdvanceLine,
        Test::Tolerance,
        Test::Cornering,
        Test::ShaperFrequency,
        Test::ShaperDamping,
        Test::Vfa,
    ];

    /// Not part of the walkthrough's main path (alternatives, or printer tuning that's usually
    /// done once in Klipper's config).
    pub fn optional(self) -> bool {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0) >= 6
    }

    /// How many values a result has (input shaping: one per axis).
    pub fn axes(self) -> usize {
        if self == Test::ShaperFrequency { 2 } else { 1 }
    }

    /// The profile a result changes: `filament`, `machine` or `process`.
    pub fn result_kind(self) -> &'static str {
        match self {
            Test::Cornering | Test::ShaperFrequency | Test::ShaperDamping => "machine",
            Test::Tolerance | Test::Vfa => "process",
            _ => "filament",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Test::Temperature => "temperature",
            Test::FlowCoarse => "flow_coarse",
            Test::FlowFine => "flow_fine",
            Test::PressureAdvance => "pressure_advance",
            Test::Retraction => "retraction",
            Test::VolumetricSpeed => "volumetric_speed",
            Test::PressureAdvanceLine => "pressure_advance_line",
            Test::Tolerance => "tolerance",
            Test::Cornering => "cornering",
            Test::ShaperFrequency => "input_shaping_frequency",
            Test::ShaperDamping => "input_shaping_damping",
            Test::Vfa => "vfa",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.slug() == s)
    }

    pub fn title(self) -> &'static str {
        match self {
            Test::Temperature => "temperature tower",
            Test::FlowCoarse => "flow rate · coarse",
            Test::FlowFine => "flow rate · fine",
            Test::PressureAdvance => "pressure advance tower",
            Test::Retraction => "retraction test",
            Test::VolumetricSpeed => "max volumetric speed",
            Test::PressureAdvanceLine => "pressure advance lines",
            Test::Tolerance => "tolerance",
            Test::Cornering => "cornering (square corner velocity)",
            Test::ShaperFrequency => "input shaping · frequency",
            Test::ShaperDamping => "input shaping · damping",
            Test::Vfa => "VFA (vertical fine artifacts)",
        }
    }

    /// What it finds, one sentence.
    pub fn purpose(self) -> &'static str {
        match self {
            Test::Temperature => {
                "Finds the nozzle temperature this filament prints best at. Each 10 mm block of the tower prints 5 °C (or your step) cooler than the one below it, and is numbered with its temperature."
            }
            Test::FlowCoarse => {
                "Finds the flow ratio: how much plastic comes out compared with what the slicer asks for. Nine blocks print with −20 % to +20 % flow; each is numbered with its change."
            }
            Test::FlowFine => {
                "Fine-tunes the flow ratio from the coarse pass: ten blocks from −9 % to 0 % of the new value."
            }
            Test::PressureAdvance => {
                "Finds the pressure advance value: how far ahead the extruder pushes to keep corners sharp. The tower's walls print with a higher value every millimetre (a tick on the front marks every 5 mm)."
            }
            Test::Retraction => {
                "Finds the shortest retraction that stops stringing. Two towers print with a longer retraction every millimetre (the bands alternate in width so they're easy to count)."
            }
            Test::VolumetricSpeed => {
                "Finds how fast the hot end can melt this filament. A single-wall vase prints faster every millimetre, until the hot end can't keep up."
            }
            Test::PressureAdvanceLine => {
                "Another way to find pressure advance: one layer of numbered lines, each with its own value, printed slow, fast, then slow again. Quicker than the tower, and easier to read for some."
            }
            Test::Tolerance => {
                "Finds how much bigger holes should be: a plate with 8 mm square holes, each 0.05 mm larger than the last (numbered in hundredths), and an 8 mm peg to try in them."
            }
            Test::Cornering => {
                "Finds Klipper's square corner velocity: how fast the head may take a corner. A single-wall vase prints with a higher value every 2 mm, at the printer's top speed and acceleration."
            }
            Test::ShaperFrequency => {
                "Finds the input shaper frequency for each axis (if you have no accelerometer). A single-wall vase prints at full speed with a higher frequency every millimetre. Needs an [input_shaper] section in Klipper's config."
            }
            Test::ShaperDamping => {
                "Finds the input shaper damping ratio, after the frequency: the same tower, with the frequencies fixed and a higher damping every 2 mm. Needs an [input_shaper] section in Klipper's config."
            }
            Test::Vfa => {
                "Finds outer wall speeds that leave fine vertical ripples (VFA, from motor and belt resonance). An eight-sided single-wall vase prints faster every 5 mm."
            }
        }
    }

    /// How to read the print.
    pub fn reading(self) -> &'static str {
        match self {
            Test::Temperature => {
                "Pick the block with the cleanest overhangs and bridge, the least stringing and still good layer bonding. Hotter is stronger, cooler is neater."
            }
            Test::FlowCoarse | Test::FlowFine => {
                "Run a finger over the tops. Pick the smoothest block: no gaps between lines (too little) and no ridges or a rough, shiny lip (too much)."
            }
            Test::PressureAdvance => {
                "Look at the corners from the bottom up. Find the height where they're sharpest: below it they bulge, above it they're rounded or have gaps after the corner. Measure from the bed (the ticks are every 5 mm)."
            }
            Test::Retraction => {
                "Find the lowest band where the strings between the towers stop. Count the bands from the bottom (the first band is just above the thin base)."
            }
            Test::VolumetricSpeed => {
                "Find the height where the wall starts to look rough, thin or gappy, measured from the bed. A little below that is a safe value."
            }
            Test::PressureAdvanceLine => {
                "Pick the line that's the same width all along: too low bulges where it speeds up and thins where it slows down, too high the opposite (or gaps)."
            }
            Test::Tolerance => {
                "Find the smallest hole the peg slides into without forcing. Holes are numbered by how much larger they are, in hundredths of a millimetre."
            }
            Test::Cornering => {
                "Look at the walls just after the corners and the ribs. Pick the highest band before ripples (ringing) or rounded corners appear, measured from the bed."
            }
            Test::ShaperFrequency => {
                "Ringing after the ribs shows on two faces: the one with a rib on the front (−Y face) shows the X axis, the one on the right (+X face) the Y axis. For each, pick the height where the ripples are faintest."
            }
            Test::ShaperDamping => {
                "Pick the height where the ripples after the ribs are faintest, on both faces."
            }
            Test::Vfa => {
                "Pick the band with the smoothest flat faces (fewest fine vertical lines). Measure from the bed; bands are 5 mm."
            }
        }
    }

    /// Whether the user picks a range (the flow passes and tolerance are fixed).
    pub fn has_range(self) -> bool {
        !matches!(self, Test::FlowCoarse | Test::FlowFine | Test::Tolerance)
    }

    pub fn unit(self) -> &'static str {
        match self {
            Test::Temperature => "°C",
            Test::FlowCoarse | Test::FlowFine => "%",
            Test::PressureAdvance | Test::PressureAdvanceLine | Test::ShaperDamping => "",
            Test::Retraction | Test::Tolerance => "mm",
            Test::VolumetricSpeed => "mm³/s",
            Test::Cornering | Test::Vfa => "mm/s",
            Test::ShaperFrequency => "Hz",
        }
    }

    /// Decimals the range inputs and values use.
    pub fn places(self) -> usize {
        match self {
            Test::Temperature | Test::FlowCoarse | Test::FlowFine | Test::Vfa => 0,
            Test::PressureAdvance | Test::PressureAdvanceLine => 4,
            Test::Retraction | Test::Tolerance => 2,
            Test::VolumetricSpeed | Test::Cornering | Test::ShaperFrequency => 1,
            Test::ShaperDamping => 3,
        }
    }

    /// Where a result goes, for the page: the setting in [`Test::result_kind`]'s profile.
    pub fn setting(self) -> &'static str {
        match self {
            Test::Temperature => "nozzle temperature",
            Test::FlowCoarse | Test::FlowFine => "flow ratio",
            Test::PressureAdvance | Test::PressureAdvanceLine => "pressure advance",
            Test::Retraction => "retraction length (filament override)",
            Test::VolumetricSpeed => "max volumetric speed",
            Test::Tolerance => "X-Y hole compensation",
            Test::Cornering => "max jerk X and Y (Klipper's square corner velocity)",
            Test::ShaperFrequency => "input shaping frequency X and Y",
            Test::ShaperDamping => "input shaping damping X and Y",
            Test::Vfa => "outer wall speed",
        }
    }

    /// Height of one band (mm).
    fn band(self) -> f64 {
        match self {
            Test::Temperature => TEMP_BLOCK,
            Test::Cornering | Test::ShaperDamping => 2.0,
            Test::Vfa => 5.0,
            _ => 1.0,
        }
    }

    fn base(self) -> f64 {
        match self {
            Test::Retraction => RETRACT_BASE,
            _ => 0.0,
        }
    }
}

const TEMP_BLOCK: f64 = 10.0;
const RETRACT_BASE: f64 = 0.4;
const FLOW_COARSE: [i32; 9] = [-20, -15, -10, -5, 0, 5, 10, 15, 20];
const FLOW_FINE: [i32; 10] = [-9, -8, -7, -6, -5, -4, -3, -2, -1, 0];
/// Tolerance holes' extra size (mm), and their nominal size.
const TOLERANCES: [f64; 7] = [0.0, 0.05, 0.1, 0.15, 0.2, 0.25, 0.3];
const HOLE: f64 = 8.0;
/// The pressure advance lines: length, pitch, and where the fast middle part starts and ends.
const PA_LINE: f64 = 80.0;
const PA_PITCH: f64 = 4.0;
const PA_FAST: (f64, f64) = (20.0, 60.0);

/// One setting a result changes. Only keys in [`RESULT_KEYS`] are ever written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// `filament`, `machine` or `process` (results stored before 2026-11-28 are all filament).
    #[serde(default = "filament_kind")]
    pub kind: String,
    pub key: String,
    pub value: String,
}

fn filament_kind() -> String {
    "filament".into()
}

/// The settings results can change: `(profile kind, key, Orca keeps it as a list)`.
pub const RESULT_KEYS: [(&str, &str, bool); 15] = [
    ("filament", "nozzle_temperature", true),
    ("filament", "nozzle_temperature_initial_layer", true),
    ("filament", "filament_flow_ratio", true),
    ("filament", "pressure_advance", true),
    ("filament", "enable_pressure_advance", true),
    ("filament", "filament_retraction_length", true),
    ("filament", "filament_max_volumetric_speed", true),
    ("machine", "machine_max_jerk_x", true),
    ("machine", "machine_max_jerk_y", true),
    ("machine", "input_shaping_freq_x", false),
    ("machine", "input_shaping_freq_y", false),
    ("machine", "input_shaping_damp_x", false),
    ("machine", "input_shaping_damp_y", false),
    ("process", "outer_wall_speed", true),
    ("process", "xy_hole_compensation", false),
];

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} = {}", self.key, self.value)
    }
}

/// Writes the `kind` profile's share of `changes` into it (each per-extruder list gets the value
/// everywhere). Keys outside [`RESULT_KEYS`] are ignored.
pub fn apply_changes(profile: &mut Profile, kind: &str, changes: &[Change]) {
    for c in changes.iter().filter(|c| c.kind == kind) {
        if let Some((_, key, list)) = RESULT_KEYS.iter().find(|(k, key, _)| *k == kind && *key == c.key) {
            set(profile, key, &c.value, *list);
        }
    }
}

/// The three profiles a calibration works with.
#[derive(Debug, Clone, Default)]
pub struct Profiles {
    pub machine: Profile,
    pub process: Profile,
    pub filament: Profile,
}

impl Profiles {
    /// Applies results (each to its own profile).
    pub fn apply(&mut self, changes: &[Change]) {
        apply_changes(&mut self.machine, "machine", changes);
        apply_changes(&mut self.process, "process", changes);
        apply_changes(&mut self.filament, "filament", changes);
    }
}

/// What to add to Klipper's config for a result that's really the printer's, or `None`.
pub fn klipper_note(test: Test, changes: &[Change]) -> Option<String> {
    let get = |k: &str| changes.iter().find(|c| c.key == k).map(|c| c.value.clone());
    match test {
        Test::Cornering => Some(format!(
            "[printer]\nsquare_corner_velocity: {}",
            get("machine_max_jerk_x")?
        )),
        Test::ShaperFrequency => Some(format!(
            "[input_shaper]\nshaper_freq_x: {}\nshaper_freq_y: {}",
            get("input_shaping_freq_x")?,
            get("input_shaping_freq_y")?
        )),
        Test::ShaperDamping => Some(format!(
            "[input_shaper]\ndamping_ratio_x: {}\ndamping_ratio_y: {}",
            get("input_shaping_damp_x")?,
            get("input_shaping_damp_y")?
        )),
        Test::PressureAdvance | Test::PressureAdvanceLine => Some(format!(
            "[extruder]\npressure_advance: {}",
            get("pressure_advance")?
        )),
        _ => None,
    }
}

/// A calibration job: the test, its range, and what earlier steps found that isn't in the
/// profiles yet (applied first, so each test builds on the last).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    pub test: Test,
    #[serde(default)]
    pub start: f64,
    #[serde(default)]
    pub end: f64,
    #[serde(default)]
    pub step: f64,
    #[serde(default)]
    pub carry: Vec<Change>,
}

/// The range a test starts with, from the profiles: `(start, end, step)`.
pub fn defaults(test: Test, machine: &Profile, filament: &Profile) -> (f64, f64, f64) {
    let bowden = text(machine, "extruder_type").eq_ignore_ascii_case("bowden");
    match test {
        Test::Temperature => {
            // Orca's dialog's ranges by filament type, else around the profile's temperature.
            let ty = text(filament, "filament_type").to_ascii_uppercase();
            let (hi, lo) = match ty.as_str() {
                "PLA" | "PLA-CF" | "PVA" => (230.0, 190.0),
                "PETG" | "PET" | "PCTG" => (250.0, 230.0),
                "ABS" | "ASA" => (270.0, 230.0),
                "TPU" => (240.0, 210.0),
                "PA" | "PA-CF" | "PET-CF" | "PC" => (300.0, 260.0),
                _ => {
                    let t = (number(filament, "nozzle_temperature").unwrap_or(210.0) / 5.0).round() * 5.0;
                    (t + 20.0, t - 20.0)
                }
            };
            (hi, lo, 5.0)
        }
        Test::FlowCoarse | Test::FlowFine => (0.0, 0.0, 0.0),
        Test::PressureAdvance if bowden => (0.0, 1.0, 0.02),
        Test::PressureAdvance => (0.0, 0.1, 0.002),
        Test::Retraction if bowden => (1.0, 6.0, 0.25),
        Test::Retraction => (0.0, 2.0, 0.1),
        Test::VolumetricSpeed => (5.0, 20.0, 0.5),
        Test::PressureAdvanceLine if bowden => (0.0, 0.8, 0.05),
        Test::PressureAdvanceLine => (0.0, 0.08, 0.005),
        Test::Tolerance => (0.0, 0.0, 0.0),
        Test::Cornering => (1.0, 20.0, 1.0),
        Test::ShaperFrequency => (20.0, 80.0, 1.0),
        Test::ShaperDamping => (0.02, 0.3, 0.01),
        Test::Vfa => (40.0, 200.0, 10.0),
    }
}

impl Calibration {
    /// Checks a range for `test` against what can be printed on `machine`.
    pub fn new(test: Test, start: f64, end: f64, step: f64, machine: &Profile) -> Result<Self, String> {
        let mut c = Calibration {
            test,
            start,
            end,
            step,
            carry: Vec::new(),
        };
        if !test.has_range() {
            (c.start, c.end, c.step) = (0.0, 0.0, 0.0);
            return Ok(c);
        }
        if ![start, end, step].iter().all(|v| v.is_finite()) || step <= 0.0 {
            return Err("Start, end and step must be numbers, and the step more than 0.".into());
        }
        let (lo, hi, min_step, max_bands) = match test {
            Test::Temperature => (150.0, 350.0, 1.0, 15),
            Test::PressureAdvance => (0.0, 2.0, 0.0005, 100),
            Test::PressureAdvanceLine => (0.0, 2.0, 0.0005, 30),
            Test::Retraction => (0.0, 10.0, 0.05, 60),
            Test::Cornering => (0.5, 50.0, 0.1, 60),
            Test::ShaperFrequency => (5.0, 200.0, 0.5, 150),
            Test::ShaperDamping => (0.0, 0.9, 0.005, 60),
            Test::Vfa => (10.0, 1000.0, 1.0, 40),
            _ => (0.5, 100.0, 0.1, 120),
        };
        let u = test.unit();
        if !(lo..=hi).contains(&start) || !(lo..=hi).contains(&end) {
            return Err(format!(
                "Start and end must be {}–{} {u}.",
                gcode::fmt(lo, 4),
                gcode::fmt(hi, 4)
            ));
        }
        if step < min_step {
            return Err(format!(
                "The step must be at least {} {u}.",
                gcode::fmt(min_step, 4)
            ));
        }
        // The tower starts hot and cools down; the others count up.
        let span = if test == Test::Temperature {
            start - end
        } else {
            end - start
        };
        if span <= 0.0 {
            return Err(if test == Test::Temperature {
                "The tower starts at the hottest temperature: start must be above end.".into()
            } else {
                "End must be above start.".into()
            });
        }
        let n = span / step;
        if (n - n.round()).abs() > 1e-6 {
            return Err("The range must be a whole number of steps.".into());
        }
        let bands = n.round() as usize + 1;
        if bands > max_bands {
            return Err(format!(
                "That's {bands} steps; the most is {max_bands}. Use a bigger step or a smaller range."
            ));
        }
        if test == Test::PressureAdvanceLine {
            let (lines, deep) = (PA_LINE + 20.0, bands as f64 * PA_PITCH + 10.0);
            if let Some((w, d)) = bed_size(machine)
                && (lines > w || deep > d)
            {
                return Err(format!(
                    "{bands} lines need {} × {} mm of bed; this one is {} × {} mm.",
                    gcode::fmt(lines, 0),
                    gcode::fmt(deep, 0),
                    gcode::fmt(w, 0),
                    gcode::fmt(d, 0)
                ));
            }
            c.start = start;
            return Ok(c);
        }
        let height = c.height_for(bands);
        if let Some(max) = number(machine, "printable_height").filter(|h| *h > 0.0)
            && height > max
        {
            return Err(format!(
                "That tower would be {} mm tall; this printer prints up to {} mm.",
                gcode::fmt(height, 1),
                gcode::fmt(max, 1)
            ));
        }
        c.start = start;
        Ok(c)
    }

    fn height_for(&self, bands: usize) -> f64 {
        self.test.base() + bands as f64 * self.test.band()
    }

    /// The value of each band, from the bottom (empty for the flow passes).
    pub fn values(&self) -> Vec<f64> {
        if !self.test.has_range() {
            return Vec::new();
        }
        let span = (self.end - self.start).abs();
        let n = (span / self.step).round() as usize + 1;
        let sign = if self.test == Test::Temperature { -1.0 } else { 1.0 };
        (0..n)
            .map(|i| round(self.start + sign * i as f64 * self.step, 6))
            .collect()
    }

    /// The flow passes' changes, in %, in block order.
    fn modifiers(&self) -> &'static [i32] {
        match self.test {
            Test::FlowCoarse => &FLOW_COARSE,
            Test::FlowFine => &FLOW_FINE,
            _ => &[],
        }
    }

    /// The G-code's file name stem.
    pub fn file_stem(&self) -> String {
        if self.test.has_range() {
            let p = self.test.places();
            format!(
                "calib_{}_{}-{}",
                self.test.slug(),
                gcode::fmt(self.start, p),
                gcode::fmt(self.end, p)
            )
        } else {
            format!("calib_{}", self.test.slug())
        }
    }

    /// A short label for the job panel.
    pub fn summary(&self) -> String {
        if self.test.has_range() {
            let p = self.test.places();
            format!(
                "calibration: {} {}→{} step {} {}",
                self.test.title(),
                gcode::fmt(self.start, p),
                gcode::fmt(self.end, p),
                gcode::fmt(self.step, p),
                self.test.unit()
            )
            .trim_end()
            .to_string()
        } else {
            format!("calibration: {}", self.test.title())
        }
    }

    // ── 1. the model ──────────────────────────────────────────────────────────────────────

    /// The test piece as STL bytes.
    pub fn model(&self, machine: &Profile, process: &Profile) -> Vec<u8> {
        let mesh = match self.test {
            Test::Temperature => self.temperature_tower(),
            Test::FlowCoarse | Test::FlowFine => {
                let (first, layer) = flow_layers(machine, process);
                flow_blocks(self.modifiers(), first, layer).0
            }
            Test::PressureAdvance => self.pa_tower(),
            Test::Retraction => self.retraction_towers(),
            Test::VolumetricSpeed => self.speed_vase(machine),
            Test::PressureAdvanceLine => self.pa_lines(machine, process).0,
            Test::Tolerance => tolerance_plate(),
            Test::Cornering | Test::ShaperFrequency | Test::ShaperDamping => self.ringing_tower(),
            Test::Vfa => self.vfa_tower(),
        };
        mesh.to_stl(&format!("printpit calibration: {}", self.test.title()))
    }

    fn temperature_tower(&self) -> Mesh {
        let mut m = Mesh::new();
        let glyph = Glyph {
            width: 3.2,
            height: 6.0,
            stroke: 1.0,
            gap: 0.5,
            sink: 0.3,
        };
        let (d, h) = (12.0, TEMP_BLOCK);
        for (i, t) in self.values().iter().enumerate() {
            let z = i as f64 * h;
            // Two pillars and a 12 mm bridge over a window, a 45° overhang on the left and a
            // steeper one on the right.
            m.cuboid([0.0, 0.0, z], [12.0, d, z + h]);
            m.cuboid([24.0, 0.0, z], [30.0, d, z + h]);
            m.cuboid([12.0, 0.0, z + h - 2.4], [24.0, d, z + h]);
            m.prism_y(&[(0.0, z + h - 5.0), (0.0, z + h), (-5.0, z + h)], 0.0, d);
            m.prism_y(&[(30.0, z + h - 3.0), (30.0, z + h), (35.2, z + h)], 0.0, d);
            let label = gcode::fmt(*t, 0);
            let x = (12.0 - glyph.measure(&label)) / 2.0;
            m.text_front(&label, x, z + 2.0, 0.0, glyph, 0.8);
        }
        m
    }

    fn pa_tower(&self) -> Mesh {
        let mut m = Mesh::new();
        let height = self.height_for(self.values().len());
        m.cuboid([0.0, 0.0, 0.0], [40.0, 40.0, height]);
        // A tick on the front every 5 mm, away from the corners.
        let mut k = 5.0;
        while k < height - 0.6 {
            m.cuboid([19.0, -0.8, k], [21.0, 0.3, k + 0.6]);
            k += 5.0;
        }
        m
    }

    fn retraction_towers(&self) -> Mesh {
        let mut m = Mesh::new();
        let n = self.values().len();
        m.cuboid([0.0, 0.0, 0.0], [50.0, 10.0, RETRACT_BASE]);
        for x in [0.0, 40.0] {
            for i in 0..n {
                let z = RETRACT_BASE + i as f64;
                // Every other band is 0.4 mm narrower all round, so the bands can be counted.
                let inset = if i % 2 == 1 { 0.4 } else { 0.0 };
                m.cuboid(
                    [x + inset, inset, z - 0.01],
                    [x + 10.0 - inset, 10.0 - inset, z + 1.0],
                );
            }
        }
        m
    }

    fn speed_vase(&self, machine: &Profile) -> Mesh {
        let mut m = Mesh::new();
        let width = bed_size(machine).map_or(100.0, |(w, _)| (w - 40.0).clamp(30.0, 100.0));
        m.cuboid(
            [0.0, 0.0, 0.0],
            [width, 20.0, self.height_for(self.values().len())],
        );
        m
    }

    /// One layer of bars (two first-layer lines wide), numbered on the left every other one.
    /// Returns the mesh and each bar's cell.
    fn pa_lines(&self, machine: &Profile, process: &Profile) -> (Mesh, Vec<[f64; 4]>) {
        let h = first_layer(process);
        let bar = 2.0 * pa_line_width(machine);
        let glyph = Glyph {
            width: 2.6,
            height: 4.5,
            stroke: 0.8,
            gap: 0.5,
            sink: 0.0,
        };
        let mut m = Mesh::new();
        let mut cells = Vec::new();
        for (i, v) in self.values().iter().enumerate() {
            let y = i as f64 * PA_PITCH;
            m.cuboid([0.0, y, 0.0], [PA_LINE, y + bar, h]);
            cells.push([0.0, y, PA_LINE, y + bar]);
            if i % 2 == 0 {
                let label = gcode::fmt(*v, 4);
                let x = -2.5 - glyph.measure(&label);
                m.text_top(&label, x, y + bar / 2.0 - glyph.height / 2.0, 0.0, glyph, h);
            }
        }
        (m, cells)
    }

    /// A 50 mm square single-wall tower with a rib on the front (−y) and the right (+x) face, so
    /// each axis's ringing shows behind a sharp turn.
    fn ringing_tower(&self) -> Mesh {
        let mut m = Mesh::new();
        let h = self.height_for(self.values().len());
        m.cuboid([0.0, 0.0, 0.0], [50.0, 50.0, h]);
        m.cuboid([12.0, -2.0, 0.0], [14.0, 0.3, h]);
        m.cuboid([49.7, 12.0, 0.0], [52.0, 14.0, h]);
        m
    }

    /// An eight-sided single-wall tower, 50 mm across.
    fn vfa_tower(&self) -> Mesh {
        let mut m = Mesh::new();
        let r = 25.0 / (std::f64::consts::PI / 8.0).cos();
        let oct: Vec<(f64, f64)> = (0..8)
            .map(|i| {
                let a = std::f64::consts::PI / 8.0 + std::f64::consts::PI / 4.0 * i as f64;
                (25.0 + r * a.cos(), 25.0 + r * a.sin())
            })
            .collect();
        m.prism_z(&oct, 0.0, self.height_for(self.values().len()));
        m
    }

    // ── 2. the profiles ───────────────────────────────────────────────────────────────────

    /// Changes the job's copies of the profiles for the test, as Orca's GUI does.
    pub fn prepare(&self, machine: &mut Profile, process: &mut Profile, filament: &mut Profile) {
        apply_changes(machine, "machine", &self.carry);
        apply_changes(process, "process", &self.carry);
        apply_changes(filament, "filament", &self.carry);
        let nozzle = nozzle(machine);
        let s = |p: &mut Profile, k: &str, v: &str| set(p, k, v, false);
        let l = |p: &mut Profile, k: &str, v: &str| set(p, k, v, true);
        for (k, v) in [
            ("alternate_extra_wall", "0"),
            ("seam_slope_type", "none"),
            ("max_volumetric_extrusion_rate_slope", "0"),
        ] {
            s(process, k, v);
        }
        match self.test {
            Test::Temperature => {
                let t = gcode::fmt(self.start, 0);
                l(filament, "nozzle_temperature", &t);
                l(filament, "nozzle_temperature_initial_layer", &t);
                for (k, v) in [
                    ("brim_type", "outer_only"),
                    ("brim_width", "5"),
                    ("brim_object_gap", "0"),
                    ("overhang_reverse", "0"),
                ] {
                    s(process, k, v);
                }
            }
            Test::FlowCoarse | Test::FlowFine => {
                let (first, layer) = flow_layers(machine, process);
                let width = gcode::fmt(nozzle * 1.2, 3);
                for (k, v) in [
                    ("layer_height", gcode::fmt(layer, 3)),
                    ("initial_layer_print_height", gcode::fmt(first, 3)),
                    ("wall_loops", "1".into()),
                    ("only_one_wall_top", "1".into()),
                    ("sparse_infill_density", "35%".into()),
                    ("sparse_infill_pattern", "zigzag".into()),
                    ("bottom_shell_layers", "2".into()),
                    ("top_shell_layers", "5".into()),
                    ("top_shell_thickness", "0".into()),
                    ("bottom_shell_thickness", "0".into()),
                    ("top_surface_pattern", "monotonic".into()),
                    ("top_surface_line_width", width.clone()),
                    ("internal_solid_infill_line_width", width),
                    ("infill_direction", "45".into()),
                    ("solid_infill_direction", "135".into()),
                    ("ironing_type", "no ironing".into()),
                    ("gap_fill_target", "nowhere".into()),
                    ("skirt_loops", "0".into()),
                    ("brim_type", "no_brim".into()),
                ] {
                    s(process, k, &v);
                }
                fit_layer_height(machine, layer);
            }
            Test::PressureAdvance => {
                for (k, v) in [
                    ("wall_loops", "2"),
                    ("top_shell_layers", "0"),
                    ("bottom_shell_layers", "0"),
                    ("sparse_infill_density", "0%"),
                    ("seam_position", "back"),
                    ("brim_type", "brim_ears"),
                    ("brim_width", "6"),
                    ("brim_object_gap", "0"),
                    ("brim_ears_max_angle", "135"),
                ] {
                    s(process, k, v);
                }
                l(filament, "slow_down_layer_time", "1");
            }
            Test::Retraction => {
                let layer = if nozzle <= 0.1 {
                    0.05
                } else if nozzle <= 0.2 {
                    0.1
                } else {
                    0.2
                };
                let lh = gcode::fmt(layer, 3);
                for (k, v) in [
                    ("layer_height", lh.as_str()),
                    ("initial_layer_print_height", lh.as_str()),
                    ("wall_loops", "2"),
                    ("top_shell_layers", "0"),
                    ("bottom_shell_layers", "3"),
                    ("sparse_infill_density", "0%"),
                    ("seam_position", "aligned"),
                    ("wall_sequence", "inner wall/outer wall"),
                    ("overhang_reverse", "0"),
                ] {
                    s(process, k, v);
                }
                fit_layer_height(machine, layer);
                // Retractions must be plain E moves (no firmware retraction, no wipe), and long
                // enough that Orca writes them at all; the G-code gets each band's length after.
                let longest = gcode::fmt(self.end.max(0.5), 3);
                s(machine, "use_firmware_retraction", "0");
                l(machine, "wipe", "0");
                l(machine, "retraction_length", &longest);
                for k in ["filament_wipe", "filament_retraction_length"] {
                    if filament.contains_key(k) {
                        l(filament, k, if k == "filament_wipe" { "0" } else { &longest });
                    }
                }
            }
            Test::VolumetricSpeed => {
                let layer = round(nozzle * 0.8, 3);
                for (k, v) in [
                    ("spiral_mode", "1".to_string()),
                    ("wall_loops", "1".into()),
                    ("top_shell_layers", "0".into()),
                    ("bottom_shell_layers", "0".into()),
                    ("sparse_infill_density", "0%".into()),
                    ("layer_height", gcode::fmt(layer, 3)),
                    ("outer_wall_line_width", gcode::fmt(nozzle * 1.75, 3)),
                    ("brim_type", "outer_and_inner".into()),
                    ("brim_width", "5".into()),
                    ("brim_object_gap", "0".into()),
                ] {
                    s(process, k, &v);
                }
                l(process, "enable_overhang_speed", "0");
                fit_layer_height(machine, layer);
                let max = number(filament, "filament_max_volumetric_speed").unwrap_or(0.0);
                l(
                    filament,
                    "filament_max_volumetric_speed",
                    &gcode::fmt(max.max(200.0), 1),
                );
                l(filament, "slow_down_layer_time", "0");
            }
            Test::PressureAdvanceLine => {
                let w = gcode::fmt(pa_line_width(machine), 3);
                for (k, v) in [
                    ("wall_loops", "1"),
                    ("top_shell_layers", "1"),
                    ("bottom_shell_layers", "1"),
                    ("sparse_infill_density", "0%"),
                    ("brim_type", "no_brim"),
                    ("skirt_loops", "0"),
                    ("detect_thin_wall", "0"),
                    ("initial_layer_line_width", w.as_str()),
                ] {
                    s(process, k, v);
                }
            }
            Test::Tolerance => {}
            Test::Cornering | Test::ShaperFrequency | Test::ShaperDamping | Test::Vfa => {
                for (k, v) in [
                    ("spiral_mode", "1"),
                    ("spiral_mode_smooth", "0"),
                    ("wall_loops", "1"),
                    ("top_shell_layers", "0"),
                    ("bottom_shell_layers", "1"),
                    ("sparse_infill_density", "0%"),
                    ("detect_thin_wall", "0"),
                    ("brim_type", "outer_only"),
                    ("brim_width", "3"),
                    ("brim_object_gap", "0"),
                ] {
                    s(process, k, v);
                }
                l(process, "enable_overhang_speed", "0");
                l(filament, "slow_down_layer_time", "0");
                if self.test != Test::Vfa {
                    // Full speed and acceleration, and no corner velocity of Orca's own (it would
                    // send one with every acceleration change).
                    let max_speed = number(machine, "machine_max_speed_x")
                        .zip(number(machine, "machine_max_speed_y"))
                        .map(|(x, y)| x.min(y))
                        .filter(|v| *v > 0.0);
                    if let Some(v) = max_speed {
                        l(process, "outer_wall_speed", &gcode::fmt(v, 0));
                    }
                    if let Some(a) =
                        number(machine, "machine_max_acceleration_extruding").filter(|a| *a > 0.0)
                    {
                        l(process, "default_acceleration", &gcode::fmt(a, 0));
                        l(process, "outer_wall_acceleration", &gcode::fmt(a, 0));
                    }
                    l(process, "default_jerk", "0");
                    l(filament, "slow_down_min_speed", "0");
                    l(filament, "slow_down_for_layer_cooling", "0");
                    s(machine, "input_shaping_emit", "0");
                    if self.test == Test::ShaperFrequency {
                        s(process, "layer_height", "0.2");
                    }
                    let max = number(filament, "filament_max_volumetric_speed").unwrap_or(0.0);
                    l(
                        filament,
                        "filament_max_volumetric_speed",
                        &gcode::fmt(max.max(200.0), 1),
                    );
                }
                if self.test == Test::Cornering {
                    let e = gcode::fmt(self.end, 2);
                    l(machine, "machine_max_jerk_x", &e);
                    l(machine, "machine_max_jerk_y", &e);
                }
            }
        }
    }

    // ── 3. the G-code ─────────────────────────────────────────────────────────────────────

    /// How the sliced G-code becomes the test. `machine`, `process` and `filament` are the job's
    /// prepared profiles.
    /// `None` when the G-code is used as Orca wrote it (tolerance).
    pub fn plan(&self, machine: &Profile, process: &Profile, filament: &Profile) -> Option<Plan> {
        let values = self.values();
        let command = |command, first| Plan::Command {
            command,
            band: self.test.band(),
            values: values.clone(),
            first,
        };
        Some(match self.test {
            Test::Temperature => command(Command::Temperature, Vec::new()),
            Test::PressureAdvance => command(Command::PressureAdvance, Vec::new()),
            Test::Cornering => command(Command::CornerVelocity, Vec::new()),
            Test::ShaperFrequency => command(Command::ShaperFrequency, Vec::new()),
            Test::ShaperDamping => {
                // The frequencies found by the frequency test (saved, or carried from it).
                let f = |k| number(machine, k).filter(|v| *v > 0.0);
                let first = match (f("input_shaping_freq_x"), f("input_shaping_freq_y")) {
                    (Some(x), Some(y)) => vec![format!(
                        "SET_INPUT_SHAPER SHAPER_FREQ_X={x:.2} SHAPER_FREQ_Y={y:.2}"
                    )],
                    _ => Vec::new(),
                };
                command(Command::ShaperDamping, first)
            }
            Test::Vfa => Plan::Speed {
                band: self.test.band(),
                values: values.clone(),
                area: None,
            },
            Test::Tolerance => return None,
            Test::PressureAdvanceLine => {
                let (mesh, bars) = self.pa_lines(machine, process);
                let (lo, hi) = mesh.bounds();
                // The bars' own bounds (the labels stick out to the left).
                let lo_x = bars.iter().map(|b| b[0]).fold(f64::MAX, f64::min).min(lo[0]);
                let fast = number(process, "outer_wall_speed")
                    .unwrap_or(100.0)
                    .clamp(30.0, 150.0);
                Plan::PaLine {
                    bars,
                    values: values.clone(),
                    slow: PA_FAST,
                    speeds: (round(fast / 5.0, 1), fast),
                    center: ((lo_x + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0),
                }
            }
            Test::Retraction => Plan::Retraction {
                base: RETRACT_BASE,
                band: 1.0,
                values,
            },
            Test::VolumetricSpeed => {
                let d = number(filament, "filament_diameter")
                    .filter(|d| *d > 0.5)
                    .unwrap_or(1.75);
                Plan::Speed {
                    band: 1.0,
                    values: values.clone(),
                    area: Some(std::f64::consts::PI * d * d / 4.0),
                }
            }
            Test::FlowCoarse | Test::FlowFine => {
                let (first, layer) = flow_layers(machine, process);
                let (mesh, cells) = flow_blocks(self.modifiers(), first, layer);
                let (lo, hi) = mesh.bounds();
                Plan::Flow {
                    cells,
                    factors: self.modifiers().iter().map(|m| 1.0 + *m as f64 / 100.0).collect(),
                    center: ((lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0),
                }
            }
        })
    }

    // ── results ───────────────────────────────────────────────────────────────────────────

    /// What the user can pick from the print: `(value, label)`, bottom to top or in block order.
    /// `now` is the profiles with earlier, unsaved results applied.
    pub fn choices(&self, now: &Profiles) -> Vec<(String, String)> {
        let p = self.test.places();
        let u = self.test.unit();
        match self.test {
            Test::FlowCoarse | Test::FlowFine => {
                let flow = flow_ratio(&now.filament);
                return self
                    .modifiers()
                    .iter()
                    .map(|m| {
                        let v = signed(*m);
                        let label = format!("{v} → flow ratio {}", gcode::fmt(flow_after(flow, *m), 4));
                        (v, label)
                    })
                    .collect();
            }
            Test::Tolerance => {
                return TOLERANCES
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let label = format!(
                            "hole {} (marked {}) · +{} mm",
                            i + 1,
                            gcode::fmt(t * 100.0, 0),
                            gcode::fmt(*t, 2)
                        );
                        (gcode::fmt(*t, 2), label)
                    })
                    .collect();
            }
            _ => {}
        }
        let band = self.test.band();
        let base = self.test.base();
        self.values()
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let value = gcode::fmt(*v, p);
                let where_ = match self.test {
                    Test::Temperature => format!("block {}", i + 1),
                    Test::PressureAdvanceLine => format!("line {}", i + 1),
                    _ => {
                        let lo = base + i as f64 * band;
                        format!("{}–{} mm", gcode::fmt(lo, 1), gcode::fmt(lo + band, 1))
                    }
                };
                let label = format!("{where_} · {value} {u}").trim_end().to_string();
                (value, label)
            })
            .collect()
    }

    /// The profile changes for the values picked (one of [`Calibration::choices`] per axis).
    pub fn changes(&self, picked: &[&str], now: &Profiles) -> Result<Vec<Change>, String> {
        let choices = self.choices(now);
        if picked.len() != self.test.axes() {
            return Err("Pick a value for each axis.".into());
        }
        let mut v = Vec::new();
        for p in picked {
            let p = p.trim();
            if !choices.iter().any(|(c, _)| c == p) {
                return Err("Pick one of the values the print has.".into());
            }
            v.push(
                p.trim_start_matches('+')
                    .parse::<f64>()
                    .map_err(|_| "That isn't a number.".to_string())?,
            );
        }
        let kind = self.test.result_kind();
        let ch = |k: &str, v: String| Change {
            kind: kind.to_string(),
            key: k.to_string(),
            value: v,
        };
        let (filament, places) = (&now.filament, self.test.places());
        Ok(match self.test {
            Test::Temperature => {
                // The first layer keeps its offset from the other layers.
                let old = number(filament, "nozzle_temperature").unwrap_or(v[0]);
                let first = number(filament, "nozzle_temperature_initial_layer").unwrap_or(old);
                vec![
                    ch("nozzle_temperature", gcode::fmt(v[0], 0)),
                    ch(
                        "nozzle_temperature_initial_layer",
                        gcode::fmt((first - old + v[0]).clamp(0.0, 500.0), 0),
                    ),
                ]
            }
            Test::FlowCoarse | Test::FlowFine => vec![ch(
                "filament_flow_ratio",
                gcode::fmt(flow_after(flow_ratio(filament), v[0] as i32), 4),
            )],
            Test::PressureAdvance | Test::PressureAdvanceLine => vec![
                ch("pressure_advance", gcode::fmt(v[0], 4)),
                ch("enable_pressure_advance", "1".into()),
            ],
            Test::Retraction => vec![ch("filament_retraction_length", gcode::fmt(v[0], 2))],
            Test::VolumetricSpeed => vec![ch("filament_max_volumetric_speed", gcode::fmt(v[0], 1))],
            Test::Tolerance => {
                // Holes grow by the compensation on every side: half the clearance the peg needed.
                let now_comp = number(&now.process, "xy_hole_compensation").unwrap_or(0.0);
                vec![ch("xy_hole_compensation", gcode::fmt(now_comp + v[0] / 2.0, 3))]
            }
            Test::Cornering => vec![
                ch("machine_max_jerk_x", gcode::fmt(v[0], places)),
                ch("machine_max_jerk_y", gcode::fmt(v[0], places)),
            ],
            Test::ShaperFrequency => vec![
                ch("input_shaping_freq_x", gcode::fmt(v[0], places)),
                ch("input_shaping_freq_y", gcode::fmt(v[1], places)),
            ],
            Test::ShaperDamping => vec![
                ch("input_shaping_damp_x", gcode::fmt(v[0], places)),
                ch("input_shaping_damp_y", gcode::fmt(v[0], places)),
            ],
            Test::Vfa => vec![ch("outer_wall_speed", gcode::fmt(v[0], 0))],
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Rewrites a job's sliced G-code (`out.gcode`) into the test. The rewrite goes to a random
/// temporary name and is moved into place here, so a cancelled job can't have a late rewrite
/// land on a newer slice.
pub async fn rewrite_output(
    storage: &super::Storage,
    job: i64,
    plan: Plan,
) -> Result<gcode::Summary, String> {
    let src = storage.output_path(job);
    let tmp = storage.job_dir(job).join(format!(
        ".calib.{}",
        hex::encode(crate::crypto::random_bytes::<6>())
    ));
    let st = storage.clone();
    let (s, t) = (src.clone(), tmp.clone());
    let res = tokio::task::spawn_blocking(move || {
        use std::io::BufReader;
        let open = |p: &std::path::Path| {
            std::fs::File::open(p)
                .map(BufReader::new)
                .map_err(|e| format!("couldn't read the G-code: {e}"))
        };
        let out = st
            .create_blocking(&t)
            .map(std::io::BufWriter::new)
            .map_err(|e| format!("couldn't write the G-code: {e}"))?;
        let again = s.clone();
        gcode::rewrite(
            open(&s)?,
            move || Ok(Box::new(BufReader::new(std::fs::File::open(&again)?)) as Box<dyn std::io::BufRead>),
            out,
            &plan,
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("the G-code rewrite stopped: {e}")));
    match res {
        Ok(sum) => match tokio::fs::rename(&tmp, &src).await {
            Ok(()) => Ok(sum),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(format!("couldn't store the G-code: {e}"))
            }
        },
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// The tolerance test: a plate with 8 mm square holes of growing clearance (numbered in
/// hundredths of a millimetre below each), and an 8 mm square peg beside it.
fn tolerance_plate() -> Mesh {
    const PITCH: f64 = 13.0;
    const T: f64 = 4.0;
    const D: f64 = 22.0;
    let glyph = Glyph {
        width: 2.0,
        height: 3.5,
        stroke: 0.7,
        gap: 0.4,
        sink: 0.05,
    };
    let mut m = Mesh::new();
    let width = 2.0 * PITCH / 2.0 + (TOLERANCES.len() - 1) as f64 * PITCH;
    let cy = 13.0;
    let mut x = 0.0;
    for (i, t) in TOLERANCES.iter().enumerate() {
        let cx = PITCH / 2.0 + i as f64 * PITCH;
        let half = (HOLE + t) / 2.0;
        // The plate around this hole: the strip before it, then above and below it.
        m.cuboid([x, 0.0, 0.0], [cx - half, D, T]);
        m.cuboid([cx - half - 0.01, 0.0, 0.0], [cx + half + 0.01, cy - half, T]);
        m.cuboid([cx - half - 0.01, cy + half, 0.0], [cx + half + 0.01, D, T]);
        x = cx + half;
        let label = gcode::fmt(t * 100.0, 0);
        m.text_top(&label, cx - glyph.measure(&label) / 2.0, 1.2, T, glyph, 0.6);
    }
    m.cuboid([x, 0.0, 0.0], [width, D, T]);
    // The peg, beside the plate.
    m.cuboid(
        [width + 6.0, cy - HOLE / 2.0, 0.0],
        [width + 6.0 + HOLE, cy + HOLE / 2.0, 12.0],
    );
    m
}

/// The process's first layer height.
fn first_layer(process: &Profile) -> f64 {
    number(process, "initial_layer_print_height")
        .filter(|h| *h > 0.0)
        .unwrap_or(0.2)
}

/// The pressure advance lines' line width (each bar is two of them).
fn pa_line_width(machine: &Profile) -> f64 {
    round(nozzle(machine) * 1.125, 3)
}

/// The flow test's first layer and layer height: half the nozzle, as Orca's GUI does.
fn flow_layers(machine: &Profile, process: &Profile) -> (f64, f64) {
    let layer = round(nozzle(machine) / 2.0, 3);
    let first = number(process, "initial_layer_print_height")
        .unwrap_or(layer)
        .max(layer);
    (first, layer)
}

/// The flow test: a 20 mm block per modifier (2 bottom, 3 infill and 5 top layers), each with a
/// numbered tab in front, in rows. Returns the mesh and each block's cell (block and tab).
fn flow_blocks(modifiers: &[i32], first: f64, layer: f64) -> (Mesh, Vec<[f64; 4]>) {
    const SIZE: f64 = 20.0;
    const TAB: f64 = 7.0;
    const GAP: f64 = 4.0;
    let cols = if modifiers.len() > 9 { 5 } else { 3 };
    let rows = modifiers.len().div_ceil(cols);
    let glyph = Glyph {
        width: 3.0,
        height: 5.0,
        stroke: 0.9,
        gap: 0.6,
        sink: 0.05,
    };
    let top = first + 9.0 * layer;
    let tab_top = first + layer;
    let mut m = Mesh::new();
    let mut cells = Vec::new();
    for (i, md) in modifiers.iter().enumerate() {
        // The first block at the back left, reading like a page.
        let (c, r) = (i % cols, rows - 1 - i / cols);
        let x = c as f64 * (SIZE + GAP);
        let y = r as f64 * (SIZE + TAB + GAP);
        m.cuboid([x, y, 0.0], [x + SIZE, y + TAB + 0.5, tab_top]);
        m.cuboid([x, y + TAB, 0.0], [x + SIZE, y + TAB + SIZE, top]);
        let label = signed(*md);
        let lx = x + (SIZE - glyph.measure(&label)) / 2.0;
        m.text_top(&label, lx, y + 1.0, tab_top, glyph, 3.0 * layer);
        cells.push([x, y, x + SIZE, y + TAB + SIZE]);
    }
    (m, cells)
}

fn signed(m: i32) -> String {
    if m > 0 { format!("+{m}") } else { m.to_string() }
}

fn flow_ratio(filament: &Profile) -> f64 {
    number(filament, "filament_flow_ratio")
        .filter(|f| *f > 0.0)
        .unwrap_or(1.0)
}

fn flow_after(flow: f64, modifier: i32) -> f64 {
    round(flow * (1.0 + modifier as f64 / 100.0), 4)
}

fn nozzle(machine: &Profile) -> f64 {
    number(machine, "nozzle_diameter")
        .filter(|d| *d > 0.0)
        .unwrap_or(0.4)
}

/// The bed's size from `printable_area` (`["0x0", "350x0", …]`).
fn bed_size(machine: &Profile) -> Option<(f64, f64)> {
    let pts: Vec<(f64, f64)> = match machine.get("printable_area")? {
        Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str())
            .filter_map(|s| s.split_once('x'))
            .filter_map(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)))
            .collect(),
        _ => return None,
    };
    let xs = pts.iter().map(|p| p.0);
    let ys = pts.iter().map(|p| p.1);
    let w = xs.clone().fold(f64::MIN, f64::max) - xs.fold(f64::MAX, f64::min);
    let h = ys.clone().fold(f64::MIN, f64::max) - ys.fold(f64::MAX, f64::min);
    (w > 0.0 && h > 0.0).then_some((w, h))
}

/// Lets the machine print `layer` (its min/max layer height limits).
fn fit_layer_height(machine: &mut Profile, layer: f64) {
    if number(machine, "max_layer_height").is_some_and(|m| m < layer) {
        set(machine, "max_layer_height", &gcode::fmt(layer, 3), true);
    }
    if number(machine, "min_layer_height").is_some_and(|m| m > layer) {
        set(machine, "min_layer_height", &gcode::fmt(layer, 3), true);
    }
}

fn round(v: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (v * f).round() / f
}

/// Sets a setting, keeping the shape it has (a list gets the value in every slot). A key the
/// profile lacks becomes a one-element list when `list`.
fn set(p: &mut Profile, key: &str, value: &str, list: bool) {
    let v = Value::String(value.to_string());
    let new = match p.get(key) {
        Some(Value::Array(a)) if !a.is_empty() => Value::Array(vec![v; a.len()]),
        Some(Value::Array(_)) => Value::Array(vec![v]),
        Some(_) => v,
        None if list => Value::Array(vec![v]),
        None => v,
    };
    p.insert(key.to_string(), new);
}

/// The first value of a setting as text.
fn text(p: &Profile, key: &str) -> String {
    super::settings::current(p, key)
}

fn number(p: &Profile, key: &str) -> Option<f64> {
    text(p, key)
        .trim_end_matches('%')
        .trim()
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Profile {
        v.as_object().unwrap().clone()
    }

    fn machine() -> Profile {
        obj(json!({
            "nozzle_diameter": ["0.4"],
            "printable_area": ["0x0", "220x0", "220x220", "0x220"],
            "printable_height": "250",
            "max_layer_height": ["0.28"],
            "min_layer_height": ["0.08"],
            "retraction_length": ["0.8"],
            "wipe": ["1"],
            "extruder_type": ["Direct Drive"],
        }))
    }

    fn filament() -> Profile {
        obj(json!({
            "filament_type": ["PLA"],
            "nozzle_temperature": ["220"],
            "nozzle_temperature_initial_layer": ["225"],
            "filament_flow_ratio": ["0.98"],
            "filament_max_volumetric_speed": ["12"],
            "filament_diameter": ["1.75"],
            "filament_retraction_length": ["nil"],
        }))
    }

    #[test]
    fn slugs_round_trip() {
        for t in Test::ALL {
            assert_eq!(Test::parse(t.slug()), Some(t));
        }
        assert_eq!(Test::parse("nope"), None);
    }

    #[test]
    fn defaults_follow_the_profiles() {
        assert_eq!(
            defaults(Test::Temperature, &machine(), &filament()),
            (230.0, 190.0, 5.0)
        );
        assert_eq!(
            defaults(Test::PressureAdvance, &machine(), &filament()),
            (0.0, 0.1, 0.002)
        );
        let mut bowden = machine();
        bowden.insert("extruder_type".into(), json!(["Bowden"]));
        assert_eq!(
            defaults(Test::PressureAdvance, &bowden, &filament()),
            (0.0, 1.0, 0.02)
        );
        let mut odd = filament();
        odd.insert("filament_type".into(), json!(["HIPS"]));
        assert_eq!(defaults(Test::Temperature, &machine(), &odd), (240.0, 200.0, 5.0));
        // Every default is a valid range.
        for t in Test::ALL {
            let (s, e, st) = defaults(t, &machine(), &filament());
            Calibration::new(t, s, e, st, &machine()).unwrap();
        }
    }

    #[test]
    fn ranges_are_checked() {
        let m = machine();
        let bad = |t, s, e, st| Calibration::new(t, s, e, st, &m).unwrap_err();
        assert!(bad(Test::Temperature, 190.0, 230.0, 5.0).contains("hottest"));
        assert!(bad(Test::Temperature, 230.0, 190.0, 3.0).contains("whole number"));
        assert!(bad(Test::Temperature, 330.0, 160.0, 5.0).contains("most is 15"));
        assert!(bad(Test::PressureAdvance, 0.0, 0.1, 0.0).contains("more than 0"));
        assert!(bad(Test::PressureAdvance, 0.0, f64::NAN, 0.1).contains("numbers"));
        assert!(bad(Test::Retraction, 0.0, 20.0, 0.5).contains("0–10"));
        let mut short = m.clone();
        short.insert("printable_height".into(), json!("50"));
        let e = Calibration::new(Test::Temperature, 230.0, 190.0, 5.0, &short).unwrap_err();
        assert!(e.contains("90 mm tall"), "{e}");
        let c = Calibration::new(Test::Temperature, 230.0, 190.0, 5.0, &m).unwrap();
        assert_eq!(
            c.values(),
            [230.0, 225.0, 220.0, 215.0, 210.0, 205.0, 200.0, 195.0, 190.0]
        );
        let c = Calibration::new(Test::PressureAdvance, 0.0, 0.1, 0.002, &m).unwrap();
        assert_eq!(c.values().len(), 51);
        assert_eq!(c.values()[12], 0.024);
        assert_eq!(c.file_stem(), "calib_pressure_advance_0-0.1");
        let c = Calibration::new(Test::FlowCoarse, 1.0, 2.0, 3.0, &m).unwrap();
        assert_eq!((c.start, c.end, c.step), (0.0, 0.0, 0.0));
        assert_eq!(c.file_stem(), "calib_flow_coarse");
    }

    #[test]
    fn models_are_the_right_size() {
        let m = machine();
        let p = obj(json!({"initial_layer_print_height": "0.2"}));
        let size = |c: &Calibration| {
            let stl = c.model(&m, &p);
            let n = u32::from_le_bytes(stl[80..84].try_into().unwrap()) as usize;
            assert_eq!(stl.len(), 84 + n * 50);
            n
        };
        let temp = Calibration::new(Test::Temperature, 230.0, 190.0, 5.0, &m).unwrap();
        assert_eq!(temp.temperature_tower().bounds().1[2], 90.0);
        assert!(size(&temp) > 9 * 5 * 12);
        let pa = Calibration::new(Test::PressureAdvance, 0.0, 0.1, 0.002, &m).unwrap();
        assert_eq!(pa.pa_tower().bounds().1[2], 51.0);
        let r = Calibration::new(Test::Retraction, 0.0, 2.0, 0.1, &m).unwrap();
        let (_, hi) = r.retraction_towers().bounds();
        assert!((hi[2] - 21.4).abs() < 1e-9);
        let v = Calibration::new(Test::VolumetricSpeed, 5.0, 20.0, 0.5, &m).unwrap();
        let (lo, hi) = v.speed_vase(&m).bounds();
        assert_eq!((hi[0] - lo[0], hi[2]), (100.0, 31.0));
        for t in [Test::FlowCoarse, Test::FlowFine] {
            let c = Calibration::new(t, 0.0, 0.0, 0.0, &m).unwrap();
            assert!(size(&c) > 0);
        }
    }

    #[test]
    fn flow_cells_hold_their_blocks() {
        let (mesh, cells) = flow_blocks(&FLOW_FINE, 0.2, 0.2);
        assert_eq!(cells.len(), 10);
        let (lo, hi) = mesh.bounds();
        assert_eq!(lo[0], 0.0);
        assert!((hi[2] - 2.0).abs() < 1e-9, "{hi:?}");
        // Cells don't overlap, and together cover the model.
        for (i, a) in cells.iter().enumerate() {
            for b in &cells[i + 1..] {
                assert!(a[2] <= b[0] || b[2] <= a[0] || a[3] <= b[1] || b[3] <= a[1]);
            }
        }
        let max_x = cells.iter().map(|c| c[2]).fold(0.0, f64::max);
        assert_eq!(max_x, hi[0]);
    }

    #[test]
    fn prepare_writes_the_tests_settings() {
        let (mut m, mut f) = (machine(), filament());
        let mut p = obj(json!({"wall_loops": "3", "enable_overhang_speed": ["1", "1"]}));
        let c = Calibration::new(Test::Retraction, 0.0, 2.0, 0.1, &m).unwrap();
        c.prepare(&mut m, &mut p, &mut f);
        assert_eq!(m["wipe"], json!(["0"]));
        assert_eq!(m["retraction_length"], json!(["2"]));
        assert_eq!(f["filament_retraction_length"], json!(["2"]));
        assert_eq!(p["wall_loops"], json!("2"));
        assert_eq!(p["layer_height"], json!("0.2"));

        let (mut m, mut f) = (machine(), filament());
        let mut c = Calibration::new(Test::VolumetricSpeed, 5.0, 20.0, 0.5, &m).unwrap();
        c.carry = vec![Change {
            kind: "filament".into(),
            key: "filament_flow_ratio".into(),
            value: "1.01".into(),
        }];
        c.prepare(&mut m, &mut p, &mut f);
        assert_eq!(p["spiral_mode"], json!("1"));
        assert_eq!(p["layer_height"], json!("0.32"));
        assert_eq!(p["enable_overhang_speed"], json!(["0", "0"]));
        assert_eq!(m["max_layer_height"], json!(["0.32"]));
        assert_eq!(f["filament_max_volumetric_speed"], json!(["200"]));
        assert_eq!(f["filament_flow_ratio"], json!(["1.01"]));

        let (mut m, mut f) = (machine(), filament());
        let c = Calibration::new(Test::Temperature, 230.0, 190.0, 5.0, &m).unwrap();
        c.prepare(&mut m, &mut p, &mut f);
        assert_eq!(f["nozzle_temperature"], json!(["230"]));
        assert_eq!(f["nozzle_temperature_initial_layer"], json!(["230"]));
    }

    fn with(f: &Profile) -> Profiles {
        Profiles {
            machine: machine(),
            process: obj(json!({"xy_hole_compensation": "0.02", "outer_wall_speed": ["200"]})),
            filament: f.clone(),
        }
    }

    #[test]
    fn results_become_filament_changes() {
        let m = machine();
        let f = filament();
        let temp = Calibration::new(Test::Temperature, 230.0, 190.0, 5.0, &m).unwrap();
        let ch = temp.choices(&with(&f));
        assert_eq!(ch[0], ("230".into(), "block 1 · 230 °C".into()));
        let c = temp.changes(&["210"], &with(&f)).unwrap();
        assert_eq!(c[0].to_string(), "nozzle_temperature = 210");
        assert_eq!(c[0].kind, "filament");
        assert_eq!(c[1].to_string(), "nozzle_temperature_initial_layer = 215");
        assert!(temp.changes(&["211"], &with(&f)).is_err());
        assert!(temp.changes(&["+5; G28"], &with(&f)).is_err());
        assert!(temp.changes(&["210", "210"], &with(&f)).is_err());

        let coarse = Calibration::new(Test::FlowCoarse, 0.0, 0.0, 0.0, &m).unwrap();
        let ch = coarse.choices(&with(&f));
        assert_eq!(ch[5], ("+5".into(), "+5 → flow ratio 1.029".into()));
        assert_eq!(coarse.changes(&["+5"], &with(&f)).unwrap()[0].value, "1.029");
        assert_eq!(coarse.changes(&["-20"], &with(&f)).unwrap()[0].value, "0.784");

        let mut f2 = f.clone();
        apply_changes(&mut f2, "filament", &coarse.changes(&["+5"], &with(&f)).unwrap());
        let fine = Calibration::new(Test::FlowFine, 0.0, 0.0, 0.0, &m).unwrap();
        assert_eq!(fine.changes(&["-3"], &with(&f2)).unwrap()[0].value, "0.9981");

        let pa = Calibration::new(Test::PressureAdvance, 0.0, 0.1, 0.002, &m).unwrap();
        assert_eq!(pa.choices(&with(&f))[12].1, "12–13 mm · 0.024");
        let c = pa.changes(&["0.024"], &with(&f)).unwrap();
        assert_eq!(c.len(), 2);
        let mut f3 = f.clone();
        apply_changes(&mut f3, "filament", &c);
        assert_eq!(f3["pressure_advance"], json!(["0.024"]));
        assert_eq!(
            klipper_note(Test::PressureAdvance, &c).unwrap(),
            "[extruder]\npressure_advance: 0.024"
        );

        let r = Calibration::new(Test::Retraction, 0.0, 2.0, 0.1, &m).unwrap();
        assert_eq!(r.choices(&with(&f))[3].1, "3.4–4.4 mm · 0.3 mm");
        let mut f4 = f.clone();
        apply_changes(&mut f4, "filament", &r.changes(&["0.3"], &with(&f)).unwrap());
        assert_eq!(f4["filament_retraction_length"], json!(["0.3"]));

        // Unknown keys, and keys of another profile kind, never reach a profile.
        let mut f5 = f.clone();
        let bad = |kind: &str, key: &str| Change {
            kind: kind.into(),
            key: key.into(),
            value: "G28".into(),
        };
        apply_changes(
            &mut f5,
            "filament",
            &[
                bad("filament", "filament_start_gcode"),
                bad("machine", "nozzle_temperature"),
            ],
        );
        assert_eq!(f5, f);
        // Results stored before kinds existed read as filament.
        let old: Change = serde_json::from_str(r#"{"key":"filament_flow_ratio","value":"1.01"}"#).unwrap();
        assert_eq!(old.kind, "filament");
    }

    #[test]
    fn printer_and_process_results() {
        let m = machine();
        let f = filament();
        let now = with(&f);
        let tol = Calibration::new(Test::Tolerance, 0.0, 0.0, 0.0, &m).unwrap();
        assert_eq!(
            tol.choices(&now)[2],
            ("0.1".into(), "hole 3 (marked 10) · +0.1 mm".into())
        );
        let c = tol.changes(&["0.1"], &now).unwrap();
        assert_eq!(
            (c[0].kind.as_str(), c[0].to_string()),
            ("process", "xy_hole_compensation = 0.07".into())
        );

        let corner = Calibration::new(Test::Cornering, 1.0, 20.0, 1.0, &m).unwrap();
        assert_eq!(corner.choices(&now)[4].1, "8–10 mm · 5 mm/s");
        let c = corner.changes(&["5"], &now).unwrap();
        assert_eq!(c[0].kind, "machine");
        let mut mm = m.clone();
        apply_changes(&mut mm, "machine", &c);
        assert_eq!(mm["machine_max_jerk_y"], json!(["5"]));
        assert_eq!(
            klipper_note(Test::Cornering, &c).unwrap(),
            "[printer]\nsquare_corner_velocity: 5"
        );

        let freq = Calibration::new(Test::ShaperFrequency, 20.0, 80.0, 1.0, &m).unwrap();
        assert!(freq.changes(&["48"], &now).is_err(), "needs both axes");
        let c = freq.changes(&["48", "52"], &now).unwrap();
        let mut mm = m.clone();
        apply_changes(&mut mm, "machine", &c);
        assert_eq!(mm["input_shaping_freq_x"], json!("48"));
        assert_eq!(mm["input_shaping_freq_y"], json!("52"));
        assert_eq!(
            klipper_note(Test::ShaperFrequency, &c).unwrap(),
            "[input_shaper]\nshaper_freq_x: 48\nshaper_freq_y: 52"
        );

        let vfa = Calibration::new(Test::Vfa, 40.0, 200.0, 10.0, &m).unwrap();
        let c = vfa.changes(&["120"], &now).unwrap();
        let mut p = now.process.clone();
        apply_changes(&mut p, "process", &c);
        assert_eq!(p["outer_wall_speed"], json!(["120"]));

        let line = Calibration::new(Test::PressureAdvanceLine, 0.0, 0.08, 0.005, &m).unwrap();
        assert_eq!(line.choices(&now)[2].1, "line 3 · 0.01");
        assert_eq!(
            line.changes(&["0.01"], &now).unwrap()[0].to_string(),
            "pressure_advance = 0.01"
        );
    }

    #[test]
    fn the_optional_tests_models_and_plans() {
        let m = machine();
        let p = obj(json!({"initial_layer_print_height": "0.25", "outer_wall_speed": ["300"]}));
        let mut f = filament();
        // Bed and height limits.
        let e = Calibration::new(Test::PressureAdvanceLine, 0.0, 0.3, 0.01, &m).unwrap_err();
        assert!(e.contains("most is 30"), "{e}");
        let mut small = m.clone();
        small.insert("printable_area".into(), json!(["0x0", "90x0", "90x90", "0x90"]));
        let e = Calibration::new(Test::PressureAdvanceLine, 0.0, 0.08, 0.005, &small).unwrap_err();
        assert!(e.contains("need 100 × 78 mm of bed"), "{e}");
        let line = Calibration::new(Test::PressureAdvanceLine, 0.0, 0.08, 0.005, &m).unwrap();
        let (mesh, bars) = line.pa_lines(&m, &p);
        assert_eq!(bars.len(), 17);
        let (lo, hi) = mesh.bounds();
        assert!(
            lo[0] < -10.0 && hi[0] == PA_LINE && (hi[2] - 0.25).abs() < 1e-9,
            "{lo:?} {hi:?}"
        );
        let Some(Plan::PaLine { speeds, values, .. }) = line.plan(&m, &p, &f) else {
            panic!()
        };
        assert_eq!(speeds, (30.0, 150.0));
        assert_eq!(values.len(), 17);

        let tol = Calibration::new(Test::Tolerance, 0.0, 0.0, 0.0, &m).unwrap();
        assert_eq!(tol.plan(&m, &p, &f), None);
        let (lo, hi) = tolerance_plate().bounds();
        assert_eq!((lo[0], hi[2]), (0.0, 12.0));

        let mut m2 = m.clone();
        m2.insert("machine_max_speed_x".into(), json!(["500", "200"]));
        m2.insert("machine_max_speed_y".into(), json!(["400", "200"]));
        m2.insert("machine_max_acceleration_extruding".into(), json!(["7000"]));
        let mut p2 = p.clone();
        let corner = Calibration::new(Test::Cornering, 1.0, 20.0, 1.0, &m).unwrap();
        corner.prepare(&mut m2, &mut p2, &mut f);
        assert_eq!(p2["outer_wall_speed"], json!(["400"]));
        assert_eq!(p2["default_acceleration"], json!(["7000"]));
        assert_eq!(p2["default_jerk"], json!(["0"]));
        assert_eq!(p2["spiral_mode"], json!("1"));
        assert_eq!(m2["machine_max_jerk_x"], json!(["20"]));
        assert_eq!(corner.ringing_tower().bounds().1[2], 40.0);
        assert!(matches!(
            corner.plan(&m2, &p2, &f),
            Some(Plan::Command {
                command: Command::CornerVelocity,
                band: 2.0,
                ..
            })
        ));

        // Damping uses the frequencies found (carried or saved) as its preamble.
        let mut damp = Calibration::new(Test::ShaperDamping, 0.02, 0.3, 0.01, &m).unwrap();
        damp.carry = Calibration::new(Test::ShaperFrequency, 20.0, 80.0, 1.0, &m)
            .unwrap()
            .changes(&["48", "52"], &with(&f))
            .unwrap();
        let (mut m3, mut p3, mut f3) = (m.clone(), p.clone(), f.clone());
        damp.prepare(&mut m3, &mut p3, &mut f3);
        let Some(Plan::Command { first, values, .. }) = damp.plan(&m3, &p3, &f3) else {
            panic!()
        };
        assert_eq!(
            first,
            ["SET_INPUT_SHAPER SHAPER_FREQ_X=48.00 SHAPER_FREQ_Y=52.00"]
        );
        assert_eq!(values.len(), 29);

        let vfa = Calibration::new(Test::Vfa, 40.0, 200.0, 10.0, &m).unwrap();
        let (lo, hi) = vfa.vfa_tower().bounds();
        assert!(
            (hi[0] - lo[0] - 50.0).abs() < 1e-9 && hi[2] == 85.0,
            "{lo:?} {hi:?}"
        );
        assert!(matches!(
            vfa.plan(&m, &p, &f),
            Some(Plan::Speed {
                area: None,
                band: 5.0,
                ..
            })
        ));
        for t in Test::ALL {
            let (s, e, st) = defaults(t, &m, &f);
            let c = Calibration::new(t, s, e, st, &m).unwrap();
            assert!(c.model(&m, &p).len() > 84, "{t:?}");
            assert_eq!(t.optional(), Test::ALL.iter().position(|x| *x == t).unwrap() >= 6);
        }
    }

    #[test]
    fn plans_match_the_models() {
        let m = machine();
        let p = obj(json!({"initial_layer_print_height": "0.2"}));
        let f = filament();
        let c = Calibration::new(Test::FlowCoarse, 0.0, 0.0, 0.0, &m).unwrap();
        let Some(Plan::Flow {
            cells,
            factors,
            center,
        }) = c.plan(&m, &p, &f)
        else {
            panic!()
        };
        assert_eq!(cells.len(), 9);
        assert_eq!(factors[0], 0.8);
        assert_eq!(center, (34.0, 44.5));
        let v = Calibration::new(Test::VolumetricSpeed, 5.0, 20.0, 0.5, &m).unwrap();
        let Some(Plan::Speed { area, values, .. }) = v.plan(&m, &p, &f) else {
            panic!()
        };
        assert!((area.unwrap() - 2.405).abs() < 1e-3);
        assert_eq!(values.len(), 31);
    }
}
