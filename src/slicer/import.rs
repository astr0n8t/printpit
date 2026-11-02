//! Reading uploaded presets for the profile library: Orca preset JSON, and Orca's zips
//! (`.orca_printer`, `.orca_filament`, `.orca_bundle`, or a plain "presets (.zip)" export).
//!
//! Admins upload these, but they're still treated as untrusted: only `*.json` entries are read,
//! every size is capped while reading (not from what the zip claims), and nothing is ever
//! written to disk. Each file or entry becomes one [`Candidate`]: a preset to resolve, or the
//! reason it isn't one, so the report can account for everything that was uploaded.

use std::io::{Cursor, Read};

use super::Profile;

/// Files per import.
pub const MAX_FILES: usize = 20;
/// The import route's own body limit.
pub const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_ZIP_ENTRIES: usize = 1000;
/// One preset; real ones are a few KB to a few hundred KB.
pub const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
/// Everything unpacked from one import (a zip bomb stops here).
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Presets sent to the worker in one go (its own limit is 500).
pub const MAX_PRESETS: usize = 500;
pub const MAX_NAME: usize = 128;

/// Extensions accepted for zips.
const ZIP_EXTENSIONS: [&str; 4] = ["zip", "orca_printer", "orca_filament", "orca_bundle"];

#[derive(Debug)]
pub struct Candidate {
    /// Where it came from, for the report: `file.json` or `bundle.zip: filament/x.json`.
    pub source: String,
    pub preset: Result<Profile, String>,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub candidates: Vec<Candidate>,
    /// Things deliberately left out, e.g. `bundle.zip: 3 non-JSON entries`.
    pub skipped: Vec<String>,
    unpacked: u64,
}

impl Parsed {
    /// Adds one uploaded file.
    pub fn add_file(&mut self, file_name: &str, bytes: &[u8]) {
        let label = label(file_name);
        let ext = file_name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        if ext == "json" {
            self.unpacked += bytes.len() as u64;
            let preset = if self.unpacked > MAX_TOTAL_BYTES {
                Err("the import is too large".into())
            } else {
                parse_preset(bytes)
            };
            self.candidates.push(Candidate {
                source: label,
                preset,
            });
        } else if ZIP_EXTENSIONS.contains(&ext.as_str()) {
            if let Err(e) = self.add_zip(&label, bytes) {
                self.candidates.push(Candidate {
                    source: label,
                    preset: Err(e),
                });
            }
        } else {
            self.candidates.push(Candidate {
                source: label,
                preset: Err(
                    "not a .json preset or an Orca bundle (.zip, .orca_printer, .orca_filament)".into(),
                ),
            });
        }
    }

    fn add_zip(&mut self, label: &str, bytes: &[u8]) -> Result<(), String> {
        if !bytes.starts_with(b"PK") {
            return Err("not a zip file".into());
        }
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| "not a readable zip file")?;
        if zip.len() > MAX_ZIP_ENTRIES {
            return Err(format!("more than {MAX_ZIP_ENTRIES} entries"));
        }
        let mut other = 0usize;
        for i in 0..zip.len() {
            let mut entry = match zip.by_index(i) {
                Ok(e) => e,
                Err(_) => {
                    self.candidates.push(Candidate {
                        source: format!("{label}: entry {}", i + 1),
                        preset: Err("can't be read (encrypted or an unsupported compression)".into()),
                    });
                    continue;
                }
            };
            if entry.is_dir() {
                continue;
            }
            // A name that doesn't decode is left out like any other non-JSON entry.
            let name = entry.name().map(|n| n.to_string()).unwrap_or_default();
            let lower = name.to_ascii_lowercase();
            if !lower.ends_with(".json") {
                other += 1;
                continue;
            }
            // The bundle's table of contents, not a preset.
            if lower.rsplit('/').next() == Some("bundle_structure.json") {
                continue;
            }
            let source = format!("{label}: {}", self::label(&name));
            let mut buf = Vec::new();
            let read = (&mut entry).take(MAX_ENTRY_BYTES + 1).read_to_end(&mut buf);
            let preset = match read {
                Err(_) => Err("can't be unpacked".into()),
                Ok(_) if buf.len() as u64 > MAX_ENTRY_BYTES => {
                    Err(format!("larger than {} MB", MAX_ENTRY_BYTES / (1024 * 1024)))
                }
                Ok(n) => {
                    self.unpacked += n as u64;
                    if self.unpacked > MAX_TOTAL_BYTES {
                        return Err(format!(
                            "unpacks to more than {} MB",
                            MAX_TOTAL_BYTES / (1024 * 1024)
                        ));
                    }
                    parse_preset(&buf)
                }
            };
            self.candidates.push(Candidate { source, preset });
        }
        if other > 0 {
            self.skipped.push(format!(
                "{label}: {other} non-JSON {} left out",
                if other == 1 { "entry" } else { "entries" }
            ));
        }
        Ok(())
    }
}

fn parse_preset(bytes: &[u8]) -> Result<Profile, String> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(serde_json::Value::Object(m)) => Ok(m),
        Ok(_) => Err("not a preset (expected a JSON object)".into()),
        Err(e) => Err(format!("not valid JSON ({e})")),
    }
}

/// A short, printable label for a file or entry name.
fn label(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = base.chars().filter(|c| !c.is_control()).take(100).collect();
    if clean.is_empty() {
        "(unnamed)".into()
    } else {
        clean
    }
}

/// A profile name as stored: trimmed, 1–128 characters, no control characters.
pub fn valid_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME || n.chars().any(char::is_control) {
        return Err(format!("profile names are 1–{MAX_NAME} characters"));
    }
    Ok(n.to_string())
}

/// Whether a process or filament fits a machine, as far as can be told simply: listed in
/// `compatible_printers`, or unrestricted. A `compatible_printers_condition` alone is an
/// expression we don't evaluate: maybe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Yes,
    Maybe,
    No,
}

pub fn fit(compatible: &[String], condition: &str, machine: &str) -> Fit {
    if compatible.iter().any(|c| c == machine) {
        Fit::Yes
    } else if !compatible.is_empty() {
        Fit::No
    } else if condition.trim().is_empty() {
        Fit::Yes
    } else {
        Fit::Maybe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            if name.ends_with('/') {
                w.add_directory(*name, opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(body).unwrap();
            }
        }
        w.finish().unwrap().into_inner()
    }

    fn ok(p: &Parsed) -> Vec<String> {
        p.candidates
            .iter()
            .filter(|c| c.preset.is_ok())
            .map(|c| c.source.clone())
            .collect()
    }

    #[test]
    fn json_files() {
        let mut p = Parsed::default();
        p.add_file(
            "My PLA.json",
            br#"{"name": "My PLA", "inherits": "Generic PLA @System"}"#,
        );
        p.add_file("broken.JSON", b"{ nope");
        p.add_file("list.json", b"[1, 2]");
        p.add_file("model.stl", b"solid x");
        assert_eq!(ok(&p), ["My PLA.json"]);
        let errs: Vec<_> = p
            .candidates
            .iter()
            .filter_map(|c| c.preset.as_ref().err())
            .collect();
        assert!(errs[0].contains("not valid JSON"), "{errs:?}");
        assert!(errs[1].contains("JSON object"), "{errs:?}");
        assert!(errs[2].contains("Orca bundle"), "{errs:?}");
    }

    #[test]
    fn orca_printer_bundle() {
        let bundle = zip(&[
            (
                "bundle_structure.json",
                br#"{"bundle_type": "printer config bundle"}"#,
            ),
            ("printer/", b""),
            ("printer/My Voron.json", br#"{"name": "My Voron"}"#),
            ("filament/My PLA.json", br#"{"name": "My PLA"}"#),
            ("process/Fine.json", br#"{"name": "Fine"}"#),
            ("process/notes.txt", b"hello"),
            ("cover.png", b"\x89PNG"),
            ("../../etc/evil.json", br#"{"name": "Evil"}"#),
        ]);
        let mut p = Parsed::default();
        p.add_file("Voron.orca_printer", &bundle);
        assert_eq!(
            ok(&p),
            [
                "Voron.orca_printer: My Voron.json",
                "Voron.orca_printer: My PLA.json",
                "Voron.orca_printer: Fine.json",
                // A path in a name is only a label; nothing is written anywhere.
                "Voron.orca_printer: evil.json",
            ]
        );
        assert_eq!(p.skipped, ["Voron.orca_printer: 2 non-JSON entries left out"]);
    }

    #[test]
    fn rejects_bad_zips() {
        let mut p = Parsed::default();
        p.add_file("fake.zip", b"PK\x03\x04garbage");
        p.add_file("text.orca_filament", b"just text");
        assert!(ok(&p).is_empty());
        assert_eq!(p.candidates.len(), 2);
        assert!(p.candidates[1].preset.as_ref().unwrap_err().contains("not a zip"));
    }

    #[test]
    fn caps_sizes_while_reading() {
        // Highly compressible: tiny in the zip, huge unpacked.
        let big = vec![b' '; (MAX_ENTRY_BYTES + 10) as usize];
        let bomb = zip(&[("huge.json", &big), ("small.json", br#"{"name": "s"}"#)]);
        assert!(bomb.len() < 100_000);
        let mut p = Parsed::default();
        p.add_file("bomb.zip", &bomb);
        assert_eq!(ok(&p), ["bomb.zip: small.json"]);
        assert!(
            p.candidates[0]
                .preset
                .as_ref()
                .unwrap_err()
                .contains("larger than")
        );

        // Many entries just under the per-entry cap trip the total.
        let near = vec![b' '; (MAX_ENTRY_BYTES - 1) as usize];
        let entries: Vec<(String, &[u8])> = (0..20).map(|i| (format!("{i}.json"), near.as_slice())).collect();
        let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, b)| (n.as_str(), *b)).collect();
        let mut p = Parsed::default();
        p.add_file("many.zip", &zip(&refs));
        let last = p.candidates.last().unwrap();
        assert_eq!(last.source, "many.zip");
        assert!(last.preset.as_ref().unwrap_err().contains("unpacks to more than"));
    }

    #[test]
    fn names_and_labels() {
        assert_eq!(valid_name("  My PLA  ").unwrap(), "My PLA");
        assert!(valid_name("").is_err());
        assert!(valid_name("a\nb").is_err());
        assert!(valid_name(&"x".repeat(129)).is_err());
        assert_eq!(label("dir/sub\\x.json"), "x.json");
        assert_eq!(label("bad\u{7}name.json"), "badname.json");
        assert_eq!(label("/"), "(unnamed)");
    }

    #[test]
    fn compatibility() {
        let m = "Voron 2.4 350 0.4 nozzle";
        assert_eq!(fit(&[m.into()], "", m), Fit::Yes);
        assert_eq!(fit(&["Other".into()], "", m), Fit::No);
        assert_eq!(fit(&[], "", m), Fit::Yes);
        assert_eq!(fit(&[], "nozzle_diameter[0]==0.4", m), Fit::Maybe);
    }
}
