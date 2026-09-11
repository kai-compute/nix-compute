use anyhow::{ensure, Context};
use std::{
    io::Read,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub struct Cancellation {
    pub flag: Arc<AtomicBool>,
    registrations: Vec<signal_hook::SigId>,
}
impl Cancellation {
    pub fn install() -> anyhow::Result<Self> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut result = Self {
            flag,
            registrations: vec![],
        };
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            result
                .registrations
                .push(signal_hook::flag::register(signal, result.flag.clone())?);
        }
        Ok(result)
    }
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        for id in &self.registrations {
            signal_hook::low_level::unregister(*id);
        }
    }
}

struct Group(Child);
impl Drop for Group {
    fn drop(&mut self) {
        // The workload gets a new process group; kill descendants even if its leader exited.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

pub fn run(
    command: &mut Command,
    timeout: Option<u64>,
    cancelled: &AtomicBool,
) -> anyhow::Result<i32> {
    command.process_group(0).stdin(Stdio::null());
    let mut group = Group(command.spawn().context("starting workload")?);
    wait(&mut group, timeout, cancelled)
}

fn wait(group: &mut Group, timeout: Option<u64>, cancelled: &AtomicBool) -> anyhow::Result<i32> {
    let started = Instant::now();
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(130);
        }
        if let Some(status) = group.0.try_wait()? {
            return Ok(status.code().unwrap_or(1));
        }
        if timeout.is_some_and(|t| started.elapsed() >= Duration::from_secs(t)) {
            return Ok(124);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

pub fn capture(command: &mut Command, timeout: u64) -> anyhow::Result<Vec<u8>> {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut group = Group(command.spawn()?);
    let stdout = group.0.stdout.take().unwrap();
    let stderr = group.0.stderr.take().unwrap();
    let read = |stream: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            stream
                .take(4 * 1024 * 1024)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let code = wait(&mut group, Some(timeout), &AtomicBool::new(false));
    drop(group);
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader failed"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
    ensure!(
        code? == 0,
        "command failed or timed out: {}",
        String::from_utf8_lossy(&stderr)
    );
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timeout_and_cancellation_kill_descendants() {
        for cancel in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("leaked");
            let mut command = Command::new("sh");
            command
                .args(["-c", "(sleep 2; touch \"$1\") & wait", "test"])
                .arg(&marker);
            let flag = AtomicBool::new(cancel);
            assert_eq!(
                run(&mut command, Some(1), &flag).unwrap(),
                if cancel { 130 } else { 124 }
            );
            thread::sleep(Duration::from_millis(1300));
            assert!(!marker.exists());
        }
    }
}
