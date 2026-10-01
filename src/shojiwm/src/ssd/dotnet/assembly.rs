//! Immutable configuration/dependency staging, independent of compositor types.
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug)]
pub struct GenerationDirectory(PathBuf);

impl GenerationDirectory {
    pub fn create() -> Result<Arc<Self>, String> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "shoji-dotnet-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            // Private staging directory, including config dependencies.
            use std::os::unix::fs::DirBuilderExt;
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Arc::new(Self(path))),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn copy_config(config: &Path) -> Result<(Arc<Self>, PathBuf), String> {
        let config =
            fs::canonicalize(config).map_err(|e| format!("config {}: {e}", config.display()))?;
        let directory = Self::create()?;
        copy_directory(
            config.parent().ok_or("config has no directory")?,
            directory.path(),
        )?;
        let staged = directory
            .path()
            .join(config.file_name().ok_or("config has no filename")?);
        Ok((directory, staged))
    }
}

impl Drop for GenerationDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_directory(source: &Path, target: &Path) -> Result<(), String> {
    for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let destination = target.join(entry.file_name());
        if kind.is_dir() {
            fs::create_dir(&destination).map_err(|e| e.to_string())?;
            copy_directory(&entry.path(), &destination)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), destination).map_err(|e| e.to_string())?;
        } else {
            return Err(format!(
                "unsupported config dependency: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}
