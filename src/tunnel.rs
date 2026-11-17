//! One supervised `ssh -N -D` SOCKS tunnel per remote machine.
//!
//! Each tunnel is a tokio task that owns its ssh child (killed when the task
//! is dropped), restarts it with backoff when it exits, and keeps the last
//! line ssh printed to stderr so the dashboard can say why a remote is
//! offline.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinHandle;

use crate::config::Remote;

const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);
/// A run at least this long counts as healthy and resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Local socket paths at least this long cannot be bound on macOS
/// (`sun_path` is 104 bytes), and a forward that cannot bind would take the
/// whole tunnel down (`ExitOnForwardFailure`).
const MAX_SOCKET_PATH: usize = 100;

/// Where the remote's herdr socket is forwarded to on this machine, if the
/// remote has one and the path is short enough to bind.
pub fn herdr_forward_path(socket_dir: &Path, remote: &Remote) -> Option<PathBuf> {
    remote.herdr_socket.as_ref()?;
    let path = socket_dir.join(format!("herdr-{}.sock", remote.socks_port));
    (path.as_os_str().len() < MAX_SOCKET_PATH).then_some(path)
}

/// The ssh arguments for `remote`'s tunnel; `forward` is the local end of
/// the herdr socket forward (see [`herdr_forward_path`]).
pub fn ssh_args(remote: &Remote, forward: Option<&Path>) -> Vec<String> {
    let mut args = base_args(remote);
    if let (Some(local), Some(remote_socket)) = (forward, &remote.herdr_socket) {
        args.extend([
            "-o".to_string(),
            "StreamLocalBindUnlink=yes".to_string(),
            "-L".to_string(),
            format!("{}:{remote_socket}", local.display()),
        ]);
    }
    args.push(remote.ssh.clone());
    args
}

fn base_args(remote: &Remote) -> Vec<String> {
    [
        "-N".to_string(),
        "-D".to_string(),
        format!("127.0.0.1:{}", remote.socks_port),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=5".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=2".to_string(),
    ]
    .into()
}

/// ssh prints a line for every SOCKS CONNECT the remote refused; with one
/// probe per down port every refresh, those are routine, not errors.
pub fn is_probe_noise(line: &str) -> bool {
    line.strip_prefix("channel ")
        .and_then(|rest| rest.split_once(": open failed:"))
        .is_some_and(|(number, _)| number.chars().all(|ch| ch.is_ascii_digit()))
}

/// Whether `command` (a process's full command line) is a tunnel WhatsUp
/// started on `socks_port`, to any host. A WhatsUp that was force-quit leaves
/// its ssh running, holding the port.
pub fn is_our_tunnel(command: &str, socks_port: u16) -> bool {
    let Some((program, rest)) = command.split_once(char::is_whitespace) else {
        return false;
    };
    if program != "ssh" && !program.ends_with("/ssh") {
        return false;
    }
    let base = base_args(&Remote {
        name: String::new(),
        ssh: String::new(),
        browser_profile: String::new(),
        socks_port,
        herdr_socket: None,
    })
    .join(" ");
    let Some(tail) = rest
        .trim()
        .strip_prefix(&base)
        .and_then(|tail| tail.strip_prefix(' '))
    else {
        return false;
    };
    let is_host = |word: &str| !word.is_empty() && !word.contains(char::is_whitespace);
    if is_host(tail) {
        return true;
    }
    // The herdr forward's local path may contain spaces ("Application
    // Support"), so it is matched by shape, not split into words.
    let Some(forward) = tail.strip_prefix("-o StreamLocalBindUnlink=yes -L ") else {
        return false;
    };
    forward.rsplit_once(' ').is_some_and(|(spec, host)| {
        is_host(host) && spec.contains(&format!("/herdr-{socks_port}.sock:/"))
    })
}

/// Stops a tunnel of ours that a force-quit WhatsUp left listening on
/// `socks_port`, so the new one can take the port. Anything else holding the
/// port is left alone (ssh then reports the port as taken).
fn reclaim_port(socks_port: u16) {
    let Ok(listeners) = std::process::Command::new("lsof")
        .args(["-t", "-nP", &format!("-iTCP:{socks_port}"), "-sTCP:LISTEN"])
        .output()
    else {
        return;
    };
    for pid in String::from_utf8_lossy(&listeners.stdout).split_whitespace() {
        let Ok(command) = std::process::Command::new("ps")
            .args(["-o", "command=", "-p", pid])
            .output()
        else {
            continue;
        };
        if is_our_tunnel(String::from_utf8_lossy(&command.stdout).trim(), socks_port) {
            let _ = std::process::Command::new("kill").arg(pid).status();
        }
    }
}

pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub fn new() -> Self {
        Self { next: FIRST_DELAY }
    }

    /// How long to wait before restarting a tunnel that ran for `ran_for`.
    pub fn next_delay(&mut self, ran_for: Duration) -> Duration {
        if ran_for >= HEALTHY_RUN {
            self.next = FIRST_DELAY;
        }
        let delay = self.next;
        self.next = (self.next * 2).min(MAX_DELAY);
        delay
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    Starting,
    Running,
    Waiting { last_error: String },
}

struct Entry {
    remote: Remote,
    task: JoinHandle<()>,
    state: Arc<Mutex<TunnelState>>,
}

pub struct Tunnels {
    program: PathBuf,
    /// Where herdr socket forwards are bound (WhatsUp's runtime dir).
    socket_dir: PathBuf,
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl Tunnels {
    /// `program` is `ssh` in production.
    pub fn new(program: PathBuf, socket_dir: PathBuf) -> Self {
        Self {
            program,
            socket_dir,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Runs exactly one tunnel per remote in `remotes`: starts new ones,
    /// stops removed ones and restarts any whose settings changed. Must be
    /// called from within a tokio runtime.
    pub fn reconcile(&self, remotes: &[Remote]) {
        let mut entries = self.entries.lock().expect("tunnels lock");
        entries.retain(|_, entry| {
            let keep = remotes.contains(&entry.remote);
            if !keep {
                entry.task.abort();
                if let Some(path) = herdr_forward_path(&self.socket_dir, &entry.remote) {
                    let _ = std::fs::remove_file(path);
                }
            }
            keep
        });
        for remote in remotes {
            if entries.contains_key(&remote.name) {
                continue;
            }
            let forward = herdr_forward_path(&self.socket_dir, remote);
            if remote.herdr_socket.is_some() && forward.is_none() {
                eprintln!(
                    "{}: not forwarding herdr, the socket path in {} is too long",
                    remote.name,
                    self.socket_dir.display()
                );
            }
            let state = Arc::new(Mutex::new(TunnelState::Starting));
            let task = tokio::runtime::Handle::current().spawn(supervise(
                self.program.clone(),
                remote.clone(),
                forward,
                state.clone(),
            ));
            entries.insert(
                remote.name.clone(),
                Entry {
                    remote: remote.clone(),
                    task,
                    state,
                },
            );
        }
    }

    pub fn state(&self, name: &str) -> Option<TunnelState> {
        let entries = self.entries.lock().expect("tunnels lock");
        entries
            .get(name)
            .map(|entry| entry.state.lock().expect("state lock").clone())
    }

    /// The local end of `name`'s herdr socket forward, while its tunnel runs.
    pub fn herdr_socket(&self, name: &str) -> Option<PathBuf> {
        let entries = self.entries.lock().expect("tunnels lock");
        let entry = entries.get(name)?;
        if *entry.state.lock().expect("state lock") != TunnelState::Running {
            return None;
        }
        herdr_forward_path(&self.socket_dir, &entry.remote)
    }

    /// Stops every tunnel.
    pub fn shutdown(&self) {
        self.reconcile(&[]);
    }
}

impl Drop for Tunnels {
    fn drop(&mut self) {
        if let Ok(entries) = self.entries.get_mut() {
            for entry in entries.values() {
                entry.task.abort();
            }
        }
    }
}

async fn supervise(
    program: PathBuf,
    remote: Remote,
    forward: Option<PathBuf>,
    state: Arc<Mutex<TunnelState>>,
) {
    let set = |next: TunnelState| *state.lock().expect("state lock") = next;
    let mut backoff = Backoff::new();
    loop {
        set(TunnelState::Starting);
        let port = remote.socks_port;
        let _ = tokio::task::spawn_blocking(move || reclaim_port(port)).await;
        let started = Instant::now();
        let spawned = Command::new(&program)
            .args(ssh_args(&remote, forward.as_deref()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let last_error = match spawned {
            Err(err) => format!("could not run {}: {err}", program.display()),
            Ok(mut child) => {
                set(TunnelState::Running);
                // ssh may run for days, so keep only two lines: the first
                // meaningful one usually names the cause ("bind ...: Address
                // already in use"), the last one what ssh did about it.
                let stderr = child.stderr.take();
                let read = async {
                    let mut first: Option<String> = None;
                    let mut last: Option<String> = None;
                    if let Some(stderr) = stderr {
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let line = line.trim();
                            if line.is_empty() || is_probe_noise(line) {
                                continue;
                            }
                            if first.is_none() {
                                first = Some(line.to_string());
                            } else {
                                last = Some(line.to_string());
                            }
                        }
                    }
                    match (first, last) {
                        (Some(first), Some(last)) => Some(format!("{first}; {last}")),
                        (first, _) => first,
                    }
                };
                let (last_line, status) = tokio::join!(read, child.wait());
                match (last_line, status) {
                    (Some(line), _) => line,
                    (None, Ok(status)) => format!("ssh exited ({status})"),
                    (None, Err(err)) => format!("ssh failed: {err}"),
                }
            }
        };
        let delay = backoff.next_delay(started.elapsed());
        set(TunnelState::Waiting { last_error });
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    /// A stand-in for ssh: a shell script in a fresh temp dir.
    pub fn stub(name: &str, body: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("whatsup-stub-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("ssh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nfor a; do last=$a; done\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, script)
    }

    /// A stand-in ssh that just stays up, like a healthy tunnel.
    pub fn running_stub(name: &str) -> (PathBuf, PathBuf) {
        stub(name, "exec sleep 30")
    }
}

#[cfg(test)]
mod tests {
    use super::testing::stub;
    use super::*;
    use crate::config::Remote;
    use std::path::Path;
    use std::time::Duration;

    fn lucy() -> Remote {
        Remote {
            name: "lucy".into(),
            ssh: "lucy".into(),
            browser_profile: "megan:Lucy".into(),
            // Not the default port: tests must never touch a real tunnel.
            socks_port: 47695,
            herdr_socket: None,
        }
    }

    #[test]
    fn ssh_args_match_the_spec() {
        assert_eq!(
            ssh_args(
                &Remote {
                    socks_port: 47655,
                    ..lucy()
                },
                None
            ),
            [
                "-N",
                "-D",
                "127.0.0.1:47655",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                "lucy",
            ]
        );
    }

    #[test]
    fn backoff_grows_and_resets() {
        let mut backoff = Backoff::new();
        let short = Duration::from_millis(10);
        let delays: Vec<u64> = (0..7)
            .map(|_| backoff.next_delay(short).as_secs())
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(
            backoff.next_delay(Duration::from_secs(60)),
            Duration::from_secs(1)
        );
    }

    fn alive(pid: &str) -> bool {
        // A killed child can linger as a zombie until it is reaped; that counts
        // as gone.
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .map(|out| {
                let stat = String::from_utf8_lossy(&out.stdout);
                !stat.trim().is_empty() && !stat.trim().starts_with('Z')
            })
            .unwrap_or(false)
    }

    async fn wait_for(mut check: impl FnMut() -> bool) -> bool {
        for _ in 0..40 {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|text| text.trim().to_string())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_starts_and_stops_tunnels() {
        let (dir, script) = stub(
            "reconcile",
            "echo $$ > \"$(dirname \"$0\")/$last.pid\"\nexec sleep 30",
        );
        let tunnels = Tunnels::new(script, dir.clone());
        tunnels.reconcile(&[lucy()]);
        let pid_file = dir.join("lucy.pid");
        assert!(
            wait_for(|| read(&pid_file).is_some()).await,
            "stub never ran"
        );
        let pid = read(&pid_file).unwrap();
        assert!(alive(&pid));
        assert_eq!(tunnels.state("lucy"), Some(TunnelState::Running));

        tunnels.reconcile(&[]);
        assert!(wait_for(|| !alive(&pid)).await, "stub {pid} still running");
        assert_eq!(tunnels.state("lucy"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changed_remote_restarts_its_tunnel() {
        let (dir, script) = stub(
            "restart",
            "echo $$ > \"$(dirname \"$0\")/$last.pid\"\nexec sleep 30",
        );
        let tunnels = Tunnels::new(script, dir.clone());
        tunnels.reconcile(&[lucy()]);
        let pid_file = dir.join("lucy.pid");
        assert!(wait_for(|| read(&pid_file).is_some()).await);
        let first = read(&pid_file).unwrap();
        let moved = Remote {
            socks_port: 47696,
            ..lucy()
        };
        tunnels.reconcile(&[moved]);
        assert!(wait_for(|| read(&pid_file).is_some_and(|pid| pid != first)).await);
        assert!(wait_for(|| !alive(&first)).await);
        tunnels.shutdown();
        let second = read(&pid_file).unwrap();
        assert!(wait_for(|| !alive(&second)).await);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn lucy_with_herdr() -> Remote {
        Remote {
            herdr_socket: Some("/home/jds/.config/herdr/herdr.sock".into()),
            ..lucy()
        }
    }

    #[test]
    fn ssh_args_with_forward() {
        let local = Path::new("/Users/jds/Library/Application Support/WhatsUp/herdr-47695.sock");
        let with = ssh_args(&lucy_with_herdr(), Some(local));
        let mut want = ssh_args(&lucy(), None);
        let host = want.pop().unwrap();
        want.extend([
            "-o".to_string(),
            "StreamLocalBindUnlink=yes".to_string(),
            "-L".to_string(),
            format!("{}:/home/jds/.config/herdr/herdr.sock", local.display()),
            host,
        ]);
        assert_eq!(with, want);
        // A remote without a herdr socket gets no forward even if asked.
        assert_eq!(ssh_args(&lucy(), Some(local)), ssh_args(&lucy(), None));
    }

    #[test]
    fn forward_path_needs_socket_and_short_dir() {
        let dir = Path::new("/Users/jds/Library/Application Support/WhatsUp");
        assert_eq!(
            herdr_forward_path(dir, &lucy_with_herdr()),
            Some(dir.join("herdr-47695.sock"))
        );
        assert_eq!(herdr_forward_path(dir, &lucy()), None);
        let long = PathBuf::from(format!("/{}", "d".repeat(119)));
        assert_eq!(herdr_forward_path(&long, &lucy_with_herdr()), None);
    }

    #[test]
    fn recognizes_our_leftover_tunnel_with_forward() {
        let ours = "ssh -N -D 127.0.0.1:47655 -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=5 -o ServerAliveCountMax=2 -o StreamLocalBindUnlink=yes -L /Users/jds/Library/Application Support/WhatsUp/herdr-47655.sock:/home/jds/.config/herdr/herdr.sock lucy";
        assert!(is_our_tunnel(ours, 47655));
        assert!(!is_our_tunnel(ours, 47656));
        let plain = "ssh -N -D 127.0.0.1:47655 -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=5 -o ServerAliveCountMax=2 lucy";
        assert!(is_our_tunnel(plain, 47655));
        assert!(!is_our_tunnel(&plain.replace(" lucy", " -R x lucy"), 47655));
        assert!(!is_our_tunnel(
            &ours.replace("herdr-47655.sock", "other.sock"),
            47655
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_a_tunnel_removes_its_socket_file() {
        let (dir, script) = super::testing::running_stub("unlink");
        let tunnels = Tunnels::new(script, dir.clone());
        tunnels.reconcile(&[lucy_with_herdr()]);
        assert!(wait_for(|| tunnels.state("lucy") == Some(TunnelState::Running)).await);
        let socket = dir.join("herdr-47695.sock");
        std::fs::write(&socket, "").unwrap();
        tunnels.reconcile(&[]);
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn herdr_socket_only_while_running() {
        let (dir, script) = stub("herdr-exit", "exit 1");
        let tunnels = Tunnels::new(script, dir.clone());
        tunnels.reconcile(&[lucy_with_herdr()]);
        assert!(
            wait_for(|| matches!(tunnels.state("lucy"), Some(TunnelState::Waiting { .. }))).await
        );
        assert_eq!(tunnels.herdr_socket("lucy"), None);
        tunnels.shutdown();
        let _ = std::fs::remove_dir_all(dir);

        let (dir, script) = super::testing::running_stub("herdr-up");
        let tunnels = Tunnels::new(script, dir.clone());
        tunnels.reconcile(&[lucy_with_herdr()]);
        assert!(wait_for(|| tunnels.state("lucy") == Some(TunnelState::Running)).await);
        assert_eq!(
            tunnels.herdr_socket("lucy"),
            Some(dir.join("herdr-47695.sock"))
        );
        tunnels.reconcile(&[lucy()]);
        assert!(wait_for(|| tunnels.state("lucy") == Some(TunnelState::Running)).await);
        assert_eq!(tunnels.herdr_socket("lucy"), None);
        tunnels.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recognizes_our_own_leftover_tunnel() {
        let ours = "ssh -N -D 127.0.0.1:47655 -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=5 -o ServerAliveCountMax=2 lucy";
        assert!(is_our_tunnel(ours, 47655));
        assert!(is_our_tunnel(&format!("/usr/bin/{ours}"), 47655));
        assert!(is_our_tunnel(&ours.replace(" lucy", " oldhost"), 47655));
        assert!(!is_our_tunnel(ours, 47656));
        assert!(!is_our_tunnel("ssh -N -D 127.0.0.1:47655 lucy", 47655));
        assert!(!is_our_tunnel(
            &ours.replacen("ssh", "/bin/sh /tmp/ssh", 1),
            47655
        ));
        assert!(!is_our_tunnel("", 47655));
    }

    #[test]
    fn probe_noise_is_not_an_error() {
        assert!(is_probe_noise(
            "channel 22: open failed: connect failed: Connection refused"
        ));
        assert!(!is_probe_noise(
            "bind [127.0.0.1]:47655: Address already in use"
        ));
        assert!(!is_probe_noise("Permission denied (publickey)."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn last_error_skips_channel_noise() {
        let (dir, script) = stub(
            "noise",
            "echo 'channel 2: open failed: connect failed: Connection refused' >&2\necho 'Connection closed by remote host' >&2\necho 'channel 3: open failed: connect failed: Connection refused' >&2\nexit 255",
        );
        let tunnels = Tunnels::new(script, dir.clone());
        tunnels.reconcile(&[lucy()]);
        let reported = wait_for(|| {
            tunnels.state("lucy")
                == Some(TunnelState::Waiting {
                    last_error: "Connection closed by remote host".into(),
                })
        })
        .await;
        assert!(reported, "state was {:?}", tunnels.state("lucy"));
        tunnels.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failing_tunnel_reports_its_error() {
        let (dir, script) = stub(
            "failing",
            // What OpenSSH really prints when the forward port is taken.
            "echo 'bind [127.0.0.1]:47695: Address already in use' >&2\necho 'channel_setup_fwd_listener_tcpip: cannot listen to port: 47695' >&2\necho 'Could not request local forwarding.' >&2\nexit 255",
        );
        let tunnels = Tunnels::new(script, dir.clone());
        tunnels.reconcile(&[lucy()]);
        let reported = wait_for(|| {
            matches!(
                tunnels.state("lucy"),
                Some(TunnelState::Waiting { ref last_error }) if last_error.contains("Address already in use")
            )
        })
        .await;
        assert!(reported, "state was {:?}", tunnels.state("lucy"));
        tunnels.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
