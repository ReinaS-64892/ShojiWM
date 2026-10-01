//! Generation preparation runs outside the compositor event loop. Only a
//! validated generation is published; build/load failures leave the active
//! evaluator untouched. Reload always prepares on the existing managed host.
#[cfg(test)]
use super::super::DecorationEvaluator;
use super::{
    DotNetDecorationEvaluator,
    assembly::GenerationDirectory,
    source::{build_project, source_fingerprint},
};
use smithay::reexports::calloop::channel::Sender;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const POLL: Duration = Duration::from_millis(100);
const DEBOUNCE: Duration = Duration::from_millis(400);

pub use super::source::ReloadOptions;

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
    if !current.has_active_host() {
        return Err("runtime host unavailable; ShojiWM restart required".into());
    }
    let next = current
        .prepare_assembly(config, directory)
        .map_err(|e| e.to_string())?;
    // Run the existing wire decoder and tree validation before retiring old.
    for snapshot in current.window_snapshots().map_err(|e| e.to_string())? {
        if stop.load(Ordering::Acquire) {
            return Err("reload cancelled".into());
        }
        next.validate_candidate(&snapshot)
            .map_err(|e| e.to_string())?;
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::super::super::{DecorationNode, DecorationNodeKind, WaylandWindowSnapshot};
    use super::*;
    use std::{fs, path::Path};

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
        let component: std::path::PathBuf = std::env::var_os("SHOJI_TEST_DOTNET_RUNTIME")
            .expect("runtime bootstrap DLL path")
            .into();
        let options = ReloadOptions {
            config: initial.join("Config.dll"),
            project: Some(project),
            watch_root: root.path().into(),
            dev: true,
        };
        let mut current =
            DotNetDecorationEvaluator::new_shadowed(component, options.config.clone());
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
            let previous = current.clone();
            next.activate_prepared().unwrap();
            *current = next.clone();
            manager.activated(next);
            assert!(current.shares_host_with(&previous));
        };
        activate(receive(), &mut current);
        current.evaluate_window(&snapshot(), 0).unwrap();
        fs::write(&source, config_source("after:", false)).unwrap();
        activate(receive(), &mut current);
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("after:Kitty 日本語")
        );
        let previous = current.clone();
        fs::write(&source, "this is not C# syntax").unwrap();
        assert!(matches!(receive(), ReloadEvent::Failed(error) if error.contains("build failed")));
        assert!(current.shares_host_with(&previous));
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("after:Kitty 日本語")
        );
        fs::write(&source, config_source("broken:", true)).unwrap();
        assert!(
            matches!(receive(), ReloadEvent::Failed(error) if error.contains("broken initialization"))
        );
        assert!(current.shares_host_with(&previous));
        fs::write(&source, config_source("recovered:", false)).unwrap();
        activate(receive(), &mut current);
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("recovered:Kitty 日本語")
        );
        fs::write(&source, "using ShojiWM; public sealed class Config : IWindowConfig { public CompositionNode RenderWindow(WaylandWindow window, RenderContext context) => new WindowBorder(); }").unwrap();
        assert!(
            matches!(receive(), ReloadEvent::Failed(error) if error.contains("composition tree"))
        );
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("recovered:Kitty 日本語")
        );
        // Rapid source saves coalesce into a single serialized build and candidate.
        for i in 0..15 {
            fs::write(&source, config_source(&format!("save-{i}:"), false)).unwrap();
            thread::sleep(Duration::from_millis(20));
        }
        let ReloadEvent::Ready(next) = receive() else {
            panic!("rapid saves failed")
        };
        assert!(current.shares_host_with(&next));
        manager.reload();
        manager.reload();
        thread::sleep(Duration::from_millis(600));
        assert!(
            rx.try_recv().is_err(),
            "second candidate prepared before activation acknowledgement"
        );
        next.activate_prepared().unwrap();
        current = next;
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("save-14:Kitty 日本語")
        );
        // Stop the watcher before exercising transactions explicitly.
        drop(manager);
        drop(previous);
        let mut staged_options = ReloadOptions {
            config: current.test_generation_path().unwrap().join("Config.dll"),
            project: None,
            watch_root: root.path().into(),
            dev: false,
        };
        let discarded = prepare_generation(&staged_options, &current, &stop).unwrap();
        let discarded_path = discarded.test_generation_path().unwrap();
        drop(discarded);
        assert!(!discarded_path.exists());
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("save-14:Kitty 日本語")
        );
        for _ in 0..12 {
            let old_directory = current.test_generation_path().unwrap();
            let next = prepare_generation(&staged_options, &current, &stop).unwrap();
            assert!(current.shares_host_with(&next));
            next.activate_prepared().unwrap();
            current = next;
            assert!(!old_directory.exists(), "old staging leaked");
            staged_options.config = current.test_generation_path().unwrap().join("Config.dll");
        }
    }

    #[test]
    fn watch_hash_tracks_source_content_and_ignores_build_outputs() {
        let dir = GenerationDirectory::create().unwrap();
        let file = dir.path().join("Config.cs");
        fs::write(&file, "before").unwrap();
        let options = ReloadOptions {
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
