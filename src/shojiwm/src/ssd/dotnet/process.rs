//! Build subprocess cleanup only; managed hosting has no process boundary.
use std::process::{Child, Command, Stdio};

/// Only target a process group created for our own child. This also retires
/// compiler/user child processes that have not explicitly detached themselves.
pub(super) fn terminate_child(child: &mut Child) {
    // Callers invoke this exactly once, before reaping the child. Until wait,
    // its pid cannot be reused even if the build has already exited.
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;

    #[test]
    fn build_cancellation_reaps_child_and_stops_process_group() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 60 & echo $!; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut descendant = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut descendant)
            .unwrap();
        terminate_child(&mut child);
        assert!(!child.wait().unwrap().success());
        // An orphan may briefly remain as a zombie until init reaps it. It
        // must not remain running after our own build group is terminated.
        let path = format!("/proc/{}/stat", descendant.trim());
        for _ in 0..100 {
            match std::fs::read_to_string(&path) {
                Err(_) => return,
                Ok(stat) if stat.split(") ").nth(1).unwrap().starts_with('Z') => return,
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        panic!("build descendant remained running");
    }
}
