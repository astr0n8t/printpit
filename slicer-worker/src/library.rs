//! Orca's bundled system profiles, and flattening of `inherits` chains.
//!
//! The 2.4.2 CLI does not resolve `inherits` itself: it reads each file as-is, and the values that
//! matter (`compatible_printers`, most settings) live in the parents. So everything handed to Orca is
//! flattened here first: walk the chain to its root, then apply each child's keys over its parent's.
//!
//! Layout: `profiles/<Vendor>.json` lists `machine_model_list`, `machine_list`, `process_list` and
//! `filament_list` as `{name, sub_path}` under `profiles/<Vendor>/`. Filaments a vendor doesn't ship come
//! from the shared `OrcaFilamentLibrary` vendor. Entries with `"instantiation": "true"` are selectable;
//! the rest (`fdm_*_common`, …) are abstract bases.
//!
//! The CLI decides whether a process fits a printer by the printer's *system name*: its own name for a
//! `"from": "system"` profile, otherwise its `inherits`. So a flattened **user** preset keeps
//! `inherits` pointing at its nearest system ancestor; a flattened system profile drops it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Profile = Map<String, Value>;

/// The vendor that holds generic filaments for everyone.
pub const SHARED_VENDOR: &str = "OrcaFilamentLibrary";
/// Inheritance chains in Orca are 3–5 deep; anything past this is a loop or garbage.
const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Machine,
    Process,
    Filament,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Machine, Kind::Process, Kind::Filament];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "machine" => Some(Self::Machine),
            "process" => Some(Self::Process),
            "filament" => Some(Self::Filament),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Machine => "machine",
            Self::Process => "process",
            Self::Filament => "filament",
        }
    }

    fn list_key(self) -> &'static str {
        match self {
            Self::Machine => "machine_list",
            Self::Process => "process_list",
            Self::Filament => "filament_list",
        }
    }

    /// What a preset is, from `type` or (for GUI user presets, which may lack it) its `*_settings_id`.
    pub fn of(profile: &Profile) -> Option<Self> {
        match profile.get("type").and_then(Value::as_str) {
            Some("machine" | "printer") => return Some(Self::Machine),
            Some("process" | "print") => return Some(Self::Process),
            Some("filament") => return Some(Self::Filament),
            _ => {}
        }
        if profile.contains_key("printer_settings_id") {
            Some(Self::Machine)
        } else if profile.contains_key("print_settings_id") {
            Some(Self::Process)
        } else if profile.contains_key("filament_settings_id") {
            Some(Self::Filament)
        } else {
            None
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LibError {
    #[error("unknown vendor `{0}`")]
    UnknownVendor(String),
    #[error("no {kind} profile named `{name}`")]
    NotFound { kind: &'static str, name: String },
    #[error("profile `{0}` has an inheritance loop or is nested too deeply")]
    TooDeep(String),
    #[error("profile `{name}` could not be read: {reason}")]
    Unreadable { name: String, reason: String },
    #[error("preset has no `name`")]
    NoName,
    #[error("preset `{0}` is not a machine, process or filament profile")]
    UnknownKind(String),
}

#[derive(Debug, Default)]
struct Vendor {
    version: String,
    files: HashMap<(Kind, String), PathBuf>,
    /// Names in index order, per kind.
    order: BTreeMap<Kind, Vec<String>>,
}

#[derive(Debug)]
pub struct Library {
    vendors: BTreeMap<String, Vendor>,
    /// For user presets: which vendor holds a system profile of this kind and name (first wins).
    owner: HashMap<(Kind, String), String>,
}

#[derive(Debug, Serialize)]
pub struct VendorSummary {
    pub name: String,
    pub version: String,
}

/// The selectable profiles of one vendor, for the admin's import picker.
#[derive(Debug, Serialize)]
pub struct VendorIndex {
    pub name: String,
    pub version: String,
    pub machines: Vec<MachineEntry>,
    pub processes: Vec<ProcessEntry>,
    pub filaments: Vec<FilamentEntry>,
}

#[derive(Debug, Serialize)]
pub struct MachineEntry {
    pub name: String,
    pub printer_model: String,
    pub nozzle: String,
}

#[derive(Debug, Serialize)]
pub struct ProcessEntry {
    pub name: String,
    pub layer_height: String,
    pub compatible_printers: Vec<String>,
    pub compatible_printers_condition: String,
}

#[derive(Debug, Serialize)]
pub struct FilamentEntry {
    pub name: String,
    pub filament_type: String,
    pub filament_vendor: String,
    pub compatible_printers: Vec<String>,
    pub compatible_printers_condition: String,
}

/// One user preset after resolution.
#[derive(Debug, Serialize)]
pub struct Resolved {
    pub name: String,
    pub kind: Kind,
    pub profile: Profile,
}

impl Library {
    /// Reads every `<Vendor>.json` index under `root`. A vendor whose index can't be read is skipped
    /// with a warning rather than taking the worker down.
    pub fn load(root: &Path) -> std::io::Result<Self> {
        let mut vendors = BTreeMap::new();
        let mut owner = HashMap::new();
        let mut entries: Vec<_> = std::fs::read_dir(root)?.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") || !path.is_file() {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            match read_vendor(root, &name, &path) {
                Ok(v) => {
                    for (kind, name_) in v.files.keys() {
                        owner
                            .entry((*kind, name_.clone()))
                            .or_insert_with(|| name.clone());
                    }
                    vendors.insert(name, v);
                }
                Err(reason) => tracing::warn!(vendor = %name, %reason, "skipping vendor profile index"),
            }
        }
        // Vendor-specific entries are found before the shared library's by `owner`; that's what Orca
        // does too (a vendor's own index wins). Nothing to do: BTreeMap order only affects ties
        // between vendors, which for leaf names don't happen in practice.
        Ok(Self { vendors, owner })
    }

    pub fn vendors(&self) -> Vec<VendorSummary> {
        self.vendors
            .iter()
            .map(|(name, v)| VendorSummary {
                name: name.clone(),
                version: v.version.clone(),
            })
            .collect()
    }

    /// Selectable profiles of a vendor, each flattened enough to show what it fits.
    pub fn vendor_index(&self, vendor: &str) -> Result<VendorIndex, LibError> {
        let v = self.vendor(vendor)?;
        let mut memo = HashMap::new();
        let mut index = VendorIndex {
            name: vendor.to_string(),
            version: v.version.clone(),
            machines: Vec::new(),
            processes: Vec::new(),
            filaments: Vec::new(),
        };
        for kind in Kind::ALL {
            for name in v.order.get(&kind).into_iter().flatten() {
                let p = match self.system_memo(vendor, kind, name, &mut memo) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::debug!(%vendor, %name, error = %e, "skipping unreadable profile");
                        continue;
                    }
                };
                if text(&p, "instantiation") != "true" {
                    continue;
                }
                match kind {
                    Kind::Machine => index.machines.push(MachineEntry {
                        name: name.clone(),
                        printer_model: text(&p, "printer_model"),
                        nozzle: first_non_empty(&[text(&p, "printer_variant"), text(&p, "nozzle_diameter")]),
                    }),
                    Kind::Process => index.processes.push(ProcessEntry {
                        name: name.clone(),
                        layer_height: text(&p, "layer_height"),
                        compatible_printers: list(&p, "compatible_printers"),
                        compatible_printers_condition: text(&p, "compatible_printers_condition"),
                    }),
                    Kind::Filament => index.filaments.push(FilamentEntry {
                        name: name.clone(),
                        filament_type: text(&p, "filament_type"),
                        filament_vendor: text(&p, "filament_vendor"),
                        compatible_printers: list(&p, "compatible_printers"),
                        compatible_printers_condition: text(&p, "compatible_printers_condition"),
                    }),
                }
            }
        }
        Ok(index)
    }

    /// One system profile, flattened.
    pub fn system(&self, vendor: &str, kind: Kind, name: &str) -> Result<Profile, LibError> {
        self.system_memo(vendor, kind, name, &mut HashMap::new())
    }

    /// Flattens user presets (exported from Orca, or a bundle's files). A preset's parent is looked
    /// up among the other presets of the same kind first, then in the system library.
    pub fn resolve(&self, presets: Vec<Profile>) -> Vec<Result<Resolved, LibError>> {
        let mut batch: HashMap<(Kind, String), Profile> = HashMap::new();
        let mut order = Vec::new();
        for p in presets {
            let item = match (text_opt(&p, "name"), Kind::of(&p)) {
                (None, _) => Err(LibError::NoName),
                (Some(n), None) => Err(LibError::UnknownKind(n)),
                (Some(n), Some(k)) => {
                    batch.insert((k, n.clone()), p);
                    Ok((k, n))
                }
            };
            order.push(item);
        }
        order
            .into_iter()
            .map(|item| {
                let (kind, name) = item?;
                let profile = self.resolve_user(&batch, kind, &name)?;
                Ok(Resolved { name, kind, profile })
            })
            .collect()
    }

    fn resolve_user(
        &self,
        batch: &HashMap<(Kind, String), Profile>,
        kind: Kind,
        name: &str,
    ) -> Result<Profile, LibError> {
        let mut chain: Vec<&Profile> = Vec::new();
        let mut seen = HashSet::new();
        let mut current = name.to_string();
        // Walk the user part of the chain.
        let system_parent = loop {
            if chain.len() >= MAX_DEPTH || !seen.insert(current.clone()) {
                return Err(LibError::TooDeep(name.to_string()));
            }
            let Some(p) = batch.get(&(kind, current.clone())) else {
                break Some(current);
            };
            chain.push(p);
            match text_opt(p, "inherits") {
                Some(parent) => current = parent,
                None => break None,
            }
        };
        let mut flat = match &system_parent {
            Some(parent) => {
                let vendor = self
                    .owner
                    .get(&(kind, parent.clone()))
                    .ok_or_else(|| LibError::NotFound {
                        kind: kind.as_str(),
                        name: parent.clone(),
                    })?;
                self.system(vendor, kind, parent)?
            }
            None => Profile::new(),
        };
        for p in chain.iter().rev() {
            merge(&mut flat, p);
        }
        flat.remove("inherits");
        if let Some(parent) = system_parent {
            flat.insert("inherits".into(), Value::String(parent));
        }
        flat.insert("name".into(), Value::String(name.to_string()));
        flat.insert("type".into(), Value::String(kind.as_str().into()));
        // Even when the export left `from` out (it would otherwise inherit "system" from the parent),
        // so that the CLI takes the system name from `inherits`.
        flat.insert("from".into(), Value::String("User".into()));
        Ok(flat)
    }

    fn vendor(&self, vendor: &str) -> Result<&Vendor, LibError> {
        self.vendors
            .get(vendor)
            .ok_or_else(|| LibError::UnknownVendor(vendor.to_string()))
    }

    fn system_memo(
        &self,
        vendor: &str,
        kind: Kind,
        name: &str,
        memo: &mut HashMap<(String, Kind, String), Profile>,
    ) -> Result<Profile, LibError> {
        let key = (vendor.to_string(), kind, name.to_string());
        if let Some(p) = memo.get(&key) {
            return Ok(p.clone());
        }
        let mut flat = self.flatten(vendor, kind, name, memo, 0)?;
        flat.remove("inherits");
        flat.insert("name".into(), Value::String(name.to_string()));
        flat.insert("type".into(), Value::String(kind.as_str().into()));
        memo.insert(key, flat.clone());
        Ok(flat)
    }

    fn flatten(
        &self,
        vendor: &str,
        kind: Kind,
        name: &str,
        memo: &mut HashMap<(String, Kind, String), Profile>,
        depth: usize,
    ) -> Result<Profile, LibError> {
        if depth >= MAX_DEPTH {
            return Err(LibError::TooDeep(name.to_string()));
        }
        let (owner, path) = self.locate(vendor, kind, name)?;
        let own = read_profile(path, name)?;
        let mut flat = match text_opt(&own, "inherits") {
            Some(parent) => {
                let key = (owner.to_string(), kind, parent.clone());
                match memo.get(&key) {
                    Some(p) => p.clone(),
                    None => self.flatten(owner, kind, &parent, memo, depth + 1)?,
                }
            }
            None => Profile::new(),
        };
        merge(&mut flat, &own);
        Ok(flat)
    }

    /// A name is looked up in the vendor itself, then in the shared filament library.
    fn locate<'a>(
        &'a self,
        vendor: &'a str,
        kind: Kind,
        name: &str,
    ) -> Result<(&'a str, &'a Path), LibError> {
        let key = (kind, name.to_string());
        if let Some(p) = self.vendor(vendor)?.files.get(&key) {
            return Ok((vendor, p));
        }
        if let Some(p) = self.vendors.get(SHARED_VENDOR).and_then(|v| v.files.get(&key)) {
            return Ok((SHARED_VENDOR, p));
        }
        Err(LibError::NotFound {
            kind: kind.as_str(),
            name: name.to_string(),
        })
    }
}

fn read_vendor(root: &Path, name: &str, index: &Path) -> Result<Vendor, String> {
    let raw = std::fs::read(index).map_err(|e| e.to_string())?;
    let doc: Profile = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
    let mut v = Vendor {
        version: text(&doc, "version"),
        ..Vendor::default()
    };
    let dir = root.join(name);
    for kind in Kind::ALL {
        let Some(items) = doc.get(kind.list_key()).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let (Some(n), Some(sub)) = (
                item.get("name").and_then(Value::as_str),
                item.get("sub_path").and_then(Value::as_str),
            ) else {
                continue;
            };
            // The index is bundled with Orca, but a path that climbs out of the vendor folder is
            // still refused.
            let sub = Path::new(sub);
            if !sub.components().all(|c| matches!(c, Component::Normal(_))) {
                continue;
            }
            if v.files.insert((kind, n.to_string()), dir.join(sub)).is_none() {
                v.order.entry(kind).or_default().push(n.to_string());
            }
        }
    }
    Ok(v)
}

fn read_profile(path: &Path, name: &str) -> Result<Profile, LibError> {
    let unreadable = |reason: String| LibError::Unreadable {
        name: name.to_string(),
        reason,
    };
    let raw = std::fs::read(path).map_err(|e| unreadable(e.kind().to_string()))?;
    serde_json::from_slice(&raw).map_err(|e| unreadable(e.to_string()))
}

/// Child keys replace the parent's (Orca's own rule: whole values, never element-wise).
fn merge(into: &mut Profile, child: &Profile) {
    for (k, v) in child {
        into.insert(k.clone(), v.clone());
    }
}

fn text_opt(p: &Profile, key: &str) -> Option<String> {
    p.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A setting as text: a string, or the first element of Orca's per-extruder arrays.
pub fn text(p: &Profile, key: &str) -> String {
    match p.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.first().and_then(Value::as_str).unwrap_or_default().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

pub fn list(p: &Profile, key: &str) -> Vec<String> {
    match p.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(s)) if !s.is_empty() => s.split(';').map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

fn first_non_empty(options: &[String]) -> String {
    options
        .iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixtures() -> Library {
        Library::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/profiles")).unwrap()
    }

    fn obj(v: Value) -> Profile {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn lists_vendors() {
        let names: Vec<_> = fixtures().vendors().into_iter().map(|v| v.name).collect();
        assert_eq!(names, ["Acme", "Broken", SHARED_VENDOR]);
    }

    #[test]
    fn flattens_a_system_chain() {
        let lib = fixtures();
        let m = lib.system("Acme", Kind::Machine, "Acme One 0.4 nozzle").unwrap();
        assert!(!m.contains_key("inherits"));
        assert_eq!(text(&m, "name"), "Acme One 0.4 nozzle");
        assert_eq!(text(&m, "type"), "machine");
        // From the root, overridden in the middle, overridden again in the leaf.
        assert_eq!(text(&m, "gcode_flavor"), "klipper");
        assert_eq!(text(&m, "retraction_length"), "0.5");
        assert_eq!(list(&m, "printable_area"), ["0x0", "200x0", "200x200", "0x200"]);
    }

    #[test]
    fn compatibility_comes_from_the_parent() {
        let p = fixtures()
            .system("Acme", Kind::Process, "0.20mm Standard @Acme")
            .unwrap();
        assert_eq!(list(&p, "compatible_printers"), ["Acme One 0.4 nozzle"]);
        assert_eq!(text(&p, "layer_height"), "0.2");
    }

    #[test]
    fn filaments_fall_back_to_the_shared_library() {
        let lib = fixtures();
        let f = lib.system("Acme", Kind::Filament, "Generic PLA @System").unwrap();
        assert_eq!(text(&f, "filament_type"), "PLA");
        assert_eq!(text(&f, "nozzle_temperature"), "220");
        // A vendor filament that inherits from the shared library.
        let f = lib.system("Acme", Kind::Filament, "Acme PLA").unwrap();
        assert_eq!(text(&f, "nozzle_temperature"), "215");
        assert_eq!(text(&f, "filament_density"), "1.24");
    }

    #[test]
    fn unknowns_and_loops() {
        let lib = fixtures();
        assert_eq!(
            lib.system("Nope", Kind::Machine, "x").unwrap_err(),
            LibError::UnknownVendor("Nope".into())
        );
        assert!(matches!(
            lib.system("Acme", Kind::Machine, "No Such Printer"),
            Err(LibError::NotFound { .. })
        ));
        assert!(matches!(
            lib.system("Broken", Kind::Machine, "Loop A"),
            Err(LibError::TooDeep(_))
        ));
        assert!(matches!(
            lib.system("Broken", Kind::Machine, "Bad Json"),
            Err(LibError::Unreadable { .. })
        ));
    }

    #[test]
    fn index_has_only_selectable_profiles() {
        let i = fixtures().vendor_index("Acme").unwrap();
        let names = |v: Vec<String>| v;
        assert_eq!(
            names(i.machines.iter().map(|m| m.name.clone()).collect()),
            ["Acme One 0.4 nozzle"]
        );
        assert_eq!(i.machines[0].printer_model, "Acme One");
        assert_eq!(i.machines[0].nozzle, "0.4");
        assert_eq!(i.processes.len(), 1);
        assert_eq!(i.processes[0].compatible_printers, ["Acme One 0.4 nozzle"]);
        assert_eq!(i.filaments.len(), 1);
        assert_eq!(i.filaments[0].filament_type, "PLA");
        // The broken vendor still lists what it can.
        let b = fixtures().vendor_index("Broken").unwrap();
        assert!(b.machines.is_empty());
    }

    #[test]
    fn user_presets_keep_their_system_parent() {
        let lib = fixtures();
        let out = lib.resolve(vec![
            obj(
                json!({"name": "My Acme", "from": "User", "inherits": "Acme One 0.4 nozzle",
                       "printer_settings_id": "My Acme", "retraction_length": ["0.8"]}),
            ),
            // A user process inheriting another user process, which inherits a system one.
            obj(
                json!({"name": "Fine", "from": "User", "inherits": "Fast", "print_settings_id": "Fine",
                       "layer_height": "0.12"}),
            ),
            obj(
                json!({"name": "Fast", "from": "User", "inherits": "0.20mm Standard @Acme",
                       "print_settings_id": "Fast", "sparse_infill_density": "10%"}),
            ),
        ]);
        let out: Vec<_> = out.into_iter().map(Result::unwrap).collect();
        let m = &out[0];
        assert_eq!(m.kind, Kind::Machine);
        assert_eq!(text(&m.profile, "inherits"), "Acme One 0.4 nozzle");
        assert_eq!(text(&m.profile, "retraction_length"), "0.8");
        assert_eq!(text(&m.profile, "gcode_flavor"), "klipper");
        assert_eq!(text(&m.profile, "type"), "machine");
        assert_eq!(text(&m.profile, "from"), "User");
        let fine = &out[1];
        assert_eq!(text(&fine.profile, "inherits"), "0.20mm Standard @Acme");
        assert_eq!(text(&fine.profile, "layer_height"), "0.12");
        assert_eq!(text(&fine.profile, "sparse_infill_density"), "10%");
        assert_eq!(
            list(&fine.profile, "compatible_printers"),
            ["Acme One 0.4 nozzle"]
        );
    }

    #[test]
    fn user_preset_errors() {
        let lib = fixtures();
        let out = lib.resolve(vec![
            obj(json!({"from": "User"})),
            obj(json!({"name": "Mystery", "foo": 1})),
            obj(json!({"name": "Orphan", "type": "filament", "inherits": "Unobtainium"})),
            obj(json!({"name": "A", "type": "process", "inherits": "B"})),
            obj(json!({"name": "B", "type": "process", "inherits": "A"})),
            obj(json!({"name": "Standalone", "type": "filament", "filament_type": ["PETG"]})),
        ]);
        assert_eq!(out[0].as_ref().unwrap_err(), &LibError::NoName);
        assert_eq!(
            out[1].as_ref().unwrap_err(),
            &LibError::UnknownKind("Mystery".into())
        );
        assert!(matches!(out[2], Err(LibError::NotFound { .. })));
        assert!(matches!(out[3], Err(LibError::TooDeep(_))));
        let s = out[5].as_ref().unwrap();
        assert!(!s.profile.contains_key("inherits"));
        assert_eq!(text(&s.profile, "filament_type"), "PETG");
    }

    #[test]
    fn kind_detection() {
        assert_eq!(Kind::of(&obj(json!({"type": "machine"}))), Some(Kind::Machine));
        assert_eq!(Kind::of(&obj(json!({"type": "print"}))), Some(Kind::Process));
        assert_eq!(
            Kind::of(&obj(json!({"filament_settings_id": ["x"]}))),
            Some(Kind::Filament)
        );
        assert_eq!(Kind::of(&obj(json!({"name": "x"}))), None);
    }
}
