//! Running the OrcaSlicer CLI: arguments, the process group, and reading back what it did.
//!
//! Every slice runs in its own temp dir with fixed file names, so no user text ever reaches the
//! command line. The child starts a new process group; dropping [`Running`] (timeout, client gone,
//! worker shutting down) kills the whole group. Orca's outcome is read from `out/result.json`, which
//! it always writes; the exit status is only the negative `CLI_*` code mod 256.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde::Deserialize;

pub const MACHINE: &str = "machine.json";
pub const PROCESS: &str = "process.json";
pub const FILAMENT: &str = "filament.json";
pub const LOG: &str = "orca.log";
pub const STDIO: &str = "stdio.log";
pub const OUT: &str = "out";
const DATA: &str = "data";
const LOG_TAIL_LINES: usize = 40;
const LOG_LINE_MAX: usize = 300;

/// Model formats Orca reads, by extension.
pub const MODEL_EXTENSIONS: [&str; 5] = ["stl", "3mf", "obj", "step", "stp"];

/// Per-slice switches. Settings themselves (layer height, infill, …) are already in the process
/// profile printpit sends; these are only what the CLI takes as flags.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    /// Rearrange objects on the plate. `None` keeps Orca's default (centred), except that a 3MF is
    /// always rearranged when our machine replaces its own, since its layout was for another bed.
    pub arrange: Option<bool>,
    pub orient: bool,
    /// Plate to slice; a 3MF project can have several. One G-code file comes back.
    pub plate: u32,
    /// For a 3MF: keep the project's process and filament settings. The machine is still ours.
    pub project_settings: bool,
    /// Copies of the model on the plate (`--clone-objects N`, which also spreads them out;
    /// `--repetitions` is rejected by the 2.4.2 CLI). Not for 3MF projects, whose layout is theirs.
    pub copies: u32,
}

pub const MAX_COPIES: u32 = 50;

impl Default for Options {
    fn default() -> Self {
        Self {
            arrange: None,
            orient: false,
            plate: 1,
            project_settings: false,
            copies: 1,
        }
    }
}

impl Options {
    pub fn validate(&self, model_ext: &str) -> Result<(), String> {
        if !(1..=64).contains(&self.plate) {
            return Err("plate must be from 1 to 64".into());
        }
        if self.project_settings && model_ext != "3mf" {
            return Err("project_settings only applies to 3MF files".into());
        }
        if !(1..=MAX_COPIES).contains(&self.copies) {
            return Err(format!("copies must be from 1 to {MAX_COPIES}"));
        }
        if self.copies > 1 && model_ext == "3mf" {
            return Err("copies don't apply to a 3MF project; lay them out in the project".into());
        }
        Ok(())
    }
}

/// The command line for one slice, relative to the job dir.
pub fn args(model: &str, options: &Options, max_secs: u64) -> Vec<OsString> {
    let mut a: Vec<String> = vec!["--datadir".into(), DATA.into()];
    if options.project_settings {
        a.extend(["--load-settings".into(), MACHINE.into()]);
    } else {
        a.extend([
            "--load-settings".into(),
            format!("{MACHINE};{PROCESS}"),
            "--load-filaments".into(),
            FILAMENT.into(),
        ]);
    }
    let is_3mf = model.ends_with(".3mf");
    match options.arrange {
        Some(on) => a.extend(["--arrange".into(), if on { "1" } else { "0" }.into()]),
        None if is_3mf => a.extend(["--arrange".into(), "1".into()]),
        None => {}
    }
    if options.orient {
        a.extend(["--orient".into(), "1".into()]);
    }
    if options.copies > 1 {
        a.extend(["--clone-objects".into(), options.copies.to_string()]);
    }
    a.extend([
        // Orca's own per-plate limit, a gentler stop than our kill.
        "--mstpp".into(),
        max_secs.to_string(),
        "--slice".into(),
        options.plate.to_string(),
        "--outputdir".into(),
        OUT.into(),
        "--debug".into(),
        "3".into(),
        "--logfile".into(),
        LOG.into(),
        model.into(),
    ]);
    a.into_iter().map(OsString::from).collect()
}

/// A running slicer. Dropping it kills the whole process group.
pub struct Running {
    child: tokio::process::Child,
    pgid: Option<i32>,
}

pub enum Finished {
    Exited(ExitStatus),
    TimedOut,
}

impl Running {
    pub fn spawn(orca: &Path, dir: &Path, args: &[OsString]) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir.join(OUT))?;
        std::fs::create_dir_all(dir.join(DATA))?;
        let stdio = std::fs::File::create(dir.join(STDIO))?;
        let mut cmd = tokio::process::Command::new(orca);
        cmd.args(args)
            .current_dir(dir)
            // Nothing from the worker's environment (the token!) reaches Orca.
            .env_clear()
            .env("HOME", dir)
            .env("TMPDIR", dir)
            .env("LC_ALL", "C")
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(stdio.try_clone()?)
            .stderr(stdio)
            .process_group(0)
            .kill_on_drop(true);
        if let Some(ld) = std::env::var_os("LD_LIBRARY_PATH") {
            cmd.env("LD_LIBRARY_PATH", ld);
        }
        let child = cmd.spawn()?;
        let pgid = child.id().and_then(|id| i32::try_from(id).ok());
        Ok(Self { child, pgid })
    }

    pub async fn wait(&mut self, timeout: Duration) -> std::io::Result<Finished> {
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(status) => Ok(Finished::Exited(status?)),
            Err(_) => {
                self.kill_group();
                Ok(Finished::TimedOut)
            }
        }
    }

    fn kill_group(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: killpg only sends a signal. The group was created for this child alone
            // (`process_group(0)`), so this can't reach the worker or another slice.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Also after a normal exit, for anything the slicer left behind in its group.
        self.kill_group();
    }
}

/// `out/result.json`.
#[derive(Debug, Deserialize)]
pub struct CliResult {
    pub return_code: i32,
    #[serde(default)]
    pub error_string: String,
}

pub fn read_result(dir: &Path) -> Option<CliResult> {
    let raw = std::fs::read(dir.join(OUT).join("result.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub fn gcode_path(dir: &Path, plate: u32) -> PathBuf {
    dir.join(OUT).join(format!("plate_{plate}.gcode"))
}

/// What kind of failure a `CLI_*` code is (from `libslic3r/Utils.hpp`), so printpit can say whether
/// to fix the model, the profiles, or neither.
pub fn classify(code: i32) -> &'static str {
    match code {
        -3 | -6 | -12..=-9 | -22..=-20 | -50 | -52 | -59 | -60 | -63 | -64 => "model",
        -5 | -7 | -18..=-15 | -23 | -61 | -62 | -68..=-65 => "profile",
        -58 => "timeout",
        -14 => "resources",
        _ => "slicer",
    }
}

/// The last lines of the slicer's log, with the job's paths taken out.
pub fn log_tail(dir: &Path, orca: &Path) -> Vec<String> {
    let text = [LOG, STDIO]
        .iter()
        .filter_map(|f| std::fs::read(dir.join(f)).ok())
        .find(|b| !b.is_empty())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    scrub(&text, dir, orca)
}

pub fn scrub(text: &str, dir: &Path, orca: &Path) -> Vec<String> {
    let mut needles: Vec<(String, &str)> = vec![
        (format!("{}/", dir.display()), ""),
        (dir.display().to_string(), "."),
    ];
    if let Ok(real) = dir.canonicalize() {
        needles.push((format!("{}/", real.display()), ""));
        needles.push((real.display().to_string(), "."));
    }
    // `/opt/orca/bin/orca-slicer` → the install root, `/opt/orca`.
    if let Some(root) = orca
        .parent()
        .and_then(Path::parent)
        .filter(|r| r.as_os_str().len() > 1)
    {
        needles.push((root.display().to_string(), "<orca>"));
    }
    let lines: Vec<String> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        // The GUI-library stubs carry no symbol versions; the loader says so, harmlessly.
        .filter(|l| !l.contains("no version information available"))
        .map(|l| {
            let mut l = l.to_string();
            for (needle, with) in &needles {
                l = l.replace(needle.as_str(), with);
            }
            if l.len() > LOG_LINE_MAX {
                let mut cut = LOG_LINE_MAX;
                while !l.is_char_boundary(cut) {
                    cut -= 1;
                }
                l.truncate(cut);
                l.push('…');
            }
            l
        })
        .collect();
    let skip = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines.into_iter().skip(skip).collect()
}

/// `orca-slicer --help` prints `OrcaSlicer-2.4.2:` first. Also proves the binary loads at all.
pub async fn probe_version(orca: &Path, work_dir: &Path) -> Result<String, String> {
    let dir = tempfile::Builder::new()
        .prefix("probe-")
        .tempdir_in(work_dir)
        .map_err(|e| format!("work dir: {e}"))?;
    let mut run =
        Running::spawn(orca, dir.path(), &["--help".into()]).map_err(|e| format!("cannot run: {e}"))?;
    match run.wait(Duration::from_secs(30)).await {
        Ok(Finished::Exited(_)) => {}
        Ok(Finished::TimedOut) => return Err("`--help` did not finish in 30 s".into()),
        Err(e) => return Err(e.to_string()),
    }
    let out = std::fs::read_to_string(dir.path().join(STDIO)).unwrap_or_default();
    out.lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("OrcaSlicer-")?
                .strip_suffix(':')
                .map(str::to_string)
        })
        .ok_or_else(|| {
            let tail = scrub(&out, dir.path(), orca);
            format!(
                "unexpected `--help` output: {}",
                tail.last().cloned().unwrap_or_default()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(a: Vec<OsString>) -> Vec<String> {
        a.into_iter().map(|x| x.into_string().unwrap()).collect()
    }

    #[test]
    fn stl_args() {
        let a = s(args("model.stl", &Options::default(), 900));
        assert_eq!(
            a,
            [
                "--datadir",
                "data",
                "--load-settings",
                "machine.json;process.json",
                "--load-filaments",
                "filament.json",
                "--mstpp",
                "900",
                "--slice",
                "1",
                "--outputdir",
                "out",
                "--debug",
                "3",
                "--logfile",
                "orca.log",
                "model.stl"
            ]
        );
    }

    #[test]
    fn options_map_to_flags() {
        let o = Options {
            arrange: Some(false),
            orient: true,
            plate: 2,
            ..Options::default()
        };
        let a = s(args("model.stl", &o, 60)).join(" ");
        assert!(a.contains("--arrange 0"), "{a}");
        assert!(a.contains("--orient 1"), "{a}");
        assert!(a.contains("--slice 2"), "{a}");
    }

    #[test]
    fn three_mf_is_rearranged_unless_told_otherwise() {
        let a = s(args("model.3mf", &Options::default(), 60)).join(" ");
        assert!(a.contains("--arrange 1"), "{a}");
        let o = Options {
            arrange: Some(false),
            ..Options::default()
        };
        assert!(s(args("model.3mf", &o, 60)).join(" ").contains("--arrange 0"));
    }

    #[test]
    fn project_settings_load_only_the_machine() {
        let o = Options {
            project_settings: true,
            ..Options::default()
        };
        let a = s(args("model.3mf", &o, 60));
        assert!(a.windows(2).any(|w| w == ["--load-settings", "machine.json"]));
        assert!(!a.iter().any(|x| x == "--load-filaments"));
        assert_eq!(o.validate("3mf"), Ok(()));
        assert!(o.validate("stl").is_err());
    }

    #[test]
    fn option_limits() {
        let o = Options {
            plate: 0,
            ..Options::default()
        };
        assert!(o.validate("stl").is_err());
        for (copies, ext, ok) in [
            (1, "stl", true),
            (50, "obj", true),
            (0, "stl", false),
            (51, "stl", false),
            (2, "3mf", false),
            (1, "3mf", true),
        ] {
            let o = Options {
                copies,
                ..Options::default()
            };
            assert_eq!(o.validate(ext).is_ok(), ok, "{copies} copies of a .{ext}");
        }
        let o = Options {
            copies: 3,
            ..Options::default()
        };
        assert!(
            s(args("model.stl", &o, 60))
                .join(" ")
                .contains("--clone-objects 3")
        );
        assert!(
            !s(args("model.stl", &Options::default(), 60))
                .join(" ")
                .contains("--clone-objects")
        );
        assert!(serde_json::from_str::<Options>(r#"{"layer_height": 0.1}"#).is_err());
        assert_eq!(serde_json::from_str::<Options>("{}").unwrap(), Options::default());
    }

    #[test]
    fn classifies_codes() {
        assert_eq!(classify(-6), "model");
        assert_eq!(classify(-50), "model");
        assert_eq!(classify(-17), "profile");
        assert_eq!(classify(-58), "timeout");
        assert_eq!(classify(-100), "slicer");
    }

    #[test]
    fn scrubs_paths_and_noise() {
        let dir = Path::new("/tmp/slice-abc");
        let text = "loaded /tmp/slice-abc/machine.json\n\n\
            /opt/orca/bin/orca-slicer: /opt/stubs/libdbus-1.so.3: no version information available\n\
            cwd is /tmp/slice-abc\nlibs in /opt/orca/lib\n";
        let out = scrub(text, dir, Path::new("/opt/orca/bin/orca-slicer"));
        assert_eq!(out, ["loaded machine.json", "cwd is .", "libs in <orca>/lib"]);
        let long = "x".repeat(1000);
        assert_eq!(
            scrub(&long, dir, Path::new("/o/b/x"))[0].chars().count(),
            LOG_LINE_MAX + 1
        );
        let many: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let tail = scrub(&many, dir, Path::new("/o/b/x"));
        assert_eq!(tail.len(), LOG_TAIL_LINES);
        assert_eq!(tail.last().unwrap(), "line 99");
    }
}
