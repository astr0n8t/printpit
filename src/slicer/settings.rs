//! The settings an admin can edit on a library profile (the `[s]Slicer` tab's "settings" panel).
//!
//! Like the Slice form's overrides ([`super::options`]), this is a fixed list of Orca keys with a
//! type and a range each: the form can't name any other key, and values are checked before they
//! are written. Start and end G-code stay out on purpose. Keys, types and values are Orca 2.5.1's
//! (`libslic3r/PrintConfig.cpp`). A value keeps the shape it already has in the profile (Orca reads
//! `"200"` and `["200"]` alike for its per-extruder lists); a key the profile lacks gets the shape
//! Orca declares for it.

use serde_json::Value;

use super::{Kind, Profile};

/// Orca's build plates (`curr_bed_type`), in its order, with the GUI's labels.
pub const BED_TYPES: &[(&str, &str)] = &[
    ("Cool Plate", "Smooth Cool Plate"),
    ("Engineering Plate", "Engineering Plate"),
    ("High Temp Plate", "Smooth High Temp Plate"),
    ("Textured PEI Plate", "Textured PEI Plate"),
    ("Textured Cool Plate", "Textured Cool Plate"),
    ("Supertack Plate", "Cool Plate (SuperTack)"),
];

/// What the worker uses when neither the profile nor the printer model names a plate (Orca's GUI
/// default; `slicer-worker` `bed::FALLBACK`).
pub const BED_FALLBACK: &str = "High Temp Plate";

/// A plate as Orca names it, from its name or its legacy number (`"4"`).
pub fn bed_type(raw: &str) -> Option<&'static str> {
    let raw = raw.trim();
    if let Ok(n) = raw.parse::<usize>() {
        return n.checked_sub(1).and_then(|i| BED_TYPES.get(i)).map(|b| b.0);
    }
    BED_TYPES
        .iter()
        .find(|b| b.0.eq_ignore_ascii_case(raw))
        .map(|b| b.0)
}

pub fn bed_label(name: &str) -> &'static str {
    BED_TYPES.iter().find(|b| b.0 == name).map_or("", |b| b.1)
}

#[derive(Debug, Clone, Copy)]
pub enum Input {
    /// A number; `int` ones are written without a decimal point. `percent` ones as `15%`.
    Number {
        min: f64,
        max: f64,
        step: &'static str,
        int: bool,
        percent: bool,
    },
    /// One of Orca's values. `clear`, when set, is the label of a blank choice that removes the
    /// key from the profile (so a default elsewhere applies).
    Choice {
        options: &'static [(&'static str, &'static str)],
        clear: Option<&'static str>,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub input: Input,
    /// Orca declares it a list (`coFloats`, `coInts`, …): one value per extruder or filament.
    pub vector: bool,
}

pub struct Section {
    pub title: &'static str,
    pub fields: &'static [Field],
}

const ON_OFF: &[(&str, &str)] = &[("1", "on"), ("0", "off")];

const fn num(key: &'static str, label: &'static str, unit: &'static str, min: f64, max: f64) -> Field {
    Field {
        key,
        label,
        unit,
        input: Input::Number {
            min,
            max,
            step: "any",
            int: false,
            percent: false,
        },
        vector: false,
    }
}

const fn int(key: &'static str, label: &'static str, unit: &'static str, min: f64, max: f64) -> Field {
    Field {
        input: Input::Number {
            min,
            max,
            step: "1",
            int: true,
            percent: false,
        },
        ..num(key, label, unit, min, max)
    }
}

const fn choice(
    key: &'static str,
    label: &'static str,
    options: &'static [(&'static str, &'static str)],
) -> Field {
    Field {
        input: Input::Choice { options, clear: None },
        ..num(key, label, "", 0.0, 0.0)
    }
}

/// Marks a field as one of Orca's per-extruder/per-filament lists.
const fn list(f: Field) -> Field {
    Field { vector: true, ..f }
}

const fn plate_temp(key: &'static str, label: &'static str) -> Field {
    list(int(key, label, "°C", 0.0, 300.0))
}

const MACHINE: &[Section] = &[
    Section {
        title: "build plate",
        fields: &[Field {
            input: Input::Choice {
                options: BED_TYPES,
                clear: Some("printer default"),
            },
            ..choice("curr_bed_type", "plate", BED_TYPES)
        }],
    },
    Section {
        title: "extruder",
        fields: &[
            num("z_offset", "z offset", "mm", -10.0, 10.0),
            list(num("retraction_length", "retraction length", "mm", 0.0, 20.0)),
            list(num("retraction_speed", "retraction speed", "mm/s", 1.0, 300.0)),
            list(num(
                "deretraction_speed",
                "deretraction speed (0 = same)",
                "mm/s",
                0.0,
                300.0,
            )),
            list(num("z_hop", "z hop", "mm", 0.0, 5.0)),
        ],
    },
];

const INFILL_PATTERNS: &[(&str, &str)] = &[
    ("rectilinear", "rectilinear"),
    ("alignedrectilinear", "aligned rectilinear"),
    ("zigzag", "zig zag"),
    ("crosszag", "cross zag"),
    ("lockedzag", "locked zag"),
    ("line", "line"),
    ("grid", "grid"),
    ("triangles", "triangles"),
    ("tri-hexagon", "tri-hexagon"),
    ("cubic", "cubic"),
    ("adaptivecubic", "adaptive cubic"),
    ("quartercubic", "quarter cubic"),
    ("supportcubic", "support cubic"),
    ("lightning", "lightning"),
    ("honeycomb", "honeycomb"),
    ("3dhoneycomb", "3D honeycomb"),
    ("lateral-honeycomb", "lateral honeycomb"),
    ("lateral-lattice", "lateral lattice"),
    ("crosshatch", "cross hatch"),
    ("tpmsd", "TPMS-D"),
    ("tpmsfk", "TPMS-FK"),
    ("gyroid", "gyroid"),
    ("concentric", "concentric"),
    ("hilbertcurve", "hilbert curve"),
    ("archimedeanchords", "archimedean chords"),
    ("octagramspiral", "octagram spiral"),
];

const SUPPORT_TYPES: &[(&str, &str)] = &[
    ("normal(auto)", "normal (auto)"),
    ("tree(auto)", "tree (auto)"),
    ("normal(manual)", "normal (manual)"),
    ("tree(manual)", "tree (manual)"),
];

const BRIM_TYPES: &[(&str, &str)] = &[
    ("auto_brim", "auto"),
    ("brim_ears", "mouse ears"),
    ("painted", "painted"),
    ("outer_only", "outer only"),
    ("inner_only", "inner only"),
    ("outer_and_inner", "outer and inner"),
    ("leading_edge_only", "leading edge only"),
    ("no_brim", "no brim"),
];

const SEAMS: &[(&str, &str)] = &[
    ("nearest", "nearest"),
    ("aligned", "aligned"),
    ("aligned_back", "aligned back"),
    ("back", "back"),
    ("random", "random"),
];

const IRONING: &[(&str, &str)] = &[
    ("no ironing", "no ironing"),
    ("top", "all top surfaces"),
    ("topmost", "topmost surface only"),
    ("solid", "all solid layers"),
];

const PROCESS: &[Section] = &[
    Section {
        title: "layers and walls",
        fields: &[
            num("layer_height", "layer height", "mm", 0.04, 1.0),
            num(
                "initial_layer_print_height",
                "first layer height",
                "mm",
                0.04,
                1.0,
            ),
            int("wall_loops", "walls", "", 1.0, 20.0),
            int("top_shell_layers", "top layers", "", 0.0, 100.0),
            int("bottom_shell_layers", "bottom layers", "", 0.0, 100.0),
            choice("seam_position", "seam", SEAMS),
        ],
    },
    Section {
        title: "infill",
        fields: &[
            Field {
                input: Input::Number {
                    min: 0.0,
                    max: 100.0,
                    step: "1",
                    int: true,
                    percent: true,
                },
                ..num("sparse_infill_density", "infill density", "%", 0.0, 100.0)
            },
            choice("sparse_infill_pattern", "infill pattern", INFILL_PATTERNS),
            choice("ironing_type", "ironing", IRONING),
        ],
    },
    Section {
        title: "support and adhesion",
        fields: &[
            choice("enable_support", "supports", ON_OFF),
            choice("support_type", "support type", SUPPORT_TYPES),
            choice("support_on_build_plate_only", "on build plate only", ON_OFF),
            choice("brim_type", "brim", BRIM_TYPES),
            num("brim_width", "brim width", "mm", 0.0, 100.0),
            int("skirt_loops", "skirt loops", "", 0.0, 20.0),
        ],
    },
    Section {
        title: "speed",
        fields: &[
            list(num("outer_wall_speed", "outer wall", "mm/s", 1.0, 1000.0)),
            list(num("inner_wall_speed", "inner wall", "mm/s", 1.0, 1000.0)),
            list(num("sparse_infill_speed", "sparse infill", "mm/s", 1.0, 1000.0)),
            list(num("travel_speed", "travel", "mm/s", 1.0, 2000.0)),
            list(num(
                "default_acceleration",
                "default acceleration",
                "mm/s²",
                0.0,
                100000.0,
            )),
        ],
    },
];

const FILAMENT: &[Section] = &[
    Section {
        title: "nozzle",
        fields: &[
            list(int(
                "nozzle_temperature_initial_layer",
                "first layer",
                "°C",
                0.0,
                500.0,
            )),
            list(int("nozzle_temperature", "other layers", "°C", 0.0, 500.0)),
        ],
    },
    Section {
        title: "bed (by plate; 0 = not for this plate)",
        fields: &[
            plate_temp("cool_plate_temp_initial_layer", "smooth cool · first layer"),
            plate_temp("cool_plate_temp", "smooth cool · other layers"),
            plate_temp("eng_plate_temp_initial_layer", "engineering · first layer"),
            plate_temp("eng_plate_temp", "engineering · other layers"),
            plate_temp("hot_plate_temp_initial_layer", "smooth high temp · first layer"),
            plate_temp("hot_plate_temp", "smooth high temp · other layers"),
            plate_temp("textured_plate_temp_initial_layer", "textured PEI · first layer"),
            plate_temp("textured_plate_temp", "textured PEI · other layers"),
            plate_temp(
                "textured_cool_plate_temp_initial_layer",
                "textured cool · first layer",
            ),
            plate_temp("textured_cool_plate_temp", "textured cool · other layers"),
            plate_temp("supertack_plate_temp_initial_layer", "supertack · first layer"),
            plate_temp("supertack_plate_temp", "supertack · other layers"),
        ],
    },
    Section {
        title: "flow and cooling",
        fields: &[
            list(num("filament_flow_ratio", "flow ratio", "", 0.5, 2.0)),
            list(choice("enable_pressure_advance", "pressure advance", ON_OFF)),
            list(num("pressure_advance", "pressure advance value", "", 0.0, 2.0)),
            list(num(
                "filament_max_volumetric_speed",
                "max volumetric speed",
                "mm³/s",
                0.0,
                200.0,
            )),
            list(num("fan_min_speed", "fan min", "%", 0.0, 100.0)),
            list(num("fan_max_speed", "fan max", "%", 0.0, 100.0)),
            list(int(
                "close_fan_the_first_x_layers",
                "no fan for the first",
                "layers",
                0.0,
                1000.0,
            )),
        ],
    },
    Section {
        title: "material",
        fields: &[
            list(num("filament_density", "density", "g/cm³", 0.1, 30.0)),
            list(num("filament_cost", "cost", "per kg", 0.0, 100000.0)),
        ],
    },
];

pub fn sections(kind: Kind) -> &'static [Section] {
    match kind {
        Kind::Machine => MACHINE,
        Kind::Process => PROCESS,
        Kind::Filament => FILAMENT,
    }
}

pub fn field(kind: Kind, key: &str) -> Option<&'static Field> {
    sections(kind)
        .iter()
        .flat_map(|s| s.fields)
        .find(|f| f.key == key)
}

/// A setting as text: the string, or the first element of a list. Empty when unset.
pub fn current(p: &Profile, key: &str) -> String {
    let v = match p.get(key) {
        Some(Value::Array(a)) => a.first(),
        v => v,
    };
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => if *b { "1" } else { "0" }.into(),
        _ => String::new(),
    }
}

/// A number as Orca writes it: `0.2`, `0.025`, `5`.
fn decimal(v: f64) -> String {
    let s = format!("{v:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn same_number(a: &str, b: f64) -> bool {
    a.trim_end_matches('%')
        .trim()
        .parse::<f64>()
        .is_ok_and(|a| (a - b).abs() < 1e-9)
}

/// One change [`apply`] made, for the audit log and the notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub key: &'static str,
    pub from: String,
    pub to: String,
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let show = |s: &str| {
            if s.is_empty() {
                "unset".to_string()
            } else {
                s.to_string()
            }
        };
        write!(f, "{} {}→{}", self.key, show(&self.from), show(&self.to))
    }
}

/// Applies a submitted form (`key → text`) to `profile`. Only the kind's fields are read; a blank
/// number or an unchanged value leaves the setting as it is. All fields are checked before
/// anything is written, so an error changes nothing.
pub fn apply(kind: Kind, profile: &mut Profile, form: &[(String, String)]) -> Result<Vec<Change>, String> {
    let get = |key: &str| {
        form.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim().to_string())
    };
    let mut writes: Vec<(&Field, Option<String>, Change)> = Vec::new();
    for f in sections(kind).iter().flat_map(|s| s.fields) {
        let Some(raw) = get(f.key) else { continue };
        let now = current(profile, f.key);
        let new = match f.input {
            Input::Number {
                min,
                max,
                int,
                percent,
                ..
            } => {
                if raw.is_empty() {
                    continue;
                }
                let v: f64 = raw
                    .trim_end_matches('%')
                    .trim()
                    .parse()
                    .ok()
                    .filter(|v: &f64| v.is_finite())
                    .ok_or_else(|| format!("{} must be a number.", f.label))?;
                if !(min..=max).contains(&v) || (int && v.fract() != 0.0) {
                    let what = if int { "a whole number" } else { "a number" };
                    return Err(format!(
                        "{} must be {what} from {} to {}{}.",
                        f.label,
                        decimal(min),
                        decimal(max),
                        if f.unit.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", f.unit)
                        }
                    ));
                }
                if same_number(&now, v) {
                    continue;
                }
                let text = if int { format!("{}", v as i64) } else { decimal(v) };
                Some(if percent { format!("{text}%") } else { text })
            }
            Input::Choice { options, clear } => {
                if raw.is_empty() {
                    if clear.is_none() || !profile.contains_key(f.key) {
                        continue;
                    }
                    None
                } else {
                    let v = options
                        .iter()
                        .find(|o| o.0 == raw)
                        .map(|o| o.0)
                        .ok_or_else(|| format!("{} isn't one of Orca's choices.", f.label))?;
                    if now == v {
                        continue;
                    }
                    Some(v.to_string())
                }
            }
        };
        let change = Change {
            key: f.key,
            from: now,
            to: new.clone().unwrap_or_default(),
        };
        writes.push((f, new, change));
    }
    let mut changes = Vec::new();
    for (f, new, change) in writes {
        match new {
            None => {
                profile.remove(f.key);
            }
            Some(v) => {
                let value = match profile.get(f.key) {
                    // Every extruder (or filament slot) gets the value.
                    Some(Value::Array(a)) if !a.is_empty() => Value::Array(vec![Value::String(v); a.len()]),
                    Some(Value::Array(_)) => Value::Array(vec![Value::String(v)]),
                    Some(_) => Value::String(v),
                    None if f.vector => Value::Array(vec![Value::String(v)]),
                    None => Value::String(v),
                };
                profile.insert(f.key.to_string(), value);
            }
        }
        changes.push(change);
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Profile {
        v.as_object().unwrap().clone()
    }

    fn form(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn keys_are_unique_per_kind() {
        for kind in Kind::ALL {
            let mut keys: Vec<_> = sections(kind)
                .iter()
                .flat_map(|s| s.fields)
                .map(|f| f.key)
                .collect();
            let n = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), n, "{kind:?}");
        }
    }

    #[test]
    fn bed_types() {
        assert_eq!(bed_type("4"), Some("Textured PEI Plate"));
        assert_eq!(bed_type("textured pei plate"), Some("Textured PEI Plate"));
        assert_eq!(bed_type("0"), None);
        assert_eq!(bed_type("Glass"), None);
        assert_eq!(bed_label("High Temp Plate"), "Smooth High Temp Plate");
    }

    #[test]
    fn writes_keep_the_shape_and_skip_blanks() {
        let mut p = obj(json!({
            "wall_loops": "2",
            "retraction_length": ["0.8", "0.8"],
            "outer_wall_speed": "200",
            "sparse_infill_density": "15%",
        }));
        let changes = apply(
            Kind::Process,
            &mut p,
            &form(&[
                ("wall_loops", "3"),
                ("outer_wall_speed", "150"),
                ("sparse_infill_density", "15"),
                ("top_shell_layers", ""),
                ("seam_position", "back"),
                ("machine_start_gcode", "M112"),
            ]),
        )
        .unwrap();
        assert_eq!(
            changes.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "wall_loops 2→3",
                "seam_position unset→back",
                "outer_wall_speed 200→150"
            ]
        );
        assert_eq!(p["wall_loops"], "3");
        assert_eq!(p["outer_wall_speed"], "150");
        assert_eq!(p["sparse_infill_density"], "15%");
        assert_eq!(p["seam_position"], "back");
        assert!(!p.contains_key("top_shell_layers"));
        assert!(!p.contains_key("machine_start_gcode"));

        let mut m = obj(json!({"retraction_length": ["0.8", "0.8"]}));
        apply(
            Kind::Machine,
            &mut m,
            &form(&[("retraction_length", "1.25"), ("z_hop", "0.4")]),
        )
        .unwrap();
        assert_eq!(m["retraction_length"], json!(["1.25", "1.25"]), "every extruder");
        assert_eq!(m["z_hop"], json!(["0.4"]), "a new list key is a list");

        let mut f = Profile::new();
        apply(
            Kind::Filament,
            &mut f,
            &form(&[("textured_plate_temp", "65"), ("enable_pressure_advance", "1")]),
        )
        .unwrap();
        assert_eq!(f["textured_plate_temp"], json!(["65"]));
        assert_eq!(f["enable_pressure_advance"], json!(["1"]));
    }

    #[test]
    fn the_plate_can_be_cleared() {
        let mut m = obj(json!({"curr_bed_type": "Cool Plate"}));
        let c = apply(
            Kind::Machine,
            &mut m,
            &form(&[("curr_bed_type", "Textured PEI Plate")]),
        )
        .unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(m["curr_bed_type"], "Textured PEI Plate");
        let c = apply(Kind::Machine, &mut m, &form(&[("curr_bed_type", "")])).unwrap();
        assert_eq!(c[0].to_string(), "curr_bed_type Textured PEI Plate→unset");
        assert!(!m.contains_key("curr_bed_type"));
        assert!(
            apply(Kind::Machine, &mut m, &form(&[("curr_bed_type", "")]))
                .unwrap()
                .is_empty()
        );
        // Other choices have no blank: blank leaves them alone.
        let mut p = obj(json!({"brim_type": "no_brim"}));
        assert!(
            apply(Kind::Process, &mut p, &form(&[("brim_type", "")]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(p["brim_type"], "no_brim");
    }

    #[test]
    fn bad_values_change_nothing() {
        for (k, v) in [
            ("wall_loops", "0"),
            ("wall_loops", "2.5"),
            ("layer_height", "abc"),
            ("layer_height", "NaN"),
            ("seam_position", "everywhere"),
            ("sparse_infill_density", "101"),
        ] {
            let mut p = obj(json!({"top_shell_layers": "4"}));
            let err = apply(Kind::Process, &mut p, &form(&[("top_shell_layers", "5"), (k, v)])).unwrap_err();
            assert!(!err.is_empty());
            assert_eq!(p["top_shell_layers"], "4", "{k}={v}: nothing written");
        }
    }

    #[test]
    fn current_reads_strings_and_lists() {
        let p = obj(json!({"a": ["220", "230"], "b": "15%", "c": 3, "d": []}));
        assert_eq!(current(&p, "a"), "220");
        assert_eq!(current(&p, "b"), "15%");
        assert_eq!(current(&p, "c"), "3");
        assert_eq!(current(&p, "d"), "");
        assert_eq!(current(&p, "missing"), "");
    }
}
