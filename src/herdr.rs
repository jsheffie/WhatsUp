//! Reads the workspace list from a herdr server over its Unix socket, and
//! matches workspace labels to dashboard cells by name prefix.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// How long one workspace query may take in total, connect to reply.
pub const QUERY_BUDGET: Duration = Duration::from_secs(1);
/// A reply larger than this is not herdr's.
const MAX_REPLY: usize = 1024 * 1024;
const REQUEST: &[u8] = b"{\"id\":\"whatsup\",\"method\":\"workspace.list\",\"params\":{}}\n";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HerdrWorkspace {
    pub number: u64,
    pub label: String,
}

/// Whether `label` names a workspace for `prefix`: the label is the prefix,
/// or continues it after a separator (`h1`, `h1-herberth`, but not `h10`).
pub fn label_matches(label: &str, prefix: &str) -> bool {
    let (Some(head), Some(rest)) = (label.get(..prefix.len()), label.get(prefix.len()..)) else {
        return false;
    };
    head.eq_ignore_ascii_case(prefix)
        && rest
            .chars()
            .next()
            .is_none_or(|next| !next.is_ascii_alphanumeric())
}

/// Labels of the workspaces matching `prefix`, in herdr's sidebar order.
pub fn matching_labels(workspaces: &[HerdrWorkspace], prefix: &str) -> Vec<String> {
    let mut matched: Vec<&HerdrWorkspace> = workspaces
        .iter()
        .filter(|workspace| label_matches(&workspace.label, prefix))
        .collect();
    matched.sort_by_key(|workspace| workspace.number);
    matched
        .into_iter()
        .map(|workspace| workspace.label.clone())
        .collect()
}

#[derive(Deserialize)]
struct Reply {
    result: ReplyResult,
}

#[derive(Deserialize)]
struct ReplyResult {
    #[serde(rename = "type")]
    kind: String,
    workspaces: Vec<HerdrWorkspace>,
}

/// The workspaces in one `workspace.list` reply line; `None` for an error
/// reply or anything else.
pub fn parse_reply(line: &str) -> Option<Vec<HerdrWorkspace>> {
    let reply: Reply = serde_json::from_str(line).ok()?;
    (reply.result.kind == "workspace_list").then_some(reply.result.workspaces)
}

/// Asks the herdr server on `socket` for its workspaces. `None` when there
/// is no server, it fails, or it does not answer within `budget`.
pub fn list_workspaces(socket: &Path, budget: Duration) -> Option<Vec<HerdrWorkspace>> {
    let deadline = Instant::now() + budget;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    };
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(remaining()?)).ok()?;
    stream.write_all(REQUEST).ok()?;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        stream.set_read_timeout(Some(remaining()?)).ok()?;
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        reply.extend_from_slice(&chunk[..read]);
        if let Some(end) = reply.iter().position(|byte| *byte == b'\n') {
            return parse_reply(std::str::from_utf8(&reply[..end]).ok()?);
        }
        if reply.len() > MAX_REPLY {
            return None;
        }
    }
}

/// herdr's default-session socket on this machine.
pub fn local_socket(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let absolute = |key: &str| {
        env(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    absolute("XDG_CONFIG_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".config")))
        .map(|config| config.join("herdr").join("herdr.sock"))
}

#[cfg(test)]
pub(crate) mod testing {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    /// A herdr server on `path` answering every `workspace.list` with
    /// `workspaces` (number, label). Runs for the rest of the test process.
    pub fn serve_labels(path: &Path, workspaces: &[(u64, &str)]) {
        let list: Vec<_> = workspaces
            .iter()
            .map(|(number, label)| {
                serde_json::json!({ "workspace_id": format!("w{number}"), "number": number, "label": label })
            })
            .collect();
        let reply = serde_json::json!({
            "id": "whatsup",
            "result": { "type": "workspace_list", "workspaces": list },
        })
        .to_string();
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_ok() {
                    let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
                }
            }
        });
    }

    /// A herdr server on `path` that accepts and never answers.
    pub fn serve_silence(path: &Path) {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    const FIXTURE: &str = r#"{"id":"whatsup","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1","number":2,"label":"h1","focused":false,"pane_count":1,"agent_status":"unknown","future_field":1}]}}"#;

    fn ws(number: u64, label: &str) -> HerdrWorkspace {
        HerdrWorkspace {
            number,
            label: label.into(),
        }
    }

    /// A short, unique socket path (macOS limits these to ~104 bytes).
    fn socket_path() -> std::path::PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "wu-herdr-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Serves one connection on a fresh socket with `serve`.
    fn fake_herdr(
        serve: impl FnOnce(std::os::unix::net::UnixStream) + Send + 'static,
    ) -> std::path::PathBuf {
        let path = socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve(stream);
            }
        });
        path
    }

    #[test]
    fn prefix_boundaries() {
        for (label, prefix) in [
            ("h1", "h1"),
            ("h1-herberth", "h1"),
            ("H1-x", "h1"),
            ("h1 x", "h1"),
            ("h1_x", "h1"),
            ("h1.x", "h1"),
        ] {
            assert!(
                label_matches(label, prefix),
                "{label} should match {prefix}"
            );
        }
        for (label, prefix) in [
            ("h10-e2e-testing", "h1"),
            ("h1x", "h1"),
            ("xh1", "h1"),
            ("h", "h1"),
        ] {
            assert!(
                !label_matches(label, prefix),
                "{label} should not match {prefix}"
            );
        }
    }

    #[test]
    fn matching_orders_by_number() {
        let workspaces = [ws(5, "h6-b"), ws(3, "h6"), ws(4, "h60")];
        assert_eq!(matching_labels(&workspaces, "h6"), vec!["h6", "h6-b"]);
    }

    #[test]
    fn parse_reply_reads_real_shape() {
        assert_eq!(parse_reply(FIXTURE), Some(vec![ws(2, "h1")]));
        assert_eq!(
            parse_reply(r#"{"id":"whatsup","error":{"code":"invalid_request","message":"x"}}"#),
            None
        );
        assert_eq!(parse_reply("not json"), None);
        assert_eq!(
            parse_reply(r#"{"id":"whatsup","result":{"type":"pane_list","workspaces":[]}}"#),
            None
        );
    }

    #[test]
    fn list_workspaces_round_trip() {
        let (sent_tx, sent_rx) = std::sync::mpsc::channel();
        let path = fake_herdr(move |stream| {
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            sent_tx.send(line).unwrap();
            (&stream)
                .write_all(format!("{FIXTURE}\n").as_bytes())
                .unwrap();
        });
        assert_eq!(
            list_workspaces(&path, QUERY_BUDGET),
            Some(vec![ws(2, "h1")])
        );
        assert_eq!(
            sent_rx.recv().unwrap(),
            "{\"id\":\"whatsup\",\"method\":\"workspace.list\",\"params\":{}}\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn list_workspaces_eof_is_fast_none() {
        let path = fake_herdr(drop);
        let started = Instant::now();
        assert_eq!(list_workspaces(&path, QUERY_BUDGET), None);
        assert!(started.elapsed() < Duration::from_millis(200));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn list_workspaces_silent_server_times_out() {
        let path = fake_herdr(|stream| {
            std::thread::sleep(Duration::from_secs(2));
            drop(stream);
        });
        let started = Instant::now();
        assert_eq!(list_workspaces(&path, Duration::from_millis(300)), None);
        assert!(started.elapsed() < Duration::from_millis(600));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn list_workspaces_missing_socket_is_none() {
        assert_eq!(list_workspaces(&socket_path(), QUERY_BUDGET), None);
    }

    #[test]
    fn local_socket_prefers_absolute_xdg() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            local_socket(&env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/home/a")])),
            Some(PathBuf::from("/x/herdr/herdr.sock"))
        );
        assert_eq!(
            local_socket(&env(&[("XDG_CONFIG_HOME", "rel"), ("HOME", "/home/a")])),
            Some(PathBuf::from("/home/a/.config/herdr/herdr.sock"))
        );
        assert_eq!(local_socket(&env(&[])), None);
    }
}
