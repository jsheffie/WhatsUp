//! Starting what serves a row's ports on this machine.
//!
//! A port with no start command is started through docker: the container
//! that publishes it already exists, stopped, so its whole compose project
//! (or the single container) is started again. A port with a start command
//! is typed into a tab of the row's herdr workspace, then watched for a
//! few seconds in case it exits at once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::expand_dir;
use crate::herdr::{label_matches, CallError, HerdrClient};
use crate::stop::{find_docker, run_docker, run_docker_as, Action, PortResult};

/// The line typed into a tab: `run` in `dir`, then a marker with its exit
/// code that only this start knows to look for.
pub fn typed_line(dir: &Path, run: &str, nonce: &str) -> String {
    let quoted = dir.to_string_lossy().replace('\'', r"'\''");
    format!("cd -- '{quoted}' && {{ {run}; }}; echo \"[whatsup {nonce}] exited with $?\"")
}

/// How many recent lines of a pane are read while watching it.
const READ_LINES: u32 = 60;
/// How many printed lines an early exit reports.
const SHOWN_LINES: usize = 5;

/// The exit code in `text`'s marker for `nonce` (a line containing
/// `[whatsup <nonce>] exited with <digits>`), and the last non-blank lines
/// the command printed between its echoed line and the marker.
pub fn early_exit(text: &str, nonce: &str) -> Option<(i32, Vec<String>)> {
    let tag = format!("[whatsup {nonce}] exited with ");
    let lines: Vec<&str> = text.lines().collect();
    let (at, head, code) = lines.iter().enumerate().rev().find_map(|(index, line)| {
        let found = line.rfind(&tag)?;
        let digits = line[found + tag.len()..].trim_end();
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some((index, &line[..found], digits.parse::<i32>().ok()?))
    })?;
    // The shell echoes the typed line; what follows it is the command's.
    // When it has scrolled away, everything above the marker counts.
    let echo = format!("{tag}$?");
    let first = lines[..at]
        .iter()
        .rposition(|line| line.contains(&echo))
        .map_or(0, |echoed| echoed + 1);
    let mut printed: Vec<String> = lines[first..at]
        .iter()
        .chain([&head])
        .map(|line| line.trim_end())
        .filter(|line| !line.trim().is_empty())
        .map(String::from)
        .collect();
    let shown = printed.split_off(printed.len().saturating_sub(SHOWN_LINES));
    Some((code, shown))
}

/// A marker tag no earlier start used: time, pid and a counter, in hex.
fn nonce() -> String {
    static STARTS: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!(
        "{nanos:x}{:x}{:x}",
        std::process::id(),
        STARTS.fetch_add(1, Ordering::Relaxed)
    )
}

fn not_running(machine: &str) -> String {
    format!("herdr is not running on {machine}")
}

/// Types `run` into the herdr tab `label` of the first workspace matching
/// `prefix`, creating the workspace or tab when missing, and watches it for
/// an early exit. The detail on success names the tab and workspace. All of
/// it fits in `timings.link_budget`.
pub fn launch(
    client: &HerdrClient,
    prefix: &str,
    label: &str,
    dir: &Path,
    run: &str,
    timings: &HerdrTimings,
    machine: &str,
) -> Result<String, String> {
    let deadline = Instant::now() + timings.link_budget;
    let left = || deadline.saturating_duration_since(Instant::now());
    // A client for one call, bounded by what is left of the budget.
    let herdr = || match left() {
        left if left.is_zero() => Err(not_running(machine)),
        left => Ok(HerdrClient {
            socket: client.socket.clone(),
            timeout: client.timeout.min(left),
        }),
    };
    let failed = |error: CallError| match error {
        CallError::NoAnswer => not_running(machine),
        CallError::Failed(message) => format!("herdr: {message}"),
    };
    let pause = |wait: Duration| std::thread::sleep(wait.min(left()));

    let matched = herdr()?
        .workspaces()
        .map_err(failed)?
        .into_iter()
        .filter(|(_, _, workspace)| label_matches(workspace, prefix))
        .min_by_key(|(_, number, _)| *number);
    let (workspace, pane, fresh) = match matched {
        None => {
            let (_, tab, pane) = herdr()?.create_workspace(prefix, dir).map_err(failed)?;
            herdr()?.rename_tab(&tab, label).map_err(failed)?;
            (prefix.to_string(), pane, true)
        }
        Some((id, _, workspace)) => {
            let tabs = herdr()?.tabs(&id).map_err(failed)?;
            match tabs
                .into_iter()
                .find(|(_, seen)| seen.eq_ignore_ascii_case(label))
            {
                Some((tab, _)) => {
                    let busy = || {
                        format!(
                            "herdr tab \"{label}\" in {workspace} is busy; it may still be starting"
                        )
                    };
                    let pane = herdr()?
                        .panes(&id)
                        .map_err(failed)?
                        .into_iter()
                        .find(|(_, of)| *of == tab)
                        .map(|(pane, _)| pane)
                        .ok_or_else(busy)?;
                    if !herdr()?.idle(&pane).map_err(failed)? {
                        return Err(busy());
                    }
                    (workspace, pane, false)
                }
                None => {
                    let (_, pane) = herdr()?.create_tab(&id, dir, label).map_err(failed)?;
                    (workspace, pane, true)
                }
            }
        }
    };

    // A new pane's shell may still be starting. If it never shows a
    // prompt, the line is typed anyway: the terminal holds it for the shell.
    if fresh {
        let until = Instant::now() + timings.prompt_wait;
        while herdr()?
            .read(&pane, READ_LINES)
            .map_err(failed)?
            .trim()
            .is_empty()
            && Instant::now() < until
        {
            pause(timings.prompt_poll);
        }
    }

    let nonce = nonce();
    herdr()?
        .type_line(&pane, &typed_line(dir, run, &nonce))
        .map_err(failed)?;

    let until = Instant::now() + timings.watch;
    loop {
        pause(
            timings
                .watch_poll
                .min(until.saturating_duration_since(Instant::now())),
        );
        let text = herdr()?.read(&pane, READ_LINES).map_err(failed)?;
        if let Some((code, printed)) = early_exit(&text, &nonce) {
            let mut error = format!("{run} exited with {code}");
            if !printed.is_empty() {
                error = format!("{error}: {}", printed.join(" / "));
            }
            return Err(error);
        }
        if Instant::now() >= until {
            return Ok(format!("herdr tab {label} in {workspace}"));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartKind {
    /// No start command: start the container that publishes the port.
    Docker,
    /// Type `run` in `dir` into the herdr tab `label` of workspace `prefix`.
    Herdr {
        prefix: String,
        label: String,
        dir: PathBuf,
        run: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortStart {
    pub port: u16,
    pub kind: StartKind,
}

pub trait Starter: Send + Sync {
    /// Starts what serves each port; one result per entry, in the same order.
    fn start(&self, plan: &[PortStart]) -> Vec<PortResult>;
}

/// How long the herdr half of a start waits for each step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HerdrTimings {
    /// One herdr socket call.
    pub call: Duration,
    /// How long a tab's shell gets to show a prompt.
    pub prompt_wait: Duration,
    pub prompt_poll: Duration,
    /// How long to watch for the typed command exiting early.
    pub watch: Duration,
    pub watch_poll: Duration,
    /// Everything herdr may take for one link.
    pub link_budget: Duration,
}

impl Default for HerdrTimings {
    fn default() -> Self {
        Self {
            call: Duration::from_secs(1),
            prompt_wait: Duration::from_secs(10),
            prompt_poll: Duration::from_millis(200),
            watch: Duration::from_secs(5),
            watch_poll: Duration::from_millis(500),
            link_budget: Duration::from_secs(30),
        }
    }
}

pub struct SystemStarter {
    pub docker: Option<PathBuf>,
    /// How long one docker listing may take.
    pub list_timeout: Duration,
    /// How long one docker start command may run.
    pub command_timeout: Duration,
    pub herdr: Option<PathBuf>,
    pub herdr_timings: HerdrTimings,
    pub machine: String,
}

impl SystemStarter {
    pub fn new(herdr_socket: Option<PathBuf>, machine: String) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self {
            docker: find_docker(std::env::var_os("PATH").as_deref(), home.as_deref()),
            list_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(120),
            herdr: herdr_socket,
            herdr_timings: HerdrTimings::default(),
            machine,
        }
    }
}

/// What starts the container that publishes a port.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DockerTarget {
    Compose(String),
    Container { id: String, name: String },
}

/// The host-port ranges (inclusive) in a container's `PortBindings` JSON.
fn parse_bindings(json: &str) -> Vec<(u16, u16)> {
    let Ok(serde_json::Value::Object(bindings)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    bindings
        .values()
        .filter_map(|entries| entries.as_array())
        .flatten()
        .filter_map(|entry| {
            let text = entry.get("HostPort")?.as_str()?;
            let (first, last) = text.split_once('-').unwrap_or((text, text));
            Some((first.parse().ok()?, last.parse().ok()?))
        })
        .collect()
}

/// The container that publishes host `port`, from `docker ps -a` output
/// (`id`, compose project) and `docker inspect` output (`id`, name, port
/// bindings).
fn docker_owners(ps_all: &str, inspect: &str, port: u16) -> Result<DockerTarget, String> {
    let mut owners: Vec<(&str, &str)> = Vec::new();
    for line in inspect.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(id), Some(name), Some(bindings)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if parse_bindings(bindings)
            .iter()
            .any(|(first, last)| (*first..=*last).contains(&port))
        {
            owners.push((id, name.trim_start_matches('/')));
        }
    }
    match owners.as_slice() {
        [] => Err("no start command, and no container publishes it".into()),
        [(id, name)] => {
            let project = ps_all.lines().find_map(|line| {
                let (listed, project) = line.split_once('\t')?;
                (listed == *id).then_some(project)
            });
            Ok(match project {
                Some(project) if !project.is_empty() => DockerTarget::Compose(project.into()),
                _ => DockerTarget::Container {
                    id: (*id).into(),
                    name: (*name).into(),
                },
            })
        }
        several => Err(format!(
            "several containers publish it: {}",
            several
                .iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

const DOCKER_PS_ALL_FORMAT: &str = "{{.ID}}\t{{.Label \"com.docker.compose.project\"}}";
const DOCKER_INSPECT_FORMAT: &str = "{{.Id}}\t{{.Name}}\t{{json .HostConfig.PortBindings}}";

/// What happened to one port: the action, a detail for the reply, and the
/// failure if there was one.
type Outcome = (Action, String, Option<String>);

impl SystemStarter {
    /// Every container's id and compose project (`docker ps -a`), and its
    /// name and published ports (`docker inspect`). A failure to list them is
    /// docker's own error, after the short command.
    fn list_containers(&self) -> Result<(String, String), String> {
        let docker = self.docker.as_deref().ok_or(DOCKER_MISSING)?;
        let ps_all = run_docker_as(
            docker,
            &["ps", "-a", "--no-trunc", "--format", DOCKER_PS_ALL_FORMAT],
            self.list_timeout,
            "docker ps -a",
        )?;
        let ids: Vec<&str> = ps_all
            .lines()
            .filter_map(|line| line.split('\t').next())
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return Ok((ps_all, String::new()));
        }
        let mut args = vec!["inspect", "--format", DOCKER_INSPECT_FORMAT];
        args.extend(ids);
        let inspect = run_docker_as(docker, &args, self.list_timeout, "docker inspect")?;
        Ok((ps_all, inspect))
    }

    fn start_target(&self, target: &DockerTarget) -> Outcome {
        let (action, args, shown): (_, Vec<&str>, _) = match target {
            DockerTarget::Compose(project) => (
                Action::Compose,
                vec!["compose", "-p", project, "start"],
                format!("docker compose -p {project} start"),
            ),
            DockerTarget::Container { id, .. } => (
                Action::Container,
                vec!["start", id],
                format!("docker start {id}"),
            ),
        };
        let result = match self.docker.as_deref() {
            Some(docker) => run_docker(docker, &args, self.command_timeout).map(|_| ()),
            None => Err(DOCKER_MISSING.into()),
        };
        (action, shown, result.err())
    }

    /// Each port's outcome, in order. Ports owned by one target share one
    /// start; targets start one after another.
    fn start_docker(&self, ports: &[u16]) -> Vec<Outcome> {
        let listing = self.list_containers();
        let mut targets: Vec<DockerTarget> = Vec::new();
        let owners: Vec<Result<usize, String>> = ports
            .iter()
            .map(|port| {
                let (ps_all, inspect) = listing.as_ref().map_err(String::clone)?;
                let target = docker_owners(ps_all, inspect, *port)?;
                Ok(targets
                    .iter()
                    .position(|seen| *seen == target)
                    .unwrap_or_else(|| {
                        targets.push(target);
                        targets.len() - 1
                    }))
            })
            .collect();
        let outcomes: Vec<Outcome> = targets
            .iter()
            .map(|target| self.start_target(target))
            .collect();
        owners
            .into_iter()
            .map(|owner| match owner {
                Ok(index) => outcomes[index].clone(),
                Err(error) => (Action::None, String::new(), Some(error)),
            })
            .collect()
    }

    /// Launches one start command in herdr, with `~` in `dir` expanded.
    fn start_herdr(&self, prefix: &str, label: &str, dir: &Path, run: &str) -> Outcome {
        let failure = |error| (Action::None, String::new(), Some(error));
        let Some(socket) = &self.herdr else {
            return failure(not_running(&self.machine));
        };
        let client = HerdrClient {
            socket: socket.clone(),
            timeout: self.herdr_timings.call,
        };
        let dir = match std::env::var_os("HOME") {
            Some(home) => expand_dir(&dir.to_string_lossy(), Path::new(&home)),
            None => dir.to_path_buf(),
        };
        match launch(
            &client,
            prefix,
            label,
            &dir,
            run,
            &self.herdr_timings,
            &self.machine,
        ) {
            Ok(detail) => (Action::Herdr, detail, None),
            Err(error) => failure(error),
        }
    }
}

const DOCKER_MISSING: &str = "docker was not found";

impl Starter for SystemStarter {
    /// The docker ports and the herdr entries start at the same time, on two
    /// threads; within each half, one start follows another.
    fn start(&self, plan: &[PortStart]) -> Vec<PortResult> {
        let docker_ports: Vec<u16> = plan
            .iter()
            .filter(|entry| entry.kind == StartKind::Docker)
            .map(|entry| entry.port)
            .collect();
        let (docker_outcomes, herdr_outcomes) = std::thread::scope(|scope| {
            let docker = scope.spawn(|| match docker_ports.is_empty() {
                true => Vec::new(),
                false => self.start_docker(&docker_ports),
            });
            let herdr = scope.spawn(|| {
                plan.iter()
                    .filter_map(|entry| match &entry.kind {
                        StartKind::Docker => None,
                        StartKind::Herdr {
                            prefix,
                            label,
                            dir,
                            run,
                        } => Some(self.start_herdr(prefix, label, dir, run)),
                    })
                    .collect::<Vec<_>>()
            });
            let join = |half: std::thread::ScopedJoinHandle<'_, Vec<Outcome>>| {
                half.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            };
            (join(docker), join(herdr))
        });
        let (mut docker_outcomes, mut herdr_outcomes) =
            (docker_outcomes.into_iter(), herdr_outcomes.into_iter());
        plan.iter()
            .map(|entry| {
                let (action, detail, error) = match entry.kind {
                    StartKind::Docker => {
                        docker_outcomes.next().expect("one outcome per docker port")
                    }
                    StartKind::Herdr { .. } => {
                        herdr_outcomes.next().expect("one outcome per herdr entry")
                    }
                };
                PortResult {
                    port: entry.port,
                    action,
                    detail,
                    error,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop::tests::{script, temp_dir};

    #[test]
    fn bindings_parse_single_range_and_empty() {
        let json = r#"{"8000/tcp":[{"HostIp":"","HostPort":"8002"}],"9000/tcp":[{"HostPort":"9000-9002"}],"5432/tcp":[{"HostPort":""}]}"#;
        let mut bindings = parse_bindings(json);
        bindings.sort();
        assert_eq!(bindings, [(8002, 8002), (9000, 9002)]);
        assert_eq!(parse_bindings("{}"), []);
        assert_eq!(parse_bindings("null"), []);
    }

    const PS: &str = "aaa\thuvr_jds2\nbbb\t\nccc\thuvr_jds2\n";
    const INSPECT: &str = "aaa\t/web\t{\"8000/tcp\":[{\"HostPort\":\"8002\"}]}\n\
                           bbb\t/solo\t{\"80/tcp\":[{\"HostPort\":\"8080\"}]}\n\
                           ccc\t/db\t{\"5432/tcp\":[{\"HostPort\":\"5433\"}]}\n";

    #[test]
    fn owner_is_the_compose_project() {
        assert_eq!(
            docker_owners(PS, INSPECT, 8002),
            Ok(DockerTarget::Compose("huvr_jds2".into()))
        );
    }

    #[test]
    fn owner_without_project_is_the_container() {
        assert_eq!(
            docker_owners(PS, INSPECT, 8080),
            Ok(DockerTarget::Container {
                id: "bbb".into(),
                name: "solo".into()
            })
        );
    }

    #[test]
    fn two_owners_fail() {
        let inspect = "aaa\t/web\t{\"1/tcp\":[{\"HostPort\":\"8002\"}]}\n\
                       bbb\t/solo\t{\"1/tcp\":[{\"HostPort\":\"8002\"}]}\n";
        assert_eq!(
            docker_owners(PS, inspect, 8002),
            Err("several containers publish it: web, solo".into())
        );
    }

    #[test]
    fn no_owner_fails() {
        assert_eq!(
            docker_owners(PS, INSPECT, 9999),
            Err("no start command, and no container publishes it".into())
        );
    }

    fn starter(docker: Option<PathBuf>) -> SystemStarter {
        SystemStarter {
            docker,
            list_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(10),
            herdr: None,
            herdr_timings: HerdrTimings::default(),
            machine: "test".into(),
        }
    }

    /// A stand-in docker: `ps` and `inspect` print canned output (the
    /// `failing` one of them fails instead), anything else is appended to
    /// the log.
    fn fake_docker(
        name: &str,
        ps: &str,
        inspect: &str,
        failing: Option<&str>,
    ) -> (PathBuf, PathBuf) {
        let dir = temp_dir(name);
        std::fs::write(dir.join("ps"), ps).unwrap();
        std::fs::write(dir.join("inspect"), inspect).unwrap();
        let (docker, log) = (dir.join("docker"), dir.join("log"));
        let body = |command: &str| match failing == Some(command) {
            true => "echo 'Cannot connect to the Docker daemon' >&2; exit 1".to_string(),
            false => format!("cat {command}; exit 0"),
        };
        script(
            &docker,
            &format!(
                "cd '{dir}'\nif [ \"$1\" = ps ]; then {ps}; fi\n\
                 if [ \"$1\" = inspect ]; then {inspect}; fi\n\
                 echo \"$@\" >> log",
                dir = dir.display(),
                ps = body("ps"),
                inspect = body("inspect"),
            ),
        );
        (docker, log)
    }

    fn docker_plan(ports: &[u16]) -> Vec<PortStart> {
        ports
            .iter()
            .map(|port| PortStart {
                port: *port,
                kind: StartKind::Docker,
            })
            .collect()
    }

    #[test]
    fn docker_ports_of_one_project_start_once() {
        let inspect = "aaa\t/web\t{\"1/tcp\":[{\"HostPort\":\"8002\"}]}\n\
                       ccc\t/db\t{\"1/tcp\":[{\"HostPort\":\"5433\"}]}\n\
                       ddd\t/api\t{\"1/tcp\":[{\"HostPort\":\"8003\"}]}\n\
                       bbb\t/solo\t{\"1/tcp\":[{\"HostPort\":\"8080\"}]}\n";
        let ps = "aaa\thuvr_jds2\nccc\thuvr_jds2\nddd\thuvr_jds2\nbbb\t\n";
        let (docker, log) = fake_docker("project", ps, inspect, None);
        let results = starter(Some(docker.clone())).start(&docker_plan(&[8002, 5433, 8003]));
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "compose -p huvr_jds2 start\n"
        );
        assert_eq!(results.len(), 3);
        for (result, port) in results.iter().zip([8002, 5433, 8003]) {
            assert_eq!(result.port, port);
            assert_eq!(result.action, Action::Compose);
            assert_eq!(result.detail, "docker compose -p huvr_jds2 start");
            assert_eq!(result.error, None);
        }
        let _ = std::fs::remove_file(&log);
        let results = starter(Some(docker)).start(&docker_plan(&[8080]));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "start bbb\n");
        assert_eq!(results[0].action, Action::Container);
        assert_eq!(results[0].detail, "docker start bbb");
        assert_eq!(results[0].error, None);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn unowned_docker_port_fails_without_starting_anything() {
        let (docker, log) = fake_docker("unowned", PS, INSPECT, None);
        let results = starter(Some(docker)).start(&docker_plan(&[9999, 8080]));
        assert_eq!(
            results[0].error.as_deref(),
            Some("no start command, and no container publishes it")
        );
        assert_eq!(results[0].action, Action::None);
        assert_eq!(results[1].error, None, "the other port still starts");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "start bbb\n");
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn docker_missing_fails_each_docker_port() {
        let results = starter(None).start(&docker_plan(&[8002, 8080]));
        assert_eq!(results.len(), 2);
        for result in results {
            assert_eq!(result.action, Action::None);
            assert_eq!(result.error.as_deref(), Some("docker was not found"));
        }
    }

    #[test]
    fn docker_listing_failure_is_reported() {
        for (failing, shown) in [("ps", "docker ps -a"), ("inspect", "docker inspect")] {
            let (docker, log) = fake_docker("listing", PS, INSPECT, Some(failing));
            let results = starter(Some(docker)).start(&docker_plan(&[8002, 8080]));
            assert_eq!(results.len(), 2);
            for result in &results {
                assert_eq!(result.action, Action::None);
                assert_eq!(
                    result.error.as_deref(),
                    Some(&*format!("{shown}: Cannot connect to the Docker daemon"))
                );
            }
            let _ = std::fs::remove_dir_all(log.parent().unwrap());
        }
    }

    #[test]
    fn typed_line_quotes_dir() {
        assert_eq!(
            typed_line(Path::new("/tmp/a b/it's"), "npm start", "N1"),
            r#"cd -- '/tmp/a b/it'\''s' && { npm start; }; echo "[whatsup N1] exited with $?""#
        );
    }

    #[test]
    fn typed_line_runs_in_a_shell() {
        // The quoting must survive a real shell: a dir with a space and a
        // quote, as in `~/work/it's here`.
        let dir = temp_dir("it's here");
        let line = typed_line(&dir, "pwd -P", "N2");
        let output = std::process::Command::new("sh")
            .args(["-c", &line])
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let shown = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(
            stdout,
            format!("{}\n[whatsup N2] exited with 0\n", shown.display())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn early_exit_finds_only_this_nonce() {
        assert_eq!(
            early_exit("x\n[whatsup OLD] exited with 1\n% ", "NEW"),
            None
        );
        let text = [
            "old output",
            r#"% cd -- '/x' && { y; }; echo "[whatsup NEW] exited with $?""#,
            "a",
            "",
            "boom",
            "[whatsup NEW] exited with 127",
            "% ",
        ]
        .join("\n");
        assert_eq!(
            early_exit(&text, "NEW"),
            Some((127, vec!["a".to_string(), "boom".to_string()]))
        );
        // The echo alone is not the marker.
        assert_eq!(early_exit(&text.replace("127", "x"), "NEW"), None);
    }

    #[test]
    fn early_exit_keeps_the_last_five_lines() {
        let mut lines = vec![r#"% echo "[whatsup N] exited with $?""#.to_string()];
        lines.extend((1..=7).map(|n| format!("line {n}")));
        lines.push("[whatsup N] exited with 2".into());
        let expected: Vec<String> = (3..=7).map(|n| format!("line {n}")).collect();
        assert_eq!(early_exit(&lines.join("\n"), "N"), Some((2, expected)));
        // The echo scrolled away: the lines above the marker still count.
        assert_eq!(
            early_exit("boom\n[whatsup N] exited with 1", "N"),
            Some((1, vec!["boom".to_string()]))
        );
    }

    use crate::herdr::testing::{socket_path, stuck_herdr, FakeHerdr};
    use serde_json::{json, Value};
    use std::time::Instant;

    fn quick() -> HerdrTimings {
        HerdrTimings {
            call: Duration::from_millis(500),
            prompt_wait: Duration::from_millis(300),
            prompt_poll: Duration::from_millis(10),
            watch: Duration::from_millis(150),
            watch_poll: Duration::from_millis(30),
            link_budget: Duration::from_secs(3),
        }
    }

    fn fake_herdr() -> (FakeHerdr, HerdrClient) {
        let path = socket_path();
        let fake = FakeHerdr::start(&path);
        let client = HerdrClient {
            socket: path,
            timeout: quick().call,
        };
        (fake, client)
    }

    fn launch_web(client: &HerdrClient) -> Result<String, String> {
        launch(
            client,
            "h2",
            "web",
            Path::new("/tmp/a b"),
            "npm start",
            &quick(),
            "lucy",
        )
    }

    /// The nonce in a typed line.
    fn nonce_of(line: &str) -> &str {
        let start = line.rfind("[whatsup ").unwrap() + "[whatsup ".len();
        &line[start..start + line[start..].find(']').unwrap()]
    }

    /// Where `method` first appears in `requests`, and its params.
    fn find(requests: &[(String, Value)], method: &str) -> Option<(usize, Value)> {
        requests
            .iter()
            .position(|(seen, _)| seen == method)
            .map(|index| (index, requests[index].1.clone()))
    }

    fn typed(requests: &[(String, Value)]) -> Vec<Value> {
        requests
            .iter()
            .filter(|(method, _)| method == "pane.send_input")
            .map(|(_, params)| params.clone())
            .collect()
    }

    #[test]
    fn launch_creates_workspace_when_none_matches() {
        let (fake, client) = fake_herdr();
        fake.add_workspace(1, "h20");
        assert_eq!(launch_web(&client), Ok("herdr tab web in h2".into()));
        let requests = fake.requests();
        let (create, params) = find(&requests, "workspace.create").unwrap();
        assert_eq!(
            params,
            json!({ "label": "h2", "cwd": "/tmp/a b", "focus": false })
        );
        let (rename, params) = find(&requests, "tab.rename").unwrap();
        assert_eq!(params["label"], "web");
        let (send, params) = find(&requests, "pane.send_input").unwrap();
        let line = params["text"].as_str().unwrap();
        assert_eq!(
            line,
            typed_line(Path::new("/tmp/a b"), "npm start", nonce_of(line))
        );
        assert_eq!(params["keys"], json!(["Enter"]));
        assert!(create < rename && rename < send, "{requests:?}");
        let (read, params) = find(&requests, "pane.read").unwrap();
        assert!(read < send, "waits for the prompt in a new pane");
        assert_eq!(params["source"], "recent_unwrapped");
        assert_eq!(find(&requests, "tab.create"), None);
    }

    #[test]
    fn launch_uses_first_matching_workspace_and_new_tab() {
        let (fake, client) = fake_herdr();
        fake.add_workspace(3, "h2-x");
        let first = fake.add_workspace(1, "h2-y");
        assert_eq!(launch_web(&client), Ok("herdr tab web in h2-y".into()));
        let requests = fake.requests();
        let (create, params) = find(&requests, "tab.create").unwrap();
        assert_eq!(
            params,
            json!({ "workspace_id": first, "cwd": "/tmp/a b", "label": "web", "focus": false })
        );
        let (send, _) = find(&requests, "pane.send_input").unwrap();
        let (read, _) = find(&requests, "pane.read").unwrap();
        assert!(create < read && read < send, "{requests:?}");
        assert_eq!(find(&requests, "workspace.create"), None);
    }

    #[test]
    fn launch_reuses_idle_tab() {
        let (fake, client) = fake_herdr();
        let ws = fake.add_workspace(1, "h2");
        fake.add_tab(&ws, "shell");
        let (_, pane) = fake.add_tab(&ws, "Web");
        assert_eq!(launch_web(&client), Ok("herdr tab web in h2".into()));
        let requests = fake.requests();
        assert_eq!(find(&requests, "tab.create"), None);
        assert_eq!(find(&requests, "workspace.create"), None);
        let (send, params) = find(&requests, "pane.send_input").unwrap();
        assert_eq!(params["pane_id"], json!(pane));
        let (read, _) = find(&requests, "pane.read").unwrap();
        assert!(send < read, "no prompt wait in a reused tab: {requests:?}");
    }

    #[test]
    fn launch_refuses_busy_tab() {
        let (fake, client) = fake_herdr();
        let ws = fake.add_workspace(1, "h2-main");
        let (_, pane) = fake.add_tab(&ws, "web");
        fake.set_busy(&pane);
        assert_eq!(
            launch_web(&client),
            Err("herdr tab \"web\" in h2-main is busy; it may still be starting".into())
        );
        assert_eq!(typed(&fake.requests()), Vec::<Value>::new());
    }

    #[test]
    fn launch_split_tab_uses_first_pane() {
        let (fake, client) = fake_herdr();
        let ws = fake.add_workspace(1, "h2");
        let (tab, first) = fake.add_tab(&ws, "web");
        let second = fake.split(&tab);
        fake.set_busy(&second);
        assert_eq!(launch_web(&client), Ok("herdr tab web in h2".into()));
        let sent = typed(&fake.requests());
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["pane_id"], json!(first));

        let (fake, client) = fake_herdr();
        let ws = fake.add_workspace(1, "h2");
        let (tab, first) = fake.add_tab(&ws, "web");
        fake.split(&tab);
        fake.set_busy(&first);
        assert!(launch_web(&client).unwrap_err().contains("is busy"));
        assert_eq!(typed(&fake.requests()), Vec::<Value>::new());
    }

    #[test]
    fn launch_unknown_shell_state_is_busy() {
        for (shell, foreground) in [(None, Some(100)), (Some(100), None)] {
            let (fake, client) = fake_herdr();
            let ws = fake.add_workspace(1, "h2");
            let (_, pane) = fake.add_tab(&ws, "web");
            fake.set_shell(&pane, shell, foreground);
            assert!(launch_web(&client).unwrap_err().contains("is busy"));
            assert_eq!(typed(&fake.requests()), Vec::<Value>::new());
        }
    }

    #[test]
    fn launch_reports_early_exit() {
        let (fake, client) = fake_herdr();
        fake.on_type(|line| format!("boom\n[whatsup {}] exited with 1", nonce_of(line)));
        let error = launch_web(&client).unwrap_err();
        assert_eq!(error, "npm start exited with 1: boom");
        assert!(!error.starts_with("port"));
    }

    #[test]
    fn launch_ignores_stale_marker() {
        let (fake, client) = fake_herdr();
        let ws = fake.add_workspace(1, "h2");
        let (_, pane) = fake.add_tab(&ws, "web");
        fake.set_text(
            &pane,
            "% npm start\nboom\n[whatsup oldnonce] exited with 1\n% ",
        );
        fake.on_type(|_| "listening on 3000".into());
        assert_eq!(launch_web(&client), Ok("herdr tab web in h2".into()));
        assert_eq!(typed(&fake.requests()).len(), 1);
    }

    #[test]
    fn launch_reads_a_narrow_pane_unwrapped() {
        // In a 20-column split both the echoed line and the marker wrap on
        // screen; read unwrapped, the lines between them are still exact.
        let (fake, client) = fake_herdr();
        let ws = fake.add_workspace(1, "h2");
        let (_, pane) = fake.add_tab(&ws, "web");
        fake.set_text(&pane, "old scrollback line\n% ");
        fake.set_width(&pane, 20);
        fake.on_type(|line| format!("a\n\nboom\n[whatsup {}] exited with 7", nonce_of(line)));
        assert_eq!(
            launch_web(&client),
            Err("npm start exited with 7: a / boom".into())
        );
        let reads: Vec<Value> = fake
            .requests()
            .into_iter()
            .filter(|(method, _)| method == "pane.read")
            .map(|(_, params)| params["source"].clone())
            .collect();
        assert!(!reads.is_empty());
        assert!(
            reads.iter().all(|source| source == "recent_unwrapped"),
            "{reads:?}"
        );
    }

    #[test]
    fn launch_without_herdr_fails() {
        let client = HerdrClient {
            socket: socket_path(),
            timeout: quick().call,
        };
        assert_eq!(
            launch_web(&client),
            Err("herdr is not running on lucy".into())
        );
    }

    #[test]
    fn launch_reports_herdr_errors() {
        // A herdr that answers with an error is running: say what it said.
        let (fake, client) = fake_herdr();
        fake.add_workspace(1, "h2");
        fake.fail("tab.create", "cwd does not exist");
        assert_eq!(launch_web(&client), Err("herdr: cwd does not exist".into()));
        assert_eq!(typed(&fake.requests()), Vec::<Value>::new());
    }

    #[test]
    fn launch_against_a_stuck_herdr_times_out() {
        let path = socket_path();
        let listener = stuck_herdr(&path);
        let timings = HerdrTimings {
            call: Duration::from_secs(5),
            link_budget: Duration::from_millis(300),
            ..quick()
        };
        let client = HerdrClient {
            socket: path.clone(),
            timeout: timings.call,
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let result = launch(
                &client,
                "h2",
                "web",
                Path::new("/tmp"),
                "true",
                &timings,
                "lucy",
            );
            let _ = done_tx.send(result);
        });
        let result = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("launch hung on a stuck herdr");
        assert_eq!(result, Err("herdr is not running on lucy".into()));
        assert!(started.elapsed() < Duration::from_millis(800));
        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    fn herdr_entry(port: u16, label: &str, dir: &str) -> PortStart {
        PortStart {
            port,
            kind: StartKind::Herdr {
                prefix: "h2".into(),
                label: label.into(),
                dir: dir.into(),
                run: "npm start".into(),
            },
        }
    }

    #[test]
    fn system_starter_mixes_docker_and_herdr() {
        let (docker, log) = fake_docker("mixed", PS, INSPECT, None);
        let plan = [
            herdr_entry(3002, "web", "/tmp"),
            docker_plan(&[8002]).remove(0),
            herdr_entry(4002, "api", "/tmp"),
        ];
        for herdr in [Some(socket_path()), None] {
            let starter = SystemStarter {
                herdr,
                herdr_timings: quick(),
                machine: "lucy".into(),
                ..starter(Some(docker.clone()))
            };
            let results = starter.start(&plan);
            let ports: Vec<u16> = results.iter().map(|result| result.port).collect();
            assert_eq!(ports, [3002, 8002, 4002]);
            assert_eq!(results[1].action, Action::Compose);
            assert_eq!(results[1].error, None);
            for result in [&results[0], &results[2]] {
                assert_eq!(result.action, Action::None);
                assert_eq!(
                    result.error.as_deref(),
                    Some("herdr is not running on lucy")
                );
            }
        }
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn system_starter_launches_herdr_entries_in_order() {
        let (fake, client) = fake_herdr();
        let starter = SystemStarter {
            herdr: Some(client.socket.clone()),
            herdr_timings: quick(),
            machine: "lucy".into(),
            ..starter(None)
        };
        let results = starter.start(&[
            herdr_entry(3002, "web", "~/a b"),
            herdr_entry(4002, "api", "/srv/api"),
        ]);
        for (result, detail) in results
            .iter()
            .zip(["herdr tab web in h2", "herdr tab api in h2"])
        {
            assert_eq!(result.action, Action::Herdr);
            assert_eq!(result.detail, detail);
            assert_eq!(result.error, None);
        }
        let methods = fake.methods();
        let count = |method: &str| methods.iter().filter(|seen| *seen == method).count();
        assert_eq!(count("workspace.create"), 1);
        assert_eq!(count("tab.create"), 1);
        let home = std::env::var("HOME").unwrap();
        let sent = typed(&fake.requests());
        let first = sent[0]["text"].as_str().unwrap();
        assert!(
            first.starts_with(&format!("cd -- '{home}/a b' && ")),
            "{first}"
        );
        assert!(sent[1]["text"]
            .as_str()
            .unwrap()
            .starts_with("cd -- '/srv/api' && "));
        assert_ne!(nonce_of(first), nonce_of(sent[1]["text"].as_str().unwrap()));
    }
}
