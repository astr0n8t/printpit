//! Calibration prints (PLAN §15.13): the tests OrcaSlicer's GUI offers (temperature tower, flow
//! rate in two passes, pressure advance tower, retraction test, max volumetric speed), made with
//! the CLI the worker runs.
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
//! What the user reads off the print becomes [`Change`]s to the filament profile
//! ([`Calibration::changes`]): fixed keys, values computed here, never user text.

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
}

impl Test {
    pub const ALL: [Test; 6] = [
        Test::Temperature,
        Test::FlowCoarse,
        Test::FlowFine,
        Test::PressureAdvance,
        Test::Retraction,
        Test::VolumetricSpeed,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            Test::Temperature => "temperature",
            Test::FlowCoarse => "flow_coarse",
            Test::FlowFine => "flow_fine",
            Test::PressureAdvance => "pressure_advance",
            Test::Retraction => "retraction",
            Test::VolumetricSpeed => "volumetric_speed",
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
        }
    }

    /// Whether the user picks a range (the flow passes are fixed).
    pub fn has_range(self) -> bool {
        !matches!(self, Test::FlowCoarse | Test::FlowFine)
    }

    pub fn unit(self) -> &'static str {
        match self {
            Test::Temperature => "°C",
            Test::FlowCoarse | Test::FlowFine => "%",
            Test::PressureAdvance => "",
            Test::Retraction => "mm",
            Test::VolumetricSpeed => "mm³/s",
        }
    }

    /// Decimals the range inputs and values use.
    pub fn places(self) -> usize {
        match self {
            Test::Temperature | Test::FlowCoarse | Test::FlowFine => 0,
            Test::PressureAdvance => 4,
            Test::Retraction => 2,
            Test::VolumetricSpeed => 1,
        }
    }

    /// Where a result goes, for the page: the filament profile's setting.
    pub fn setting(self) -> &'static str {
        match self {
            Test::Temperature => "nozzle temperature",
            Test::FlowCoarse | Test::FlowFine => "flow ratio",
            Test::PressureAdvance => "pressure advance",
            Test::Retraction => "retraction length (filament override)",
            Test::VolumetricSpeed => "max volumetric speed",
        }
    }

    /// Height of one band (mm).
    fn band(self) -> f64 {
        match self {
            Test::Temperature => TEMP_BLOCK,
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

/// One setting a result changes. Only keys in [`RESULT_KEYS`] are ever written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub key: String,
    pub value: String,
}

/// The filament settings results can change.
pub const RESULT_KEYS: [&str; 7] = [
    "nozzle_temperature",
    "nozzle_temperature_initial_layer",
    "filament_flow_ratio",
    "pressure_advance",
    "enable_pressure_advance",
    "filament_retraction_length",
    "filament_max_volumetric_speed",
];

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} = {}", self.key, self.value)
    }
}

/// Writes `changes` into a filament profile (each per-extruder list gets the value everywhere).
pub fn apply_changes(filament: &mut Profile, changes: &[Change]) {
    for c in changes.iter().filter(|c| RESULT_KEYS.contains(&c.key.as_str())) {
        set(filament, &c.key, &c.value, true);
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
            Test::Retraction => (0.0, 10.0, 0.05, 60),
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

    // ── 2. the profiles ───────────────────────────────────────────────────────────────────

    /// Changes the job's copies of the profiles for the test, as Orca's GUI does.
    pub fn prepare(&self, machine: &mut Profile, process: &mut Profile, filament: &mut Profile) {
        apply_changes(filament, &self.carry);
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
        }
    }

    // ── 3. the G-code ─────────────────────────────────────────────────────────────────────

    /// How the sliced G-code becomes the test. `machine`, `process` and `filament` are the job's
    /// prepared profiles.
    pub fn plan(&self, machine: &Profile, process: &Profile, filament: &Profile) -> Plan {
        let values = self.values();
        match self.test {
            Test::Temperature => Plan::Command {
                command: Command::Temperature,
                band: TEMP_BLOCK,
                values,
            },
            Test::PressureAdvance => Plan::Command {
                command: Command::PressureAdvance,
                band: 1.0,
                values,
            },
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
                    values,
                    area: std::f64::consts::PI * d * d / 4.0,
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
        }
    }

    // ── results ───────────────────────────────────────────────────────────────────────────

    /// What the user can pick from the print: `(value, label)`, bottom to top or in block order.
    /// `filament` is the profile with earlier, unsaved results applied.
    pub fn choices(&self, filament: &Profile) -> Vec<(String, String)> {
        let p = self.test.places();
        let u = self.test.unit();
        if !self.test.has_range() {
            let flow = flow_ratio(filament);
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
        let band = self.test.band();
        let base = self.test.base();
        self.values()
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let value = gcode::fmt(*v, p);
                let where_ = if self.test == Test::Temperature {
                    format!("block {}", i + 1)
                } else {
                    let lo = base + i as f64 * band;
                    format!("{}–{} mm", gcode::fmt(lo, 1), gcode::fmt(lo + band, 1))
                };
                let label = format!("{where_} · {value} {u}").trim_end().to_string();
                (value, label)
            })
            .collect()
    }

    /// The profile changes for the value picked (one of [`Calibration::choices`]).
    pub fn changes(&self, picked: &str, filament: &Profile) -> Result<Vec<Change>, String> {
        let picked = picked.trim();
        if !self.choices(filament).iter().any(|(v, _)| v == picked) {
            return Err("Pick one of the values the print has.".into());
        }
        let ch = |k: &str, v: String| Change {
            key: k.to_string(),
            value: v,
        };
        let v: f64 = picked
            .trim_start_matches('+')
            .parse()
            .map_err(|_| "That isn't a number.".to_string())?;
        Ok(match self.test {
            Test::Temperature => {
                // The first layer keeps its offset from the other layers.
                let old = number(filament, "nozzle_temperature").unwrap_or(v);
                let first = number(filament, "nozzle_temperature_initial_layer").unwrap_or(old);
                vec![
                    ch("nozzle_temperature", gcode::fmt(v, 0)),
                    ch(
                        "nozzle_temperature_initial_layer",
                        gcode::fmt((first - old + v).clamp(0.0, 500.0), 0),
                    ),
                ]
            }
            Test::FlowCoarse | Test::FlowFine => vec![ch(
                "filament_flow_ratio",
                gcode::fmt(flow_after(flow_ratio(filament), v as i32), 4),
            )],
            Test::PressureAdvance => vec![
                ch("pressure_advance", gcode::fmt(v, 4)),
                ch("enable_pressure_advance", "1".into()),
            ],
            Test::Retraction => vec![ch("filament_retraction_length", gcode::fmt(v, 2))],
            Test::VolumetricSpeed => vec![ch("filament_max_volumetric_speed", gcode::fmt(v, 1))],
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

    #[test]
    fn results_become_filament_changes() {
        let m = machine();
        let f = filament();
        let temp = Calibration::new(Test::Temperature, 230.0, 190.0, 5.0, &m).unwrap();
        let ch = temp.choices(&f);
        assert_eq!(ch[0], ("230".into(), "block 1 · 230 °C".into()));
        let c = temp.changes("210", &f).unwrap();
        assert_eq!(c[0].to_string(), "nozzle_temperature = 210");
        assert_eq!(c[1].to_string(), "nozzle_temperature_initial_layer = 215");
        assert!(temp.changes("211", &f).is_err());
        assert!(temp.changes("+5; G28", &f).is_err());

        let coarse = Calibration::new(Test::FlowCoarse, 0.0, 0.0, 0.0, &m).unwrap();
        let ch = coarse.choices(&f);
        assert_eq!(ch[5], ("+5".into(), "+5 → flow ratio 1.029".into()));
        assert_eq!(coarse.changes("+5", &f).unwrap()[0].value, "1.029");
        assert_eq!(coarse.changes("-20", &f).unwrap()[0].value, "0.784");

        let mut f2 = f.clone();
        apply_changes(&mut f2, &coarse.changes("+5", &f).unwrap());
        let fine = Calibration::new(Test::FlowFine, 0.0, 0.0, 0.0, &m).unwrap();
        assert_eq!(fine.changes("-3", &f2).unwrap()[0].value, "0.9981");

        let pa = Calibration::new(Test::PressureAdvance, 0.0, 0.1, 0.002, &m).unwrap();
        assert_eq!(pa.choices(&f)[12].1, "12–13 mm · 0.024");
        let c = pa.changes("0.024", &f).unwrap();
        assert_eq!(c.len(), 2);
        let mut f3 = f.clone();
        apply_changes(&mut f3, &c);
        assert_eq!(f3["pressure_advance"], json!(["0.024"]));

        let r = Calibration::new(Test::Retraction, 0.0, 2.0, 0.1, &m).unwrap();
        assert_eq!(r.choices(&f)[3].1, "3.4–4.4 mm · 0.3 mm");
        let mut f4 = f.clone();
        apply_changes(&mut f4, &r.changes("0.3", &f).unwrap());
        assert_eq!(f4["filament_retraction_length"], json!(["0.3"]));

        // Unknown keys never reach a profile.
        let mut f5 = f.clone();
        apply_changes(
            &mut f5,
            &[Change {
                key: "filament_start_gcode".into(),
                value: "G28".into(),
            }],
        );
        assert!(!f5.contains_key("filament_start_gcode"));
    }

    #[test]
    fn plans_match_the_models() {
        let m = machine();
        let p = obj(json!({"initial_layer_print_height": "0.2"}));
        let f = filament();
        let c = Calibration::new(Test::FlowCoarse, 0.0, 0.0, 0.0, &m).unwrap();
        let Plan::Flow {
            cells,
            factors,
            center,
        } = c.plan(&m, &p, &f)
        else {
            panic!()
        };
        assert_eq!(cells.len(), 9);
        assert_eq!(factors[0], 0.8);
        assert_eq!(center, (34.0, 44.5));
        let v = Calibration::new(Test::VolumetricSpeed, 5.0, 20.0, 0.5, &m).unwrap();
        let Plan::Speed { area, values, .. } = v.plan(&m, &p, &f) else {
            panic!()
        };
        assert!((area - 2.405).abs() < 1e-3);
        assert_eq!(values.len(), 31);
    }
}
