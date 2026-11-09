use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::id;

use crate::config::runtime_dir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub pid: u32,
    pub port: u16,
}

#[derive(Debug)]
pub struct InstanceLock {
    path: PathBuf,
    owned: bool,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn instance_path() -> Result<PathBuf, String> {
    let dir = runtime_dir()?;
    fs::create_dir_all(&dir).map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    Ok(dir.join("instance.json"))
}

/// Take the instance file, or report the dashboard that is already running.
pub fn claim(path: &Path, port: u16) -> Result<Result<InstanceLock, Running>, String> {
    claim_with(path, port, id(), process_alive)
}

fn claim_with(
    path: &Path,
    port: u16,
    pid: u32,
    alive: impl Fn(u32) -> bool,
) -> Result<Result<InstanceLock, Running>, String> {
    if let Some(running) = read_running(path)? {
        if running.pid != pid && alive(running.pid) {
            return Ok(Err(running));
        }
        let _ = fs::remove_file(path);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("could not create {}: {err}", path.display()))?;
    write_record(&mut file, pid, port)?;
    Ok(Ok(InstanceLock {
        path: path.to_path_buf(),
        owned: true,
    }))
}

pub fn read_running(path: &Path) -> Result<Option<Running>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let mut file =
        File::open(path).map_err(|err| format!("could not read {}: {err}", path.display()))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("invalid instance file: {err}"))?;
    let pid = value
        .get("pid")
        .and_then(|item| item.as_u64())
        .and_then(|item| u32::try_from(item).ok())
        .ok_or("instance file is missing pid")?;
    let port = value
        .get("port")
        .and_then(|item| item.as_u64())
        .and_then(|item| u16::try_from(item).ok())
        .ok_or("instance file is missing port")?;
    Ok(Some(Running { pid, port }))
}

fn write_record(file: &mut File, pid: u32, port: u16) -> Result<(), String> {
    let body = format!("{{\"pid\":{pid},\"port\":{port}}}\n");
    file.write_all(body.as_bytes())
        .map_err(|err| format!("could not write instance file: {err}"))
}

/// True when `pid` names a live process (signal 0 checks without sending).
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    libc_kill(pid as i32, 0) == 0
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_claim_reports_the_running_dashboard() {
        let dir = std::env::temp_dir().join(format!("whatsup-instance-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("instance.json");
        let first = claim_with(&path, 47654, 42, |_| true).unwrap().unwrap();
        let second = claim_with(&path, 1, 99, |_| true).unwrap().unwrap_err();
        assert_eq!(
            second,
            Running {
                pid: 42,
                port: 47654
            }
        );
        drop(first);
        let after = claim_with(&path, 47654, 7, |_| false).unwrap().unwrap();
        assert!(after.owned);
        drop(after);
        let _ = fs::remove_dir_all(&dir);
    }
}
