//! Stopping whatever serves a port on this machine.
//!
//! A port published by docker is stopped through docker (its whole compose
//! project when it has one). Any other listener is stopped by signalling its
//! process group, which holds the server and the `npm` / `just` / `pnpm`
//! that started it, but not the shell or agent that launched those.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::probe::port_is_open;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Process,
    Compose,
    Container,
    None,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Process => "process",
            Action::Compose => "compose",
            Action::Container => "container",
            Action::None => "none",
        }
    }
}

/// What happened to one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortResult {
    pub port: u16,
    pub action: Action,
    pub detail: String,
    pub error: Option<String>,
}

pub trait Stopper: Send + Sync {
    /// Stops what serves each port; one result per port, in the same order.
    fn stop(&self, ports: &[u16]) -> Vec<PortResult>;
}

pub struct SystemStopper {
    pub docker: Option<PathBuf>,
    /// How long a process group gets to close its port after SIGTERM.
    pub grace: Duration,
    /// How long to wait for the port after SIGKILL.
    pub kill_wait: Duration,
    /// How long one docker command may run.
    pub command_timeout: Duration,
}

impl Default for SystemStopper {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemStopper {
    pub fn new() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self {
            docker: find_docker(std::env::var_os("PATH").as_deref(), home.as_deref()),
            grace: Duration::from_secs(5),
            kill_wait: Duration::from_secs(2),
            command_timeout: Duration::from_secs(60),
        }
    }
}

/// `docker` on `path`, else in the places it is installed to. An app started
/// by Finder has a `PATH` without `/usr/local/bin`.
pub fn find_docker(path: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    let on_path = path.map(std::env::split_paths).into_iter().flatten();
    let fixed = [
        Some(PathBuf::from("/usr/local/bin")),
        Some(PathBuf::from("/opt/homebrew/bin")),
        home.map(|home| home.join(".docker").join("bin")),
        Some(PathBuf::from(
            "/Applications/Docker.app/Contents/Resources/bin",
        )),
    ];
    on_path
        .chain(fixed.into_iter().flatten())
        .map(|dir| dir.join("docker"))
        .find(|candidate| candidate.is_file())
}

/// The pids in `lsof -Fp` output (`p123` lines) or `ss -p` output
/// (`pid=123`), in order, without repeats.
fn parse_listener_pids(output: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut add = |text: &str| {
        let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(pid) = digits.parse::<u32>() {
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    };
    for line in output.lines() {
        match line.strip_prefix('p') {
            Some(rest) if !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()) => {
                add(rest)
            }
            _ => line.split("pid=").skip(1).for_each(&mut add),
        }
    }
    pids
}

/// Whether a `docker ps` Ports column maps host `port` to a container.
fn publishes(ports_column: &str, port: u16) -> bool {
    ports_column.split(',').any(|entry| {
        let Some((host_side, _)) = entry.trim().split_once("->") else {
            return false;
        };
        let Some((_, host_ports)) = host_side.rsplit_once(':') else {
            return false;
        };
        let (first, last) = host_ports
            .split_once('-')
            .unwrap_or((host_ports, host_ports));
        match (first.parse::<u16>(), last.parse::<u16>()) {
            (Ok(first), Ok(last)) => (first..=last).contains(&port),
            _ => false,
        }
    })
}

/// The container that publishes host `port`, from the output of
/// [`DOCKER_PS_FORMAT`]: its id and its compose project ("" when it has none).
fn docker_owner(ps_output: &str, port: u16) -> Option<(String, String)> {
    ps_output.lines().find_map(|line| {
        let mut fields = line.splitn(3, '\t');
        let (id, project, ports) = (fields.next()?, fields.next()?, fields.next()?);
        (!id.is_empty() && publishes(ports, port)).then(|| (id.to_string(), project.to_string()))
    })
}

const DOCKER_PS_FORMAT: &str = "{{.ID}}\t{{.Label \"com.docker.compose.project\"}}\t{{.Ports}}";

/// Why `pid` (named `name`) must not be signalled, if so.
fn refusal(pid: u32, name: &str, own_pid: u32) -> Option<String> {
    if pid == own_pid {
        return Some("that is WhatsUp itself".into());
    }
    if name.to_ascii_lowercase().contains("docker") {
        return Some(format!(
            "docker holds it ({}), but docker could not say which container",
            name.rsplit('/').next().unwrap_or(name)
        ));
    }
    None
}

/// One thing to signal: a whole process group, or a single process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Signal {
    Group(i32),
    Pid(i32),
}

impl Signal {
    fn send(self, signal: libc::c_int) -> Result<(), String> {
        let target = match self {
            Signal::Group(pgid) => -pgid,
            Signal::Pid(pid) => pid,
        };
        // SAFETY: kill has no memory-safety preconditions.
        if unsafe { libc::kill(target, signal) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            // Already gone.
            Some(libc::ESRCH) => Ok(()),
            _ => Err(format!("could not signal {self}: {error}")),
        }
    }
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Signal::Group(pgid) => write!(f, "pgid {pgid}"),
            Signal::Pid(pid) => write!(f, "pid {pid}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Compose(String),
    Container(String),
    Process(BTreeSet<Signal>),
    Nothing,
    Refused(String),
}

fn listener_pids(port: u16) -> Vec<u32> {
    #[cfg(target_os = "macos")]
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .stderr(Stdio::null())
        .output();
    #[cfg(not(target_os = "macos"))]
    let output = Command::new("ss")
        .args(["-H", "-ltnp", &format!("sport = :{port}")])
        .stderr(Stdio::null())
        .output();
    output
        .map(|output| parse_listener_pids(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

fn process_name(pid: u32) -> String {
    Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn signal_for(pid: u32) -> Option<Signal> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: getpgid and getpgrp have no memory-safety preconditions.
    let (group, own_group) = unsafe { (libc::getpgid(pid), libc::getpgrp()) };
    if group < 0 {
        // The process is already gone.
        return None;
    }
    Some(if group <= 1 || group == own_group {
        Signal::Pid(pid)
    } else {
        Signal::Group(group)
    })
}

fn wait_closed(ports: &[u16], limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if !ports.iter().any(|port| port_is_open(*port)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn describe(signals: &BTreeSet<Signal>) -> String {
    signals
        .iter()
        .map(Signal::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl SystemStopper {
    /// Runs docker with `args`; the error is its last line of stderr.
    fn docker(&self, docker: &Path, args: &[&str]) -> Result<String, String> {
        let shown = format!("docker {}", args.join(" "));
        let mut command = Command::new(docker);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // docker runs helpers (compose, credential stores) from its own
        // directory, which a Finder-started app does not have on PATH.
        if let Some(dir) = docker.parent() {
            let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default();
            dirs.push(dir.to_path_buf());
            if let Ok(path) = std::env::join_paths(dirs) {
                command.env("PATH", path);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("{shown}: could not run: {err}"))?;
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut text = String::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_string(&mut text);
                }
                text
            })
        };
        let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
        let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
        let deadline = Instant::now() + self.command_timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{shown}: gave up after {} s",
                        self.command_timeout.as_secs_f32()
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(err) => return Err(format!("{shown}: {err}")),
            }
        };
        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        if status.success() {
            return Ok(stdout);
        }
        let reason = stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| status.to_string());
        Err(format!("{shown}: {reason}"))
    }

    fn target_of(&self, port: u16, containers: Option<&str>) -> Target {
        if let Some((id, project)) = containers.and_then(|ps| docker_owner(ps, port)) {
            return if project.is_empty() {
                Target::Container(id)
            } else {
                Target::Compose(project)
            };
        }
        let pids = listener_pids(port);
        let mut signals = BTreeSet::new();
        for pid in &pids {
            if let Some(reason) = refusal(*pid, &process_name(*pid), std::process::id()) {
                return Target::Refused(reason);
            }
            signals.extend(signal_for(*pid));
        }
        if !signals.is_empty() {
            Target::Process(signals)
        } else if port_is_open(port) {
            Target::Refused("cannot see what is listening on it".into())
        } else {
            Target::Nothing
        }
    }

    /// Stops `target`, which serves `ports`: the action, a detail for the
    /// reply, and the failure if there was one.
    fn stop_target(&self, target: &Target, ports: &[u16]) -> (Action, String, Option<String>) {
        match target {
            Target::Nothing => (Action::None, "not running".into(), None),
            Target::Refused(reason) => (Action::None, String::new(), Some(reason.clone())),
            Target::Compose(project) => {
                let result = self.docker_stop(&["compose", "-p", project, "stop"]);
                (Action::Compose, project.clone(), result.err())
            }
            Target::Container(id) => {
                let result = self.docker_stop(&["stop", id]);
                (Action::Container, id.clone(), result.err())
            }
            Target::Process(signals) => {
                let detail = describe(signals);
                let send = |signal: libc::c_int| -> Result<(), String> {
                    signals.iter().try_for_each(|target| target.send(signal))
                };
                let result = send(libc::SIGTERM).and_then(|()| {
                    if wait_closed(ports, self.grace) {
                        return Ok(());
                    }
                    send(libc::SIGKILL)?;
                    if wait_closed(ports, self.kill_wait) {
                        Ok(())
                    } else {
                        Err(format!("still listening after SIGKILL of {detail}"))
                    }
                });
                (Action::Process, detail, result.err())
            }
        }
    }

    fn docker_stop(&self, args: &[&str]) -> Result<(), String> {
        let docker = self.docker.as_deref().ok_or("docker was not found")?;
        self.docker(docker, args).map(|_| ())
    }
}

impl Stopper for SystemStopper {
    fn stop(&self, ports: &[u16]) -> Vec<PortResult> {
        // A docker that is missing or not answering publishes nothing; a
        // port it does hold is then refused by name instead of signalled.
        let containers = self.docker.as_deref().and_then(|docker| {
            self.docker(docker, &["ps", "--no-trunc", "--format", DOCKER_PS_FORMAT])
                .ok()
        });
        let mut targets: BTreeMap<Target, Vec<u16>> = BTreeMap::new();
        for port in ports {
            let served = targets
                .entry(self.target_of(*port, containers.as_deref()))
                .or_default();
            if !served.contains(port) {
                served.push(*port);
            }
        }
        let outcomes: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = targets
                .iter()
                .map(|(target, served)| {
                    scope.spawn(move || (served, self.stop_target(target, served)))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("stop thread"))
                .collect()
        });
        ports
            .iter()
            .map(|port| {
                let (_, (action, detail, error)) = outcomes
                    .iter()
                    .find(|(served, _)| served.contains(port))
                    .expect("every port has a target");
                PortResult {
                    port: *port,
                    action: *action,
                    detail: detail.clone(),
                    error: error.clone(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    #[test]
    fn listener_pids_from_lsof_and_ss() {
        assert_eq!(parse_listener_pids("p25348\nf25\n"), [25348]);
        assert_eq!(
            parse_listener_pids("p25348\nf25\np25348\nf26\np7\n"),
            [25348, 7]
        );
        assert_eq!(
            parse_listener_pids(
                "LISTEN 0      511    [::1]:3008 [::]:* users:((\"node\",pid=1175308,fd=19))\n"
            ),
            [1175308]
        );
        assert_eq!(
            parse_listener_pids(
                "LISTEN 0 511 *:80 *:* users:((\"nginx\",pid=12,fd=6),(\"nginx\",pid=13,fd=6))\n"
            ),
            [12, 13]
        );
        // docker-proxy runs as root: ss shows the socket without its owner.
        assert_eq!(
            parse_listener_pids(
                "LISTEN 0      4096   0.0.0.0:8004 0.0.0.0:*\nLISTEN 0      4096      [::]:8004    [::]:*\n"
            ),
            Vec::<u32>::new()
        );
        assert_eq!(parse_listener_pids("python3\n"), Vec::<u32>::new());
    }

    #[test]
    fn docker_owner_reads_host_ports_only() {
        let ps = "aaa\thuvr_jds4\t0.0.0.0:8004->8000/tcp, [::]:8004->8000/tcp\n\
                  bbb\thuvr_jds4\t5432/tcp\n\
                  ccc\t\t0.0.0.0:9000-9002->9000-9002/tcp\n\
                  ddd\tother\t\n";
        assert_eq!(
            docker_owner(ps, 8004),
            Some(("aaa".into(), "huvr_jds4".into()))
        );
        assert_eq!(docker_owner(ps, 8000), None, "a container-side port");
        assert_eq!(docker_owner(ps, 5432), None, "exposed, not published");
        assert_eq!(docker_owner(ps, 9001), Some(("ccc".into(), String::new())));
        assert_eq!(docker_owner(ps, 9003), None);
        assert_eq!(docker_owner("", 8004), None);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("whatsup-stop-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn find_docker_prefers_path_then_known_places() {
        let dir = temp_dir("find");
        let (first, second, home) = (dir.join("a"), dir.join("b"), dir.join("home"));
        script(&second.join("docker"), "");
        script(&home.join(".docker/bin/docker"), "");
        std::fs::create_dir_all(&first).unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            find_docker(Some(&path), Some(&home)),
            Some(second.join("docker"))
        );
        // Whatever this machine has installed comes before ~/.docker/bin.
        let fallback = find_docker(Some(first.as_os_str()), Some(&home)).unwrap();
        assert!(fallback.is_absolute() && fallback.ends_with("docker"));
        if !Path::new("/usr/local/bin/docker").exists()
            && !Path::new("/opt/homebrew/bin/docker").exists()
        {
            assert_eq!(fallback, home.join(".docker/bin/docker"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn docker_named_listener_is_refused() {
        let reason = refusal(
            20728,
            "/Applications/Docker.app/Contents/MacOS/com.docker.backend",
            1,
        )
        .unwrap();
        assert!(reason.contains("com.docker.backend"), "{reason}");
        assert!(refusal(20728, "docker-proxy", 1).is_some());
        assert!(refusal(42, "whatsup", 42).unwrap().contains("WhatsUp"));
        assert_eq!(refusal(20728, "node", 1), None);
    }

    fn stopper(docker: Option<PathBuf>) -> SystemStopper {
        SystemStopper {
            docker,
            grace: Duration::from_secs(5),
            kill_wait: Duration::from_secs(2),
            command_timeout: Duration::from_secs(10),
        }
    }

    /// Not a test of its own: re-run as a child process by the tests below,
    /// it listens on a port until it is killed.
    #[test]
    fn helper_listener() {
        let Some(report) = std::env::var_os("WHATSUP_TEST_LISTEN") else {
            return;
        };
        if std::env::var_os("WHATSUP_TEST_IGNORE_TERM").is_some() {
            // SAFETY: ignoring a signal has no preconditions.
            unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let report = PathBuf::from(report);
        let partial = report.with_extension("partial");
        std::fs::write(&partial, format!("{port} {}", std::process::id())).unwrap();
        std::fs::rename(partial, report).unwrap();
        std::thread::sleep(Duration::from_secs(120));
    }

    /// Starts `sh` in its own process group running the helper listener
    /// beside it, like `npm start` runs vite. Returns sh, the listener's port
    /// and the listener's pid.
    fn spawn_listener(name: &str, ignore_term: bool) -> (std::process::Child, u16, i32) {
        let dir = temp_dir(name);
        let report = dir.join("port");
        let mut command = Command::new("sh");
        command
            .args(["-c", "\"$0\" \"$@\" & wait"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "stop::tests::helper_listener", "--nocapture"])
            .env("WHATSUP_TEST_LISTEN", &report)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if ignore_term {
            command.env("WHATSUP_TEST_IGNORE_TERM", "1");
        }
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = std::fs::read_to_string(&report) {
                let mut words = text.split_whitespace();
                let port = words.next().unwrap().parse().unwrap();
                let pid = words.next().unwrap().parse().unwrap();
                let _ = std::fs::remove_dir_all(dir);
                return (child, port, pid);
            }
            assert!(Instant::now() < deadline, "the helper never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            // SAFETY: signal 0 only checks that the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn stops_the_listeners_process_group() {
        let (mut shell, port, listener) = spawn_listener("group", false);
        assert!(port_is_open(port));
        let results = stopper(None).stop(&[port]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].port, port);
        assert_eq!(results[0].action, Action::Process);
        assert_eq!(results[0].error, None);
        assert_eq!(results[0].detail, format!("pgid {}", shell.id()));
        assert!(!port_is_open(port));
        // The launcher in the same group went with it.
        let status = shell.wait().unwrap();
        assert!(!status.success());
        assert!(gone(listener));
    }

    #[test]
    fn kills_a_listener_that_ignores_term() {
        let (mut shell, port, listener) = spawn_listener("stubborn", true);
        let mut stopper = stopper(None);
        stopper.grace = Duration::from_millis(300);
        let started = Instant::now();
        let results = stopper.stop(&[port]);
        assert_eq!(results[0].action, Action::Process);
        assert_eq!(results[0].error, None);
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(!port_is_open(port));
        let _ = shell.wait();
        assert!(gone(listener));
    }

    #[test]
    fn closed_port_is_not_running() {
        // Port 1 needs root to bind, so nothing listens there.
        let results = stopper(None).stop(&[1]);
        assert_eq!(
            results,
            [PortResult {
                port: 1,
                action: Action::None,
                detail: "not running".into(),
                error: None,
            }]
        );
    }

    /// A stand-in docker: `ps` prints `table`, anything else is appended to
    /// the log and then runs `otherwise`.
    fn fake_docker(name: &str, table: &str, otherwise: &str) -> (PathBuf, PathBuf) {
        let dir = temp_dir(name);
        let (docker, log) = (dir.join("docker"), dir.join("log"));
        script(
            &docker,
            &format!(
                "if [ \"$1\" = ps ]; then printf '{table}'; exit 0; fi\necho \"$@\" >> '{}'\n{otherwise}",
                log.display()
            ),
        );
        (docker, log)
    }

    #[test]
    fn compose_project_is_stopped_once() {
        let (docker, log) = fake_docker(
            "compose",
            r"aaa\tp1\t0.0.0.0:8004->8000/tcp\nbbb\tp1\t0.0.0.0:8005->8001/tcp\n",
            "",
        );
        let results = stopper(Some(docker)).stop(&[8004, 8005, 8004]);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "compose -p p1 stop\n"
        );
        assert_eq!(results.len(), 3);
        for (result, port) in results.iter().zip([8004, 8005, 8004]) {
            assert_eq!(result.port, port);
            assert_eq!(result.action, Action::Compose);
            assert_eq!(result.detail, "p1");
            assert_eq!(result.error, None);
        }
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn container_without_project_is_stopped() {
        let (docker, log) = fake_docker("container", r"abc123\t\t0.0.0.0:8004->8000/tcp\n", "");
        let results = stopper(Some(docker)).stop(&[8004]);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "stop abc123\n");
        assert_eq!(results[0].action, Action::Container);
        assert_eq!(results[0].detail, "abc123");
        assert_eq!(results[0].error, None);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn failing_docker_command_is_an_error() {
        let (docker, log) = fake_docker(
            "failing",
            r"aaa\tp1\t0.0.0.0:8004->8000/tcp\n",
            "echo progress >&2\necho boom >&2\nexit 1",
        );
        let results = stopper(Some(docker)).stop(&[8004]);
        assert_eq!(results[0].action, Action::Compose);
        assert_eq!(
            results[0].error.as_deref(),
            Some("docker compose -p p1 stop: boom")
        );
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn slow_command_is_killed() {
        let (docker, log) =
            fake_docker("slow", r"aaa\tp1\t0.0.0.0:8004->8000/tcp\n", "exec sleep 5");
        let mut stopper = stopper(Some(docker));
        stopper.command_timeout = Duration::from_millis(300);
        let started = Instant::now();
        let results = stopper.stop(&[8004]);
        assert!(started.elapsed() < Duration::from_secs(2));
        let error = results[0].error.as_deref().unwrap();
        assert!(error.contains("gave up after"), "{error}");
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }
}
