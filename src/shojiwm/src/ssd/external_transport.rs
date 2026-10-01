//! Bounded synchronous exchanges over NDJSON. Pipe I/O runs off the compositor
//! thread so a worker that stops reading stdin cannot defeat the deadline.
use std::os::unix::process::CommandExt;
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, SyncSender},
    time::Duration,
};

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct ExternalTransport {
    child: Child,
    requests: Option<SyncSender<Vec<u8>>>,
    responses: Receiver<Result<Vec<u8>, String>>,
    failed: bool,
}

impl ExternalTransport {
    #[cfg(test)]
    pub(super) fn process_id(&self) -> u32 {
        self.child.id()
    }
    pub fn start(executable: &Path, config: &Path) -> Result<Self, String> {
        let mut command = Command::new(executable);
        command.arg("--config").arg(config);
        Self::spawn(command)
    }

    pub(super) fn spawn(mut command: Command) -> Result<Self, String> {
        command.process_group(0);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("could not start external runtime: {error}"))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (request_tx, request_rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("dotnet-runtime-io".into())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                while let Ok(request) = request_rx.recv() {
                    let result = (|| {
                        stdin.write_all(&request).map_err(|e| e.to_string())?;
                        stdin.write_all(b"\n").map_err(|e| e.to_string())?;
                        stdin.flush().map_err(|e| e.to_string())?;
                        let mut frame = Vec::new();
                        stdout
                            .by_ref()
                            .take((MAX_FRAME_BYTES + 1) as u64)
                            .read_until(b'\n', &mut frame)
                            .map_err(|e| e.to_string())?;
                        if frame.len() > MAX_FRAME_BYTES {
                            return Err("external runtime response exceeds frame limit".into());
                        }
                        if frame.last() != Some(&b'\n') {
                            return Err(
                                "external runtime exited or returned an incomplete NDJSON frame"
                                    .into(),
                            );
                        }
                        Ok(frame)
                    })();
                    let failed = result.is_err();
                    if response_tx.send(result).is_err() || failed {
                        break;
                    }
                }
            });
        if let Err(error) = thread {
            terminate_child(&mut child);
            return Err(error.to_string());
        }
        Ok(Self {
            child,
            requests: Some(request_tx),
            responses: response_rx,
            failed: false,
        })
    }

    pub fn exchange(&mut self, request: Vec<u8>, timeout: Duration) -> Result<Vec<u8>, String> {
        if self.failed {
            return Err("external runtime is unavailable; reload the config to retry".into());
        }
        if request.len() >= MAX_FRAME_BYTES {
            return Err("external runtime request exceeds frame limit".into());
        }
        let result = self
            .requests
            .as_ref()
            .ok_or("external runtime stopped".to_string())
            .and_then(|tx| tx.try_send(request).map_err(|e| e.to_string()))
            .and_then(|()| {
                self.responses.recv_timeout(timeout).map_err(|e| {
                    format!("external runtime response failed (deadline {timeout:?}): {e}")
                })
            })
            .and_then(|result| result);
        if result.is_err() {
            self.stop();
        }
        result
    }

    pub fn stop(&mut self) {
        if self.failed {
            return;
        }
        self.failed = true;
        self.requests.take();
        terminate_child(&mut self.child);
    }
}

/// Only target a process group created for our own child. This also retires
/// compiler/user child processes that have not explicitly detached themselves.
pub(super) fn terminate_child(child: &mut Child) {
    // Callers invoke this exactly once, before reaping the child. Until wait,
    // its pid cannot be reused even if the worker has already exited.
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for ExternalTransport {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_worker_and_partial_frame_fail_without_hanging() {
        for script in ["exit 7", "read line; printf '{bad'"] {
            let mut command = Command::new("sh");
            command.arg("-c").arg(script);
            let mut worker = ExternalTransport::spawn(command).unwrap();
            assert!(
                worker
                    .exchange(b"{}".to_vec(), Duration::from_secs(1))
                    .is_err()
            );
        }
    }

    #[test]
    fn worker_that_does_not_read_stdin_has_bounded_write() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 10");
        let mut worker = ExternalTransport::spawn(command).unwrap();
        let before = std::time::Instant::now();
        assert!(
            worker
                .exchange(vec![b'x'; 1024 * 1024], Duration::from_millis(100))
                .is_err()
        );
        assert!(before.elapsed() < Duration::from_secs(2));
        assert!(
            worker
                .exchange(b"{}".to_vec(), Duration::from_millis(100))
                .is_err()
        );
    }

    #[test]
    fn oversized_response_is_rejected() {
        let mut command = Command::new("python3");
        command.arg("-u").arg("-c").arg(
            "import sys; sys.stdin.readline(); sys.stdout.write('x' * (8 * 1024 * 1024 + 1)); sys.stdout.flush()",
        );
        let mut worker = ExternalTransport::spawn(command).unwrap();
        let error = worker
            .exchange(b"{}".to_vec(), Duration::from_secs(2))
            .unwrap_err();
        assert!(error.contains("frame limit"), "{error}");
    }
}
