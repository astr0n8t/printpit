//! Files for slice jobs: `<data>/slicer/<job id>/model.<ext>` and `out.gcode`.
//!
//! Every path is built from the job id and fixed names; no user text reaches the filesystem.
//! Directories are 0700 and files 0600 (models are the users', and output is G-code that runs
//! on a printer). Modes are set explicitly, so the umask can't widen them.

use std::path::{Path, PathBuf};

/// Model formats the worker slices. The stored model keeps its (lowercased) extension so the
/// worker knows what it is.
pub const MODEL_EXTENSIONS: [&str; 5] = ["stl", "3mf", "obj", "step", "stp"];
const OUTPUT: &str = "out.gcode";
const PARTIAL: &str = "out.gcode.part";

#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
}

impl Storage {
    /// Creates (or tightens) `<data>/slicer`.
    pub fn open(data_dir: &Path) -> std::io::Result<Self> {
        let root = data_dir.join("slicer");
        make_private_dir(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn job_dir(&self, job: i64) -> PathBuf {
        self.root.join(job.to_string())
    }

    /// The model's path for an extension from [`MODEL_EXTENSIONS`]; `None` for anything else.
    pub fn model_path(&self, job: i64, ext: &str) -> Option<PathBuf> {
        let ext = ext.to_ascii_lowercase();
        MODEL_EXTENSIONS
            .contains(&ext.as_str())
            .then(|| self.job_dir(job).join(format!("model.{ext}")))
    }

    /// Where the finished G-code lives.
    pub fn output_path(&self, job: i64) -> PathBuf {
        self.job_dir(job).join(OUTPUT)
    }

    /// Where the G-code is written while it downloads; renamed by [`Storage::commit_output`].
    pub fn partial_output_path(&self, job: i64) -> PathBuf {
        self.job_dir(job).join(PARTIAL)
    }

    /// Creates the job's directory (0700).
    pub fn create_job_dir(&self, job: i64) -> std::io::Result<PathBuf> {
        let dir = self.job_dir(job);
        make_private_dir(&dir)?;
        Ok(dir)
    }

    /// Opens a file in a job's directory for writing, owner-only (0600), replacing any old one.
    pub async fn create(&self, path: &Path) -> std::io::Result<tokio::fs::File> {
        debug_assert!(path.starts_with(&self.root));
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let file = opts.open(path).await?;
        // A file that already existed keeps its old mode through `open`; fix it.
        set_mode(path, 0o600)?;
        Ok(file)
    }

    /// Moves a completed download into place.
    pub async fn commit_output(&self, job: i64) -> std::io::Result<()> {
        tokio::fs::rename(self.partial_output_path(job), self.output_path(job)).await
    }

    /// Removes a job's files. Missing is fine.
    pub async fn remove_job(&self, job: i64) -> std::io::Result<()> {
        match tokio::fs::remove_dir_all(self.job_dir(job)).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Job ids that have a directory, for the retention sweep (orphans included).
    pub async fn job_ids(&self) -> std::io::Result<Vec<i64>> {
        let mut ids = Vec::new();
        let mut dir = tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = dir.next_entry().await? {
            if let Some(id) = entry.file_name().to_str().and_then(|n| n.parse::<i64>().ok())
                && id > 0
                && entry.file_type().await?.is_dir()
            {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
}

/// Lowercased extension of a (cleaned) model file name, if it's one the worker slices.
pub fn model_extension(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    MODEL_EXTENSIONS.contains(&ext.as_str()).then_some(ext)
}

fn make_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)?;
    // Recursive creation leaves an existing directory's mode alone.
    set_mode(path, 0o700)
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn private_dirs_and_files() {
        let data = tempfile::tempdir().unwrap();
        // An existing, too-open directory is tightened.
        std::fs::create_dir(data.path().join("slicer")).unwrap();
        std::fs::set_permissions(data.path().join("slicer"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = Storage::open(data.path()).unwrap();
        assert_eq!(mode(s.root()), 0o700);

        let dir = s.create_job_dir(7).unwrap();
        assert_eq!(mode(&dir), 0o700);
        let model = s.model_path(7, "STL").unwrap();
        assert!(model.ends_with("7/model.stl"));
        let mut f = s.create(&model).await.unwrap();
        f.write_all(b"solid x").await.unwrap();
        assert_eq!(mode(&model), 0o600);

        // Replacing a file that someone loosened tightens it again.
        std::fs::set_permissions(&model, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(s.create(&model).await.unwrap());
        assert_eq!(mode(&model), 0o600);

        let mut out = s.create(&s.partial_output_path(7)).await.unwrap();
        out.write_all(b"G28\n").await.unwrap();
        s.commit_output(7).await.unwrap();
        assert_eq!(std::fs::read(s.output_path(7)).unwrap(), b"G28\n");
        assert_eq!(mode(&s.output_path(7)), 0o600);
    }

    #[tokio::test]
    async fn lists_and_removes_jobs() {
        let data = tempfile::tempdir().unwrap();
        let s = Storage::open(data.path()).unwrap();
        for id in [3, 12, 5] {
            s.create_job_dir(id).unwrap();
        }
        std::fs::create_dir(s.root().join("junk")).unwrap();
        std::fs::write(s.root().join("99"), b"a file, not a job").unwrap();
        assert_eq!(s.job_ids().await.unwrap(), [3, 5, 12]);
        s.remove_job(5).await.unwrap();
        s.remove_job(5).await.unwrap();
        assert_eq!(s.job_ids().await.unwrap(), [3, 12]);
    }

    #[test]
    fn model_names() {
        assert_eq!(model_extension("Benchy.STL").as_deref(), Some("stl"));
        assert_eq!(model_extension("part.step").as_deref(), Some("step"));
        assert_eq!(model_extension("x.gcode"), None);
        assert_eq!(model_extension("noext"), None);
        let s = Storage {
            root: PathBuf::from("/data/slicer"),
        };
        assert!(s.model_path(1, "exe").is_none());
        assert!(s.model_path(1, "../x").is_none());
    }
}
