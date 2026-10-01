//! Generation preparation runs outside the compositor event loop. Only a
//! validated generation is published; build/load failures leave the active
//! evaluator untouched. No CLR hosting or assembly-unloading contract is needed.
use super::external_transport::terminate_child;
use super::{DecorationEvaluator, DotNetDecorationEvaluator};
use smithay::reexports::calloop::channel::Sender;
use std::os::unix::process::CommandExt;
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(100);
const DEBOUNCE: Duration = Duration::from_millis(400);
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

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

#[derive(Debug, Clone)]
pub struct ReloadOptions {
    pub executable: PathBuf,
    pub config: PathBuf,
    pub project: Option<PathBuf>,
    pub watch_root: PathBuf,
    pub dev: bool,
}

pub enum ReloadEvent {
    Ready(DotNetDecorationEvaluator),
    Failed(String),
}

enum Control {
    Reload,
    Active(DotNetDecorationEvaluator),
}

pub struct DotNetReloadManager {
    commands: mpsc::Sender<Control>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DotNetReloadManager {
    pub fn start(
        options: ReloadOptions,
        current: DotNetDecorationEvaluator,
        events: Sender<ReloadEvent>,
    ) -> Result<Self, String> {
        tracing::info!(dev = options.dev, project = ?options.project, config = %options.config.display(), watch_root = %options.watch_root.display(), "starting C# reload manager");
        let (commands, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let thread = thread::Builder::new()
            .name("dotnet-reload".into())
            .spawn(move || {
                let mut current = current;
                let mut fingerprint = source_fingerprint(&options).ok();
                let mut pending = if options.dev && options.project.is_some() {
                    Some(Instant::now())
                } else {
                    None
                };
                let mut awaiting_swap = false;
                let mut scan_error = None;
                while !cancelled.load(Ordering::Acquire) {
                    match receiver.recv_timeout(POLL) {
                        Ok(Control::Reload) => pending = Some(Instant::now() - DEBOUNCE),
                        Ok(Control::Active(next)) => {
                            current = next;
                            awaiting_swap = false;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if options.dev {
                        match source_fingerprint(&options) {
                            Ok(next) if fingerprint != Some(next) => {
                                fingerprint = Some(next);
                                pending = Some(Instant::now());
                            }
                            Ok(_) => {
                                scan_error = None;
                            }
                            Err(error) => {
                                if scan_error.as_ref() != Some(&error) {
                                    tracing::warn!(%error, "could not scan C# reload inputs");
                                    scan_error = Some(error);
                                }
                            }
                        }
                    }
                    if !awaiting_swap && pending.is_some_and(|since| since.elapsed() >= DEBOUNCE) {
                        pending = None;
                        tracing::info!("preparing C# runtime generation");
                        let result = prepare_generation(&options, &current, &cancelled);
                        if cancelled.load(Ordering::Acquire) {
                            break;
                        }
                        // Saves during build invalidate the candidate. Debounce the
                        // newest content instead of activating an obsolete build.
                        if options.dev
                            && let Ok(latest) = source_fingerprint(&options)
                            && fingerprint != Some(latest)
                        {
                            fingerprint = Some(latest);
                            pending = Some(Instant::now());
                            continue;
                        }
                        let event = match result {
                            Ok(next) => {
                                awaiting_swap = true;
                                ReloadEvent::Ready(next)
                            }
                            Err(error) => ReloadEvent::Failed(error),
                        };
                        if events.send(event).is_err() {
                            break;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            commands,
            stop,
            thread: Some(thread),
        })
    }

    pub fn reload(&self) {
        let _ = self.commands.send(Control::Reload);
    }
    pub fn activated(&self, next: DotNetDecorationEvaluator) {
        let _ = self.commands.send(Control::Active(next));
    }
}

impl Drop for DotNetReloadManager {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn prepare_generation(
    options: &ReloadOptions,
    current: &DotNetDecorationEvaluator,
    stop: &AtomicBool,
) -> Result<DotNetDecorationEvaluator, String> {
    let (directory, config) = if let Some(project) = &options.project {
        let directory = GenerationDirectory::create()?;
        build_project(project, directory.path(), stop)?;
        let config = directory
            .path()
            .join(options.config.file_name().ok_or("config has no filename")?);
        if !config.is_file() {
            return Err(format!(
                "build did not produce {} (match --config filename to the project AssemblyName)",
                config.display()
            ));
        }
        (directory, config)
    } else {
        GenerationDirectory::copy_config(&options.config)?
    };
    let next =
        DotNetDecorationEvaluator::for_generation(options.executable.clone(), config, directory);
    current
        .copy_environment_to(&next)
        .map_err(|e| e.to_string())?;
    next.preload().map_err(|e| e.to_string())?;
    // Initialization commands must not act on the compositor before commit.
    let invocation = next.lifecycle_enable("reload").map_err(|e| e.to_string())?;
    if !invocation.actions.is_empty() {
        return Err("reload OnEnable returned unsupported window actions".into());
    }
    // Run the existing wire decoder and tree validation before retiring old.
    for snapshot in current.window_snapshots().map_err(|e| e.to_string())? {
        if stop.load(Ordering::Acquire) {
            return Err("reload cancelled".into());
        }
        next.evaluate_window_preview(&snapshot, 0)
            .map_err(|e| e.to_string())?;
    }
    Ok(next)
}

fn build_project(project: &Path, output: &Path, stop: &AtomicBool) -> Result<(), String> {
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

fn source_fingerprint(options: &ReloadOptions) -> Result<u64, String> {
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

#[cfg(test)]
mod tests {
    use super::super::{DecorationNode, DecorationNodeKind, WaylandWindowSnapshot};
    use super::*;

    fn snapshot() -> WaylandWindowSnapshot {
        WaylandWindowSnapshot {
            id: "1".into(),
            title: "Kitty 日本語".into(),
            app_id: Some("kitty".into()),
            position: Default::default(),
            rect: Default::default(),
            is_focused: true,
            is_floating: true,
            is_maximized: false,
            is_fullscreen: false,
            is_xwayland: false,
            decoration: Default::default(),
            size_constraints: Default::default(),
            is_resizable: true,
            is_transient: false,
            parent_id: None,
            icon: None,
            interaction: Default::default(),
        }
    }

    fn label(node: &DecorationNode) -> Option<&str> {
        if let DecorationNodeKind::Label(label) = &node.kind {
            return Some(&label.text);
        }
        node.children.iter().find_map(label)
    }

    fn fake_options() -> (Arc<GenerationDirectory>, ReloadOptions) {
        use std::os::unix::fs::PermissionsExt;
        let root = GenerationDirectory::create().unwrap();
        let executable = root.path().join("worker.py");
        fs::write(&executable, r#"#!/usr/bin/env python3
import json, sys
value = open(sys.argv[2]).read()
for line in sys.stdin:
    q = json.loads(line)
    response = dict(kind=q['kind'], requestId=q['requestId'], ok=True)
    if q['kind'] == 'lifecycleEnable' and value == 'init-error':
        response.update(ok=False, error='initialization failed')
    if q['kind'] in ('evaluate', 'evaluatePreview'):
        response['serialized'] = dict(kind='WindowBorder', props={}, children=[dict(kind='Label', props=dict(text=value), children=[]), dict(kind='Window', props={}, children=[])])
        if value == 'bad-tree': response['serialized']['children'] = []
    print(json.dumps(response), flush=True)
"#).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let config = root.path().join("Config.dll");
        fs::write(&config, "old").unwrap();
        let options = ReloadOptions {
            executable,
            config,
            project: None,
            watch_root: root.path().into(),
            dev: false,
        };
        (root, options)
    }

    #[test]
    fn rejected_generation_keeps_old_worker_and_twelve_swaps_reap_resources() {
        let (_root, options) = fake_options();
        let stop = AtomicBool::new(false);
        let mut current = DotNetDecorationEvaluator::new_shadowed(
            options.executable.clone(),
            options.config.clone(),
        );
        current.lifecycle_enable("initial").unwrap();
        current.evaluate_window(&snapshot(), 0).unwrap();
        let old_pid = current.test_process_id().unwrap();
        for invalid in ["init-error", "bad-tree"] {
            fs::write(&options.config, invalid).unwrap();
            assert!(prepare_generation(&options, &current, &stop).is_err());
            assert_eq!(current.test_process_id(), Some(old_pid));
            assert_eq!(
                label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
                Some("old")
            );
        }
        for generation in 0..12 {
            let text = format!("generation-{generation}");
            fs::write(&options.config, &text).unwrap();
            let next = prepare_generation(&options, &current, &stop).unwrap();
            assert_eq!(
                label(&next.evaluate_window(&snapshot(), 0).unwrap().node),
                Some(text.as_str())
            );
            let old_pid = current.test_process_id().unwrap();
            let old_directory = current.test_generation_path().unwrap();
            current.retire("reload");
            drop(current);
            assert!(
                !Path::new("/proc").join(old_pid.to_string()).exists(),
                "retired process leaked"
            );
            assert!(!old_directory.exists(), "retired staging directory leaked");
            current = next;
        }
    }

    #[test]
    fn watcher_debounces_rapid_saves_and_has_one_pending_activation() {
        let (_root, mut options) = fake_options();
        options.dev = true;
        let current = DotNetDecorationEvaluator::new_shadowed(
            options.executable.clone(),
            options.config.clone(),
        );
        let (tx, rx) = smithay::reexports::calloop::channel::channel();
        let manager = DotNetReloadManager::start(options.clone(), current, tx).unwrap();
        // Let the initial fingerprint settle before simulating editor saves.
        thread::sleep(Duration::from_millis(150));
        for i in 0..15 {
            fs::write(&options.config, format!("save-{i}")).unwrap();
            thread::sleep(Duration::from_millis(20));
        }
        let start = Instant::now();
        let next = loop {
            match rx.try_recv() {
                Ok(ReloadEvent::Ready(next)) => break next,
                Ok(ReloadEvent::Failed(error)) => panic!("{error}"),
                Err(_) if start.elapsed() < Duration::from_secs(5) => thread::sleep(POLL),
                Err(error) => panic!("reload did not complete: {error:?}"),
            }
        };
        assert_eq!(
            label(&next.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("save-14")
        );
        manager.reload();
        manager.reload();
        thread::sleep(Duration::from_millis(600));
        assert!(
            rx.try_recv().is_err(),
            "activated two generations without commit acknowledgement"
        );
        manager.activated(next);
    }

    #[test]
    #[ignore = "requires .NET 10 and SHOJI_TEST_DOTNET_RUNTIME; builds temporary config projects"]
    fn real_dotnet_source_reload_rolls_back_build_and_initialization_failures() {
        let root = GenerationDirectory::create().unwrap();
        let project = root.path().join("Config.csproj");
        let api = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dotnet/ShojiWM/ShojiWM.csproj");
        fs::write(&project, format!(r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net10.0</TargetFramework><Nullable>enable</Nullable><ImplicitUsings>enable</ImplicitUsings></PropertyGroup><ItemGroup><ProjectReference Include="{}" /></ItemGroup></Project>"#, api.display())).unwrap();
        fs::write(
            root.path().join("NuGet.Config"),
            "<configuration><packageSources><clear /></packageSources></configuration>",
        )
        .unwrap();
        let source = root.path().join("Config.cs");
        let config_source = |text: &str, fail: bool| {
            format!(
                r#"using ShojiWM;
public sealed class Config : IWindowConfig {{
    public void OnEnable(string reason) {{ {} }}
    public CompositionNode RenderWindow(WaylandWindow window, RenderContext context) =>
        new WindowBorder {{ Children = [new Label {{ Text = "{}" + window.Title }}, new ClientWindow(), new Button {{ OnClick = window.Close }}] }};
}}"#,
                if fail {
                    "throw new InvalidOperationException(\"broken initialization\");"
                } else {
                    ""
                },
                text
            )
        };
        fs::write(&source, config_source("before:", false)).unwrap();
        let initial = root.path().join("initial");
        let stop = AtomicBool::new(false);
        build_project(&project, &initial, &stop).unwrap();
        let options = ReloadOptions {
            executable: std::env::var_os("SHOJI_TEST_DOTNET_RUNTIME")
                .expect("runtime apphost path")
                .into(),
            config: initial.join("Config.dll"),
            project: Some(project),
            watch_root: root.path().into(),
            dev: true,
        };
        let mut current = DotNetDecorationEvaluator::new_shadowed(
            options.executable.clone(),
            options.config.clone(),
        );
        current.lifecycle_enable("initial").unwrap();
        current.evaluate_window(&snapshot(), 0).unwrap();
        let (tx, rx) = smithay::reexports::calloop::channel::channel();
        let manager = DotNetReloadManager::start(options, current.clone(), tx).unwrap();
        let receive = || {
            let started = Instant::now();
            loop {
                match rx.try_recv() {
                    Ok(event) => break event,
                    Err(_) if started.elapsed() < Duration::from_secs(130) => thread::sleep(POLL),
                    Err(error) => panic!("reload did not finish: {error:?}"),
                }
            }
        };
        let activate = |event: ReloadEvent, current: &mut DotNetDecorationEvaluator| {
            let ReloadEvent::Ready(next) = event else {
                panic!("expected successful reload")
            };
            let old_pid = current.test_process_id().unwrap();
            current.retire("reload");
            *current = next.clone();
            manager.activated(next);
            assert!(!Path::new("/proc").join(old_pid.to_string()).exists());
        };
        activate(receive(), &mut current);
        current.evaluate_window(&snapshot(), 0).unwrap();
        fs::write(&source, config_source("after:", false)).unwrap();
        activate(receive(), &mut current);
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("after:Kitty 日本語")
        );
        let old_pid = current.test_process_id().unwrap();
        fs::write(&source, "this is not C# syntax").unwrap();
        assert!(matches!(receive(), ReloadEvent::Failed(error) if error.contains("build failed")));
        assert_eq!(current.test_process_id(), Some(old_pid));
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("after:Kitty 日本語")
        );
        fs::write(&source, config_source("broken:", true)).unwrap();
        assert!(
            matches!(receive(), ReloadEvent::Failed(error) if error.contains("broken initialization"))
        );
        assert_eq!(current.test_process_id(), Some(old_pid));
        fs::write(&source, config_source("recovered:", false)).unwrap();
        activate(receive(), &mut current);
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("recovered:Kitty 日本語")
        );
        // Unexpected worker death is a recoverable protocol error, then an
        // explicit reload creates a fresh process, without a compositor restart.
        Command::new("kill")
            .args(["-KILL", &current.test_process_id().unwrap().to_string()])
            .status()
            .unwrap();
        assert!(current.preload().is_err());
        manager.reload();
        let ReloadEvent::Ready(next) = receive() else {
            panic!("worker crash recovery failed")
        };
        manager.activated(next.clone());
        assert_eq!(
            label(&next.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("recovered:Kitty 日本語")
        );
    }

    #[test]
    fn watch_hash_tracks_source_content_and_ignores_build_outputs() {
        let dir = GenerationDirectory::create().unwrap();
        let file = dir.path().join("Config.cs");
        fs::write(&file, "before").unwrap();
        let options = ReloadOptions {
            executable: "worker".into(),
            config: "Config.dll".into(),
            project: Some("Config.csproj".into()),
            watch_root: dir.path().into(),
            dev: true,
        };
        let before = source_fingerprint(&options).unwrap();
        fs::create_dir(dir.path().join("obj")).unwrap();
        fs::write(dir.path().join("obj/generated.cs"), "ignored").unwrap();
        fs::create_dir(dir.path().join("shoji-dotnet-build")).unwrap();
        fs::write(
            dir.path().join("shoji-dotnet-build/Config.dll"),
            "ignored staging",
        )
        .unwrap();
        assert_eq!(before, source_fingerprint(&options).unwrap());
        fs::write(file, "after!").unwrap();
        assert_ne!(before, source_fingerprint(&options).unwrap());
    }

    #[test]
    fn staging_is_immutable_and_removed_after_last_reference() {
        let source = GenerationDirectory::create().unwrap();
        let path = source.path().join("Config.dll");
        fs::write(&path, "old").unwrap();
        let (stage, assembly) = GenerationDirectory::copy_config(&path).unwrap();
        fs::write(path, "new").unwrap();
        assert_eq!(fs::read(assembly).unwrap(), b"old");
        let location = stage.path().to_path_buf();
        let other = stage.clone();
        drop(stage);
        assert!(location.exists());
        drop(other);
        assert!(!location.exists());
    }
}
