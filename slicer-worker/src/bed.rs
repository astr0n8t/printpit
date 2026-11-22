//! Which build plate a slice is for (Orca's `curr_bed_type`).
//!
//! The plate picks the bed temperatures: each filament has one pair per plate type
//! (`hot_plate_temp`, `textured_plate_temp_initial_layer`, …). Orca's GUI keeps the choice in its
//! app config, outside the presets, and starts from the printer's `default_bed_type`, else the
//! High Temp Plate. The CLI has neither and falls back to the Cool Plate, so a stock preset sliced
//! here would get the wrong bed temperature. [`choose`] does what the GUI does.

use serde_json::Value;

use crate::library::{Library, Profile, text};

/// Orca's names (`s_keys_map_BedType`), in `BedType` enum order from 1.
pub const NAMES: [&str; 6] = [
    "Cool Plate",
    "Engineering Plate",
    "High Temp Plate",
    "Textured PEI Plate",
    "Textured Cool Plate",
    "Supertack Plate",
];

/// The GUI's fallback (`Preset::get_default_bed_type`).
pub const FALLBACK: &str = "High Temp Plate";

/// A bed type as Orca's name, from its name or its legacy number (`"4"`); `None` for anything else,
/// including `"Default Plate"`.
pub fn normalize(raw: &str) -> Option<&'static str> {
    let raw = raw.trim();
    if let Ok(n) = raw.parse::<usize>() {
        return n.checked_sub(1).and_then(|i| NAMES.get(i)).copied();
    }
    NAMES.iter().find(|n| n.eq_ignore_ascii_case(raw)).copied()
}

fn setting(p: &Profile, key: &str) -> Option<&'static str> {
    normalize(&text(p, key))
}

/// The plate for a slice: the process's choice (printpit's per-job override), then the machine's,
/// then the machine's default, then its printer model's default, then [`FALLBACK`].
pub fn choose(machine: &Profile, process: Option<&Profile>, library: &Library) -> &'static str {
    process
        .and_then(|p| setting(p, "curr_bed_type"))
        .or_else(|| setting(machine, "curr_bed_type"))
        .or_else(|| setting(machine, "default_bed_type"))
        .or_else(|| library.model_bed_type(&text(machine, "printer_model")))
        .unwrap_or(FALLBACK)
}

/// Writes the plate into the profiles Orca loads. Both get it, so it holds whichever one Orca
/// reads last.
pub fn apply(bed: &str, machine: &mut Profile, process: Option<&mut Profile>) {
    machine.insert("curr_bed_type".into(), Value::String(bed.into()));
    if let Some(p) = process {
        p.insert("curr_bed_type".into(), Value::String(bed.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Profile {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn normalizes_names_and_numbers() {
        assert_eq!(normalize("Textured PEI Plate"), Some("Textured PEI Plate"));
        assert_eq!(normalize(" textured pei plate "), Some("Textured PEI Plate"));
        assert_eq!(normalize("4"), Some("Textured PEI Plate"));
        assert_eq!(normalize("1"), Some("Cool Plate"));
        assert_eq!(normalize("6"), Some("Supertack Plate"));
        for bad in ["", "0", "7", "Default Plate", "Glass"] {
            assert_eq!(normalize(bad), None, "{bad}");
        }
    }

    #[test]
    fn choice_order() {
        let lib =
            Library::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/profiles"))
                .unwrap();
        let bare = obj(json!({"printer_model": "Nobody"}));
        assert_eq!(choose(&bare, None, &lib), FALLBACK);
        let acme = obj(json!({"printer_model": "Acme One"}));
        assert_eq!(choose(&acme, None, &lib), "Textured PEI Plate", "from the model");
        let m = obj(json!({"printer_model": "Acme One", "default_bed_type": "1"}));
        assert_eq!(choose(&m, None, &lib), "Cool Plate");
        let m = obj(json!({"default_bed_type": "1", "curr_bed_type": "Engineering Plate"}));
        assert_eq!(choose(&m, None, &lib), "Engineering Plate");
        let p = obj(json!({"curr_bed_type": "Supertack Plate"}));
        assert_eq!(choose(&m, Some(&p), &lib), "Supertack Plate");
        let junk = obj(json!({"curr_bed_type": "Glass"}));
        assert_eq!(choose(&m, Some(&junk), &lib), "Engineering Plate");
    }
}
