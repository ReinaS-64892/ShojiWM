//! In-process CoreCLR hosting. Only bootstrap function pointers cross this
//! boundary. Config objects stay managed; all calls for a host run on one thread.
use netcorehost::{hostfxr::Hostfxr, pdcstr, pdcstring::PdCString};
use std::{
    path::{Path, PathBuf},
    sync::{OnceLock, mpsc},
    thread::{self, JoinHandle},
};

pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

type Create = extern "system" fn(*const u8, i32, *mut usize, *mut *mut u8, *mut i32) -> i32;
type Invoke = extern "system" fn(usize, *const u8, i32, *mut *mut u8, *mut i32) -> i32;
type Destroy = extern "system" fn(usize, *mut *mut u8, *mut i32) -> i32;
type Free = extern "system" fn(*mut u8);

#[derive(Clone, Copy)]
struct Bootstrap {
    create: Create,
    invoke: Invoke,
    destroy: Destroy,
    free: Free,
}
static BOOTSTRAP: OnceLock<Result<(PathBuf, Bootstrap), String>> = OnceLock::new();

fn component_path(path: &Path) -> Result<PathBuf, String> {
    // Accept the previous apphost spelling by resolving its sibling DLL.
    let dll = if path.extension().is_some_and(|x| x == "dll") {
        path.to_owned()
    } else {
        // "ShojiWM.Runtime" is an apphost basename, not a .Runtime extension
        // to replace. Its sibling is ShojiWM.Runtime.dll, not ShojiWM.dll.
        let mut name = path.as_os_str().to_os_string();
        name.push(".dll");
        PathBuf::from(name)
    };
    let resolved = if dll.components().count() == 1 && !dll.is_file() {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(&dll))
            .find(|p| p.is_file())
            .unwrap_or(dll)
    } else {
        dll
    };
    resolved
        .canonicalize()
        .map_err(|e| format!("bootstrap component {}: {e}", resolved.display()))
}

fn find_hostfxr() -> Result<PathBuf, String> {
    let roots = if let Some(root) = std::env::var_os("DOTNET_ROOT") {
        vec![PathBuf::from(root)]
    } else {
        let mut roots = vec![
            PathBuf::from("/usr/share/dotnet"),
            PathBuf::from("/usr/lib/dotnet"),
            PathBuf::from("/opt/dotnet"),
        ];
        if let Some(dotnet) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("dotnet"))
            .find_map(|p| p.canonicalize().ok())
        {
            if let Some(parent) = dotnet.parent() {
                roots.insert(0, parent.to_owned());
            }
        }
        roots
    };
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root.join("host/fxr")) else {
            continue;
        };
        let mut versions: Vec<_> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let version = entry
                    .file_name()
                    .to_string_lossy()
                    .split('.')
                    .map(str::parse::<u32>)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                let path = entry.path().join("libhostfxr.so");
                path.is_file().then_some((version, path))
            })
            .collect();
        versions.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some((_, path)) = versions.pop() {
            return Ok(path);
        }
    }
    Err(
        "hostfxr discovery failed: set DOTNET_ROOT to an installed .NET root containing host/fxr"
            .into(),
    )
}

fn bootstrap(path: &Path) -> Result<Bootstrap, String> {
    let path = component_path(path)?;
    let loaded = BOOTSTRAP.get_or_init(|| {
        let runtimeconfig = path.with_extension("runtimeconfig.json");
        if !runtimeconfig.is_file() { return Err(format!("missing {}", runtimeconfig.display())); }
        // Intentionally process lifetime: retain the library even if initialization
        // fails after it has loaded. Context close is not CLR shutdown/dlclose.
        let library = Hostfxr::load_from_path(find_hostfxr()?).map_err(|e| format!("hostfxr load: {e}"))?;
        let library = Box::leak(Box::new(library));
        let config = PdCString::from_os_str(&runtimeconfig).map_err(|e| e.to_string())?;
        let context = library.initialize_for_runtime_config(config).map_err(|e| format!("CoreCLR initialization: {e}"))?;
        let assembly = PdCString::from_os_str(&path).map_err(|e| e.to_string())?;
        let loader = context.get_delegate_loader_for_assembly(assembly).map_err(|e| format!("bootstrap loader: {e}"))?;
        let name = pdcstr!("ShojiWM.Runtime.NativeEntryPoint, ShojiWM.Runtime");
        // Signatures exactly match the permanent bootstrap's blittable ABI.
        let create = *loader.get_function_with_unmanaged_callers_only::<fn(*const u8, i32, *mut usize, *mut *mut u8, *mut i32) -> i32>(name, pdcstr!("CreateHost")).map_err(|e| e.to_string())?;
        let invoke = *loader.get_function_with_unmanaged_callers_only::<fn(usize, *const u8, i32, *mut *mut u8, *mut i32) -> i32>(name, pdcstr!("Invoke")).map_err(|e| e.to_string())?;
        let destroy = *loader.get_function_with_unmanaged_callers_only::<fn(usize, *mut *mut u8, *mut i32) -> i32>(name, pdcstr!("DestroyHost")).map_err(|e| e.to_string())?;
        let free = *loader.get_function_with_unmanaged_callers_only::<fn(*mut u8)>(name, pdcstr!("FreeBuffer")).map_err(|e| e.to_string())?;
        Ok((path.clone(), Bootstrap { create, invoke, destroy, free }))
    });
    match loaded {
        Ok((active, api)) if active == &path => Ok(*api),
        Ok(_) => Err("bootstrap path changed; ShojiWM restart required".into()),
        Err(error) => Err(format!("{error}; ShojiWM restart required")),
    }
}

struct ResponseBuffer {
    pointer: *mut u8,
    length: i32,
    free: Free,
}
impl ResponseBuffer {
    fn copy(&self) -> Result<Vec<u8>, String> {
        if self.length < 0
            || self.length as usize > MAX_MESSAGE_BYTES
            || (self.pointer.is_null() && self.length != 0)
        {
            return Err("invalid native response buffer or message exceeds 8 MiB limit".into());
        }
        if self.length == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: bootstrap owns a readable allocation of length bytes until
        // FreeBuffer. We copy before the RAII guard releases it.
        Ok(unsafe { std::slice::from_raw_parts(self.pointer, self.length as usize) }.to_vec())
    }
}
impl Drop for ResponseBuffer {
    fn drop(&mut self) {
        (self.free)(self.pointer);
    }
}

struct ManagedHost {
    handle: usize,
    api: Bootstrap,
}
impl ManagedHost {
    fn create(api: Bootstrap, path: &Path) -> Result<Self, String> {
        let bytes = path.to_str().ok_or("config path must be UTF-8")?.as_bytes();
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("config path too long".into());
        }
        let mut handle = 0;
        let mut output = ResponseBuffer {
            pointer: std::ptr::null_mut(),
            length: 0,
            free: api.free,
        };
        let status = (api.create)(
            bytes.as_ptr(),
            bytes.len() as i32,
            &mut handle,
            &mut output.pointer,
            &mut output.length,
        );
        if status != 0 {
            return Err(format!(
                "managed host creation ({status}): {}",
                String::from_utf8_lossy(&output.copy()?)
            ));
        }
        if handle == 0 {
            return Err("managed bootstrap returned null host handle".into());
        }
        Ok(Self { handle, api })
    }
    fn exchange(&self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("request exceeds 8 MiB message limit".into());
        }
        let mut output = ResponseBuffer {
            pointer: std::ptr::null_mut(),
            length: 0,
            free: self.api.free,
        };
        let status = (self.api.invoke)(
            self.handle,
            bytes.as_ptr(),
            bytes.len() as i32,
            &mut output.pointer,
            &mut output.length,
        );
        let bytes = output.copy()?;
        if status != 0 {
            return Err(format!(
                "managed ABI invocation ({status}): {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        Ok(bytes)
    }
}
impl Drop for ManagedHost {
    fn drop(&mut self) {
        let mut output = ResponseBuffer {
            pointer: std::ptr::null_mut(),
            length: 0,
            free: self.api.free,
        };
        let status = (self.api.destroy)(self.handle, &mut output.pointer, &mut output.length);
        if status != 0 {
            eprintln!(
                "ShojiWM .NET host disposal ({status}): {:?}",
                output
                    .copy()
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
            );
        }
    }
}

enum Command {
    Invoke(Vec<u8>, mpsc::SyncSender<Result<Vec<u8>, String>>),
}
#[derive(Debug)]
pub struct InProcessDotNetHost {
    commands: Option<mpsc::Sender<Command>>,
    thread: Option<JoinHandle<()>>,
}
impl InProcessDotNetHost {
    pub fn start(component: &Path, config: &Path) -> Result<Self, String> {
        let component = component.to_owned();
        let config = config.to_owned();
        let (commands, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("dotnet-runtime".into())
            .spawn(move || {
                let result =
                    bootstrap(&component).and_then(|api| ManagedHost::create(api, &config));
                let host = match result {
                    Ok(host) => host,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(Command::Invoke(bytes, reply)) = receiver.recv() {
                    let _ = reply.send(host.exchange(&bytes));
                }
                // Host destruction on its original thread, then thread joins. No
                // timeout, abandoned execution, CLR restart or child process.
            })
            .map_err(|e| e.to_string())?;
        let ready = ready_rx.recv().map_err(|e| e.to_string()).and_then(|r| r);
        if let Err(error) = ready {
            let _ = thread.join();
            return Err(error);
        }
        Ok(Self {
            commands: Some(commands),
            thread: Some(thread),
        })
    }
    pub fn exchange(&mut self, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("request exceeds 8 MiB message limit".into());
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .ok_or("runtime host unavailable; ShojiWM restart required")?
            .send(Command::Invoke(bytes, tx))
            .map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }
    pub fn stop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for InProcessDotNetHost {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_dotted_apphost_name_resolves_bootstrap_not_public_api() {
        let root = std::env::temp_dir().join(format!(
            "shoji-bootstrap-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let bootstrap = root.join("ShojiWM.Runtime.dll");
        std::fs::write(&bootstrap, []).unwrap();
        std::fs::write(root.join("ShojiWM.dll"), []).unwrap();
        assert_eq!(
            component_path(&root.join("ShojiWM.Runtime")).unwrap(),
            bootstrap.canonicalize().unwrap()
        );
        assert_eq!(
            component_path(&bootstrap).unwrap(),
            bootstrap.canonicalize().unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
