//! The Slice form's overrides (PLAN §15.4), applied to a copy of the process profile.
//!
//! Each override is a typed field with a range check, written to a fixed Orca config key below.
//! There is no way to set any other key: the form can't name one, and [`SliceOptions`] can't
//! hold one. Keys and values are Orca 2.4.2's (`libslic3r/PrintConfig.cpp`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Profile;
use super::client::WorkerOptions;

pub const LAYER_HEIGHT: (f64, f64) = (0.04, 1.0);
pub const WALLS: (u8, u8) = (1, 20);
pub const BRIM_WIDTH: (f64, f64) = (0.5, 30.0);
pub const PLATES: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Supports {
    Off,
    Normal,
    Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Brim {
    Auto,
    Off,
    /// An outer brim this many millimetres wide.
    Width(f64),
}

/// What a job was asked for. `None` means "as the process profile has it".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SliceOptions {
    pub layer_height: Option<f64>,
    pub infill: Option<u8>,
    pub walls: Option<u8>,
    pub supports: Option<Supports>,
    pub brim: Option<Brim>,
    pub arrange: bool,
    pub orient: bool,
    /// 3MF only: the project's own process and filament settings.
    pub project_settings: bool,
    /// 3MF only: which plate.
    pub plate: u32,
    /// The G-code's file name on the printer (from [`output_name`], decided at submit).
    pub output: Option<String>,
}

impl Default for SliceOptions {
    fn default() -> Self {
        Self {
            layer_height: None,
            infill: None,
            walls: None,
            supports: None,
            brim: None,
            arrange: false,
            orient: false,
            project_settings: false,
            plate: 1,
            output: None,
        }
    }
}

/// The form's raw fields (all text; blank means "keep the profile's").
#[derive(Debug, Default, Clone, Deserialize)]
pub struct OptionsForm {
    #[serde(default)]
    pub layer_height: String,
    #[serde(default)]
    pub infill: String,
    #[serde(default)]
    pub walls: String,
    #[serde(default)]
    pub supports: String,
    #[serde(default)]
    pub brim: String,
    #[serde(default)]
    pub brim_width: String,
    #[serde(default)]
    pub arrange: Option<String>,
    #[serde(default)]
    pub orient: Option<String>,
    #[serde(default)]
    pub project_settings: Option<String>,
    #[serde(default)]
    pub plate: String,
}

impl OptionsForm {
    /// Sets a field by its form name (multipart forms arrive field by field). Unknown names are
    /// ignored, so they can't reach the profile.
    pub fn set(&mut self, name: &str, value: String) {
        match name {
            "layer_height" => self.layer_height = value,
            "infill" => self.infill = value,
            "walls" => self.walls = value,
            "supports" => self.supports = value,
            "brim" => self.brim = value,
            "brim_width" => self.brim_width = value,
            "arrange" => self.arrange = Some(value),
            "orient" => self.orient = Some(value),
            "project_settings" => self.project_settings = Some(value),
            "plate" => self.plate = value,
            _ => {}
        }
    }

    /// Validates the fields. `is_3mf` decides whether project settings and plates apply.
    pub fn parse(&self, is_3mf: bool) -> Result<SliceOptions, String> {
        let layer_height = number::<f64>(&self.layer_height, "Layer height")?
            .map(|v| in_range(v, LAYER_HEIGHT, "Layer height", "mm"))
            .transpose()?;
        let infill = number::<u8>(&self.infill, "Infill")?
            .map(|v| {
                if v <= 100 {
                    Ok(v)
                } else {
                    Err("Infill must be 0–100 %.".to_string())
                }
            })
            .transpose()?;
        let walls = number::<u8>(&self.walls, "Walls")?
            .map(|v| {
                if (WALLS.0..=WALLS.1).contains(&v) {
                    Ok(v)
                } else {
                    Err(format!("Walls must be {}–{}.", WALLS.0, WALLS.1))
                }
            })
            .transpose()?;
        let supports = match self.supports.trim() {
            "" => None,
            "off" => Some(Supports::Off),
            "normal" => Some(Supports::Normal),
            "tree" => Some(Supports::Tree),
            _ => return Err("Supports must be off, normal or tree.".into()),
        };
        let brim = match self.brim.trim() {
            "" => None,
            "auto" => Some(Brim::Auto),
            "off" => Some(Brim::Off),
            "width" => {
                let w = number::<f64>(&self.brim_width, "Brim width")?
                    .ok_or_else(|| "Give the brim's width.".to_string())?;
                Some(Brim::Width(in_range(w, BRIM_WIDTH, "Brim width", "mm")?))
            }
            _ => return Err("Brim must be auto, off or a width.".into()),
        };
        let project_settings = is_3mf && self.project_settings.is_some();
        let plate = if is_3mf {
            number::<u32>(&self.plate, "Plate")?.unwrap_or(1)
        } else {
            1
        };
        if !(1..=PLATES).contains(&plate) {
            return Err(format!("Plate must be 1–{PLATES}."));
        }
        Ok(SliceOptions {
            layer_height,
            infill,
            walls,
            supports,
            brim,
            arrange: self.arrange.is_some(),
            orient: self.orient.is_some(),
            project_settings,
            plate,
            output: None,
        })
    }
}

fn number<T: std::str::FromStr>(raw: &str, what: &str) -> Result<Option<T>, String> {
    let raw = raw.trim().trim_end_matches('%').trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse()
        .map(Some)
        .map_err(|_| format!("{what} must be a number."))
}

fn in_range(v: f64, (lo, hi): (f64, f64), what: &str, unit: &str) -> Result<f64, String> {
    if v.is_finite() && (lo..=hi).contains(&v) {
        Ok(v)
    } else {
        Err(format!("{what} must be {lo}–{hi} {unit}."))
    }
}

/// A float as Orca writes it: `0.2`, `0.16`, `5`.
fn decimal(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// The first number of an Orca setting (they are often one-per-extruder arrays of strings).
fn setting_f64(p: &Profile, key: &str) -> Option<f64> {
    let v = match p.get(key)? {
        Value::Array(a) => a.first()?,
        v => v,
    };
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

impl SliceOptions {
    /// Writes the overrides into `process`. The layer height is also checked against the
    /// machine's own limits when it has them.
    pub fn apply(&self, process: &mut Profile, machine: &Profile) -> Result<(), String> {
        let mut set = |k: &str, v: String| {
            process.insert(k.to_string(), Value::String(v));
        };
        if let Some(h) = self.layer_height {
            let lo = setting_f64(machine, "min_layer_height").filter(|v| *v > 0.0);
            let hi = setting_f64(machine, "max_layer_height").filter(|v| *v > 0.0);
            if lo.is_some_and(|lo| h < lo - 1e-9) || hi.is_some_and(|hi| h > hi + 1e-9) {
                return Err(format!(
                    "This printer takes layer heights of {}–{} mm.",
                    lo.map_or("?".into(), decimal),
                    hi.map_or("?".into(), decimal)
                ));
            }
            set("layer_height", decimal(h));
        }
        if let Some(i) = self.infill {
            set("sparse_infill_density", format!("{i}%"));
        }
        if let Some(w) = self.walls {
            set("wall_loops", w.to_string());
        }
        match self.supports {
            None => {}
            Some(Supports::Off) => set("enable_support", "0".into()),
            Some(s) => {
                set("enable_support", "1".into());
                set(
                    "support_type",
                    if s == Supports::Tree {
                        "tree(auto)"
                    } else {
                        "normal(auto)"
                    }
                    .into(),
                );
                set("support_on_build_plate_only", "1".into());
            }
        }
        match self.brim {
            None => {}
            Some(Brim::Auto) => set("brim_type", "auto_brim".into()),
            Some(Brim::Off) => set("brim_type", "no_brim".into()),
            Some(Brim::Width(w)) => {
                set("brim_type", "outer_only".into());
                set("brim_width", decimal(w));
            }
        }
        Ok(())
    }

    /// The CLI switches for the worker.
    pub fn worker(&self) -> WorkerOptions {
        WorkerOptions {
            arrange: self.arrange.then_some(true),
            orient: self.orient,
            plate: self.plate,
            project_settings: self.project_settings,
        }
    }

    /// Short labels for the job panel: `0.16 mm layers`, `20 % infill`, …
    pub fn summary(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.project_settings {
            out.push("project settings".into());
        }
        if let Some(h) = self.layer_height {
            out.push(format!("{} mm layers", decimal(h)));
        }
        if let Some(i) = self.infill {
            out.push(format!("{i} % infill"));
        }
        if let Some(w) = self.walls {
            out.push(format!("{w} walls"));
        }
        match self.supports {
            Some(Supports::Off) => out.push("no supports".into()),
            Some(Supports::Normal) => out.push("supports".into()),
            Some(Supports::Tree) => out.push("tree supports".into()),
            None => {}
        }
        match self.brim {
            Some(Brim::Auto) => out.push("auto brim".into()),
            Some(Brim::Off) => out.push("no brim".into()),
            Some(Brim::Width(w)) => out.push(format!("{} mm brim", decimal(w))),
            None => {}
        }
        if self.arrange {
            out.push("arranged".into());
        }
        if self.orient {
            out.push("oriented".into());
        }
        if self.plate > 1 {
            out.push(format!("plate {}", self.plate));
        }
        out
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    /// Stored options; anything unreadable falls back to the defaults.
    pub fn from_json(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    /// The form fields that reproduce these options (to prefill a reslice).
    pub fn to_form(&self) -> OptionsForm {
        OptionsForm {
            layer_height: self.layer_height.map(decimal).unwrap_or_default(),
            infill: self.infill.map(|v| v.to_string()).unwrap_or_default(),
            walls: self.walls.map(|v| v.to_string()).unwrap_or_default(),
            supports: match self.supports {
                Some(Supports::Off) => "off",
                Some(Supports::Normal) => "normal",
                Some(Supports::Tree) => "tree",
                None => "",
            }
            .into(),
            brim: match self.brim {
                Some(Brim::Auto) => "auto",
                Some(Brim::Off) => "off",
                Some(Brim::Width(_)) => "width",
                None => "",
            }
            .into(),
            brim_width: match self.brim {
                Some(Brim::Width(w)) => decimal(w),
                _ => String::new(),
            },
            arrange: self.arrange.then(|| "on".into()),
            orient: self.orient.then(|| "on".into()),
            project_settings: self.project_settings.then(|| "on".into()),
            plate: if self.plate > 1 {
                self.plate.to_string()
            } else {
                String::new()
            },
        }
    }
}

/// The G-code's name on the printer: `<model>_<process>_<filament>.gcode`, or the user's
/// choice, reduced to `[A-Za-z0-9._-]` so it's safe in any path and URL.
pub fn output_name(custom: &str, model: &str, process: &str, filament: &str) -> Option<String> {
    let custom = custom.trim().trim_end_matches(".gcode").trim();
    let stem = |s: &str| s.rsplit_once('.').map_or(s, |(a, _)| a).to_string();
    let raw = if custom.is_empty() {
        [stem(model), process.to_string(), filament.to_string()]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("_")
    } else {
        custom.to_string()
    };
    let mut out = String::new();
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
            c
        } else {
            '_'
        };
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    let out: String = out
        .trim_matches(|c| c == '_' || c == '.')
        .chars()
        .take(120)
        .collect();
    let out = out.trim_end_matches(['_', '.']);
    (!out.is_empty()).then(|| format!("{out}.gcode"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form(pairs: &[(&str, &str)]) -> OptionsForm {
        let mut f = OptionsForm::default();
        for (k, v) in pairs {
            f.set(k, v.to_string());
        }
        f
    }

    fn obj(v: Value) -> Profile {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn blank_form_changes_nothing() {
        let o = form(&[]).parse(false).unwrap();
        assert_eq!(o, SliceOptions::default());
        let mut p = obj(json!({"layer_height": "0.2"}));
        o.apply(&mut p, &Profile::new()).unwrap();
        assert_eq!(p, obj(json!({"layer_height": "0.2"})));
        assert!(o.summary().is_empty());
    }

    #[test]
    fn overrides_map_to_orca_keys() {
        let o = form(&[
            ("layer_height", "0.16"),
            ("infill", "35%"),
            ("walls", "4"),
            ("supports", "tree"),
            ("brim", "width"),
            ("brim_width", "6.5"),
            ("arrange", "on"),
            ("orient", "on"),
        ])
        .parse(false)
        .unwrap();
        let mut p = obj(json!({"layer_height": "0.2", "enable_support": "0"}));
        let machine = obj(json!({"min_layer_height": ["0.08"], "max_layer_height": ["0.32"]}));
        o.apply(&mut p, &machine).unwrap();
        assert_eq!(
            p,
            obj(json!({
                "layer_height": "0.16",
                "sparse_infill_density": "35%",
                "wall_loops": "4",
                "enable_support": "1",
                "support_type": "tree(auto)",
                "support_on_build_plate_only": "1",
                "brim_type": "outer_only",
                "brim_width": "6.5",
            }))
        );
        assert_eq!(
            o.summary(),
            [
                "0.16 mm layers",
                "35 % infill",
                "4 walls",
                "tree supports",
                "6.5 mm brim",
                "arranged",
                "oriented"
            ]
        );
        let w = o.worker();
        assert_eq!(
            (w.arrange, w.orient, w.plate, w.project_settings),
            (Some(true), true, 1, false)
        );
        // Round trip through the DB and back into a form.
        let back = SliceOptions::from_json(&o.to_json());
        assert_eq!(back, o);
        assert_eq!(back.to_form().parse(false).unwrap(), o);
    }

    #[test]
    fn supports_off_and_brims() {
        let mut p = Profile::new();
        let o = form(&[("supports", "off"), ("brim", "off")])
            .parse(false)
            .unwrap();
        o.apply(&mut p, &Profile::new()).unwrap();
        assert_eq!(p, obj(json!({"enable_support": "0", "brim_type": "no_brim"})));
        let mut p = Profile::new();
        let o = form(&[("supports", "normal"), ("brim", "auto")])
            .parse(false)
            .unwrap();
        o.apply(&mut p, &Profile::new()).unwrap();
        assert_eq!(p["support_type"], "normal(auto)");
        assert_eq!(p["brim_type"], "auto_brim");
    }

    #[test]
    fn range_checks() {
        for bad in [
            [("layer_height", "0.01")],
            [("layer_height", "abc")],
            [("layer_height", "NaN")],
            [("infill", "101")],
            [("infill", "-5")],
            [("walls", "0")],
            [("walls", "21")],
            [("supports", "everywhere")],
            [("brim", "huge")],
            [("brim", "width")],
        ] {
            assert!(form(&bad).parse(false).is_err(), "{bad:?}");
        }
        assert!(
            form(&[("brim", "width"), ("brim_width", "50")])
                .parse(false)
                .is_err()
        );
        // The machine's own limits.
        let o = form(&[("layer_height", "0.4")]).parse(false).unwrap();
        let machine = obj(json!({"max_layer_height": ["0.32"]}));
        let err = o.apply(&mut Profile::new(), &machine).unwrap_err();
        assert!(err.contains("0.32"), "{err}");
    }

    #[test]
    fn three_mf_only_options() {
        let f = form(&[("project_settings", "on"), ("plate", "3")]);
        let stl = f.parse(false).unwrap();
        assert!(!stl.project_settings);
        assert_eq!(stl.plate, 1);
        let mf = f.parse(true).unwrap();
        assert!(mf.project_settings);
        assert_eq!(mf.plate, 3);
        assert!(form(&[("plate", "0")]).parse(true).is_err());
        assert!(form(&[("plate", "65")]).parse(true).is_err());
    }

    #[test]
    fn unknown_fields_go_nowhere() {
        let f = form(&[("machine_start_gcode", "M112"), ("layer_height", "0.2")]);
        let o = f.parse(false).unwrap();
        let mut p = Profile::new();
        o.apply(&mut p, &Profile::new()).unwrap();
        assert_eq!(p.keys().collect::<Vec<_>>(), ["layer_height"]);
        assert!(serde_json::from_str::<SliceOptions>(r#"{"machine_start_gcode": "M112"}"#).is_err());
        assert_eq!(SliceOptions::from_json("not json"), SliceOptions::default());
    }

    #[test]
    fn output_names() {
        assert_eq!(
            output_name(
                "",
                "Benchy v2.STL",
                "0.20mm Standard @Voron",
                "Generic PLA @System"
            )
            .as_deref(),
            Some("Benchy_v2_0.20mm_Standard_Voron_Generic_PLA_System.gcode")
        );
        assert_eq!(
            output_name("  my part.gcode ", "x.stl", "p", "f").as_deref(),
            Some("my_part.gcode")
        );
        assert_eq!(
            output_name("../../etc/passwd", "x.stl", "p", "f").as_deref(),
            Some("etc_passwd.gcode")
        );
        assert_eq!(output_name("___", "", "", ""), None);
        assert!(output_name(&"x".repeat(500), "", "", "").unwrap().len() <= 126);
    }
}
