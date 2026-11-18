//! Starting what serves a row's ports on this machine.
//!
//! A port with no start command is started through docker: the container
//! that publishes it already exists, stopped, so its whole compose project
//! (or the single container) is started again. A port with a start command
//! is typed into a herdr tab (see the herdr half, added separately).

use std::path::PathBuf;
use std::time::Duration;

use crate::stop::{find_docker, run_docker, Action, PortResult};

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
    /// docker's own error.
    fn list_containers(&self) -> Result<(String, String), String> {
        let docker = self.docker.as_deref().ok_or(DOCKER_MISSING)?;
        let ps_all = run_docker(
            docker,
            &["ps", "-a", "--no-trunc", "--format", DOCKER_PS_ALL_FORMAT],
            self.list_timeout,
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
        let inspect = run_docker(docker, &args, self.list_timeout)?;
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
}

const DOCKER_MISSING: &str = "docker was not found";

impl Starter for SystemStarter {
    fn start(&self, plan: &[PortStart]) -> Vec<PortResult> {
        let docker_ports: Vec<u16> = plan
            .iter()
            .filter(|entry| entry.kind == StartKind::Docker)
            .map(|entry| entry.port)
            .collect();
        let mut docker_outcomes = if docker_ports.is_empty() {
            Vec::new()
        } else {
            self.start_docker(&docker_ports)
        }
        .into_iter();
        plan.iter()
            .map(|entry| {
                let (action, detail, error) = match entry.kind {
                    StartKind::Docker => {
                        docker_outcomes.next().expect("one outcome per docker port")
                    }
                    StartKind::Herdr { .. } => (
                        Action::None,
                        String::new(),
                        Some("herdr start is not built yet".into()),
                    ),
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

    /// A stand-in docker: `ps` and `inspect` print canned output (`ps` fails
    /// when `ps_failure`), anything else is appended to the log.
    fn fake_docker(name: &str, ps: &str, inspect: &str, ps_failure: bool) -> (PathBuf, PathBuf) {
        let dir = temp_dir(name);
        std::fs::write(dir.join("ps"), ps).unwrap();
        std::fs::write(dir.join("inspect"), inspect).unwrap();
        let (docker, log) = (dir.join("docker"), dir.join("log"));
        let ps_body = if ps_failure {
            "echo 'Cannot connect to the Docker daemon' >&2; exit 1"
        } else {
            "cat ps; exit 0"
        };
        script(
            &docker,
            &format!(
                "cd '{dir}'\nif [ \"$1\" = ps ]; then {ps_body}; fi\n\
                 if [ \"$1\" = inspect ]; then cat inspect; exit 0; fi\n\
                 echo \"$@\" >> log",
                dir = dir.display()
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
        let (docker, log) = fake_docker("project", ps, inspect, false);
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
        let (docker, log) = fake_docker("unowned", PS, INSPECT, false);
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
        let (docker, log) = fake_docker("listing", "", "", true);
        let results = starter(Some(docker)).start(&docker_plan(&[8002, 8080]));
        assert_eq!(results.len(), 2);
        for result in &results {
            assert_eq!(result.action, Action::None);
            let error = result.error.as_deref().unwrap();
            assert!(
                error.starts_with("docker ps -a")
                    && error.ends_with(": Cannot connect to the Docker daemon"),
                "{error}"
            );
        }
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn herdr_entries_are_not_built_yet() {
        let plan = [PortStart {
            port: 3000,
            kind: StartKind::Herdr {
                prefix: "p".into(),
                label: "l".into(),
                dir: "/tmp".into(),
                run: "true".into(),
            },
        }];
        let results = starter(None).start(&plan);
        assert_eq!(
            results[0].error.as_deref(),
            Some("herdr start is not built yet")
        );
    }
}
