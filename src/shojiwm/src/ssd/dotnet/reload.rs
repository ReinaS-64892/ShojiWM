//! Generation preparation runs outside the compositor event loop. Only a
//! validated generation is published; build/load failures leave the active
//! evaluator untouched. A healthy worker prepares a collectible assembly; only
//! initial load or crash recovery starts a new worker process.
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
    let next = if current.has_active_worker() {
        current
            .prepare_assembly(config, directory)
            .map_err(|e| e.to_string())?
    } else {
        let next = DotNetDecorationEvaluator::for_generation(
            options.executable.clone(),
            config,
            directory,
        );
        current
            .copy_environment_to(&next)
            .map_err(|e| e.to_string())?;
        next.preload().map_err(|e| e.to_string())?;
        let invocation = next.lifecycle_enable("reload").map_err(|e| e.to_string())?;
        if !invocation.actions.is_empty() {
            return Err("reload OnEnable returned unsupported window actions".into());
        }
        next
    };
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
    use std::{fs, path::Path, process::Command};

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
import json, sys, os
value = open(sys.argv[2]).read()
candidate = None
candidate_path = None
for line in sys.stdin:
    q = json.loads(line)
    response = dict(kind=q['kind'], requestId=q['requestId'], ok=True)
    if q['kind'] == 'prepareAssembly':
        candidate_path = q['configPath']
        candidate = open(candidate_path).read()
        if candidate == 'init-error':
            candidate = None
            response.update(ok=False, error='initialization failed')
    if q['kind'] == 'commitAssembly':
        value, candidate = candidate, None
    if q['kind'] == 'abortAssembly':
        if candidate_path and not os.path.exists(candidate_path):
            value = 'abort-lost-dependencies'
            response.update(ok=False, error='candidate dependencies removed before cleanup')
        candidate = None
        candidate_path = None
    text = candidate if q['kind'] == 'evaluateCandidatePreview' else value
    if q['kind'] in ('evaluate', 'evaluatePreview', 'evaluateCandidatePreview'):
        response['serialized'] = dict(kind='WindowBorder', props={}, children=[dict(kind='Label', props=dict(text=text), children=[]), dict(kind='Window', props={}, children=[])])
        if text == 'bad-tree': response['serialized']['children'] = []
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
    fn rejected_assembly_keeps_worker_and_twelve_swaps_release_staging() {
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
            next.activate_prepared().unwrap();
            assert_eq!(
                label(&next.evaluate_window(&snapshot(), 0).unwrap().node),
                Some(text.as_str())
            );
            let old_pid = current.test_process_id().unwrap();
            let old_directory = current.test_generation_path().unwrap();
            drop(current);
            assert_eq!(
                next.test_process_id(),
                Some(old_pid),
                "reload restarted worker"
            );
            assert!(!old_directory.exists(), "retired staging directory leaked");
            current = next;
        }
    }

    #[test]
    fn dropping_prepared_candidate_aborts_without_replacing_active_assembly() {
        let (_root, options) = fake_options();
        let current = DotNetDecorationEvaluator::new_shadowed(
            options.executable.clone(),
            options.config.clone(),
        );
        current.lifecycle_enable("initial").unwrap();
        current.evaluate_window(&snapshot(), 0).unwrap();
        fs::write(&options.config, "discarded").unwrap();
        let next = prepare_generation(&options, &current, &AtomicBool::new(false)).unwrap();
        let directory = next.test_generation_path().unwrap();
        assert_eq!(current.test_process_id(), next.test_process_id());
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("old")
        );
        drop(next);
        assert!(!directory.exists());
        assert_eq!(
            label(&current.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("old")
        );
        fs::write(&options.config, "accepted").unwrap();
        let next = prepare_generation(&options, &current, &AtomicBool::new(false)).unwrap();
        next.activate_prepared().unwrap();
        assert_eq!(
            label(&next.evaluate_window(&snapshot(), 0).unwrap().node),
            Some("accepted")
        );
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
        next.activate_prepared().unwrap();
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
            next.activate_prepared().unwrap();
            *current = next.clone();
            manager.activated(next);
            assert_eq!(
                current.test_process_id(),
                Some(old_pid),
                "assembly reload restarted CLR"
            );
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
