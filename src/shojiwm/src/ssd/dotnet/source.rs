//! Source observation and serialized build inputs; no assembly/CLR lifecycle.
use super::process::terminate_child;
use std::os::unix::process::CommandExt;
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
const POLL: Duration = Duration::from_millis(100);
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct ReloadOptions {
    pub config: PathBuf,
    pub project: Option<PathBuf>,
    pub watch_root: PathBuf,
    pub dev: bool,
}

pub(super) fn build_project(
    project: &Path,
    output: &Path,
    stop: &AtomicBool,
) -> Result<(), String> {
    // Keep diagnostics out of protocol streams and include a bounded tail in
    // the compositor's error log/overlay when a build fails.
    fs::create_dir_all(output).map_err(|e| e.to_string())?;
    let log_path = output.join("build.log");
    let log = fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let error_log = log.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new("dotnet")
        .process_group(0)
        .arg("build")
        .arg(project)
        .args(["--configuration", "Release", "--output"])
        .arg(output)
        .args([
            "--tl:off",
            "--disable-build-servers",
            "-m:1",
            "-p:BuildInParallel=false",
            "-p:UseSharedCompilation=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .spawn()
        .map_err(|e| format!("could not start dotnet build: {e}"))?;
    let start = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) || start.elapsed() >= BUILD_TIMEOUT {
            terminate_child(&mut child);
            return Err(format!(
                "dotnet build cancelled or exceeded 120s deadline\n{}",
                build_diagnostics(&log_path)
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "dotnet build failed: {status}; keeping current runtime\n{}",
                        build_diagnostics(&log_path)
                    ))
                };
            }
            Ok(None) => thread::sleep(POLL),
            Err(error) => {
                terminate_child(&mut child);
                return Err(error.to_string());
            }
        }
    }
}

fn build_diagnostics(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let result = (|| -> std::io::Result<Vec<u8>> {
        let mut file = fs::File::open(path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
        let mut bytes = Vec::new();
        file.take(8192).read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    result
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

pub(super) fn source_fingerprint(options: &ReloadOptions) -> Result<u64, String> {
    let mut hash = DefaultHasher::new();
    if options.project.is_some() {
        hash_sources(&options.watch_root, &mut hash)?;
    } else {
        // Watch the assembly plus adjacent dependencies, not only its mtime.
        hash_sources(
            options.config.parent().ok_or("config has no directory")?,
            &mut hash,
        )?;
    }
    Ok(hash.finish())
}

fn hash_sources(root: &Path, hash: &mut DefaultHasher) -> Result<(), String> {
    let mut entries = fs::read_dir(root)
        .map_err(|e| format!("{}: {e}", root.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("shoji-dotnet-")
                && !matches!(
                    entry.file_name().to_str(),
                    Some("bin" | "obj" | ".git" | "Generated" | "node_modules" | "target")
                )
            {
                hash_sources(&path, hash)?;
            }
        } else if kind.is_file()
            && matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("cs" | "csproj" | "props" | "targets" | "json" | "dll")
            )
        {
            path.hash(hash);
            fs::read(path).map_err(|e| e.to_string())?.hash(hash);
        }
    }
    Ok(())
}
