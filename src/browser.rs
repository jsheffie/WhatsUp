//! Opening dashboard links in the configured browser.
//!
//! macOS drives the app named by `browser_app` through AppleScript, so a link
//! lands in the front window as a tab (or in a new window).
//! Linux and other Unix systems launch a browser command: `browser_app` is a
//! command name or path, and the usual macOS app names ("Google Chrome",
//! "Chromium", "Firefox", ...) map to their Linux commands so one config works
//! on both. Without any matching command, `xdg-open` opens the link in the
//! desktop's default browser.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OpenMode {
    #[default]
    Tab,
    Window,
}

pub fn open_in_browser(app: &str, url: &str, mode: OpenMode) -> Result<(), String> {
    validate_text(app, "browser_app")?;
    validate_text(url, "url")?;
    platform_open(app, url, mode)
}

#[cfg(target_os = "macos")]
fn platform_open(app: &str, url: &str, mode: OpenMode) -> Result<(), String> {
    let script = apple_script(app, url, mode);
    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|err| format!("could not run osascript: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let fallback = Command::new("open")
        .args(["-a", app, url])
        .status()
        .map_err(|err| format!("could not run open: {err}"))?;
    if fallback.success() {
        return Ok(());
    }
    Err(format!("could not open {url} in {app}: {}", stderr.trim()))
}

#[cfg(not(target_os = "macos"))]
fn platform_open(app: &str, url: &str, mode: OpenMode) -> Result<(), String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let launch = linux_launch(app, url, mode, |name| find_executable(name, &dirs))
        .ok_or_else(|| format!("could not find {app} or xdg-open to open {url}"))?;
    spawn_detached(&launch)
}

/// The command that opens `url` on Linux, or None when nothing is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
}

pub fn linux_launch(
    app: &str,
    url: &str,
    mode: OpenMode,
    find: impl Fn(&str) -> Option<PathBuf>,
) -> Option<Launch> {
    if let Some(program) = browser_commands(app).iter().find_map(|name| find(name)) {
        let mut args = Vec::new();
        if mode == OpenMode::Window {
            // Chromium-family browsers and Firefox all take --new-window; a
            // bare URL opens a tab in the running browser.
            args.push("--new-window".to_string());
        }
        args.push(url.to_string());
        return Some(Launch { program, args });
    }
    find("xdg-open").map(|program| Launch {
        program,
        args: vec![url.to_string()],
    })
}

/// Opens `url` in the Chrome profile whose display name is `profile_name`.
///
/// AppleScript cannot pick a profile, so this launches Chrome with
/// `--profile-directory`; a running Chrome takes the URL over.
pub fn open_in_profile(app: &str, profile_name: &str, url: &str) -> Result<(), String> {
    validate_text(app, "browser_app")?;
    validate_text(profile_name, "browser_profile")?;
    validate_text(url, "url")?;
    let state_path = chrome_local_state_path().ok_or("could not find Chrome's Local State file")?;
    let state = std::fs::read_to_string(&state_path)
        .map_err(|err| format!("could not read {}: {err}", state_path.display()))?;
    let dir = profile_dir_for_name(&state, profile_name)
        .ok_or_else(|| format!("Chrome profile \"{profile_name}\" not found"))?;
    spawn_detached(&platform_profile_launch(app, &dir, url)?)
}

/// The profile directory (`Default`, `Profile 7`, ...) whose display name is
/// exactly `name`, from the JSON of Chrome's `Local State` file.
pub fn profile_dir_for_name(local_state_json: &str, name: &str) -> Option<String> {
    let state: serde_json::Value = serde_json::from_str(local_state_json).ok()?;
    state
        .get("profile")?
        .get("info_cache")?
        .as_object()?
        .iter()
        .find(|(_, info)| info.get("name").and_then(|value| value.as_str()) == Some(name))
        .map(|(dir, _)| dir.clone())
}

pub fn chrome_local_state_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").filter(|home| !home.is_empty())?);
    let root = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Google/Chrome")
    } else {
        home.join(".config/google-chrome")
    };
    Some(root.join("Local State"))
}

pub fn mac_profile_launch(app: &str, profile_dir: &str, url: &str) -> Launch {
    Launch {
        program: PathBuf::from("open"),
        args: vec![
            "-na".to_string(),
            app.to_string(),
            "--args".to_string(),
            format!("--profile-directory={profile_dir}"),
            url.to_string(),
        ],
    }
}

pub fn linux_profile_launch(
    app: &str,
    profile_dir: &str,
    url: &str,
    find: impl Fn(&str) -> Option<PathBuf>,
) -> Option<Launch> {
    let program = browser_commands(app).iter().find_map(|name| find(name))?;
    Some(Launch {
        program,
        args: vec![
            format!("--profile-directory={profile_dir}"),
            url.to_string(),
        ],
    })
}

#[cfg(target_os = "macos")]
fn platform_profile_launch(app: &str, profile_dir: &str, url: &str) -> Result<Launch, String> {
    Ok(mac_profile_launch(app, profile_dir, url))
}

#[cfg(not(target_os = "macos"))]
fn platform_profile_launch(app: &str, profile_dir: &str, url: &str) -> Result<Launch, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    linux_profile_launch(app, profile_dir, url, |name| find_executable(name, &dirs))
        .ok_or_else(|| format!("could not find {app} to open {url}"))
}

/// Commands to try for `browser_app`, most specific first.
pub fn browser_commands(app: &str) -> Vec<String> {
    let known: &[&str] = match app.trim().to_ascii_lowercase().as_str() {
        "google chrome" | "chrome" => &["google-chrome-stable", "google-chrome"],
        "google chrome beta" => &["google-chrome-beta"],
        "google chrome canary" | "google chrome dev" => &["google-chrome-unstable"],
        "chromium" => &["chromium", "chromium-browser"],
        "brave browser" | "brave" => &["brave", "brave-browser"],
        "microsoft edge" | "edge" => &["microsoft-edge-stable", "microsoft-edge"],
        "firefox" => &["firefox"],
        "vivaldi" => &["vivaldi-stable", "vivaldi"],
        _ => &[],
    };
    if known.is_empty() {
        vec![app.to_string()]
    } else {
        known.iter().map(|name| name.to_string()).collect()
    }
}

#[cfg(not(target_os = "macos"))]
fn find_executable(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_executable = |path: &std::path::Path| {
        path.metadata()
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable(&path).then_some(path);
    }
    dirs.iter()
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

/// Start the browser in its own process group with no inherited stdio, so it
/// outlives WhatsUp and a Ctrl-C in WhatsUp's terminal never reaches it.
fn spawn_detached(launch: &Launch) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let mut child = Command::new(&launch.program)
        .args(&launch.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", launch.program.display()))?;
    // A running browser hands the URL over and exits at once; a fresh one keeps
    // running. Either way, reap it so it never lingers as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn apple_script(app: &str, url: &str, mode: OpenMode) -> String {
    let app = escape_apple(app);
    let url = escape_apple(url);
    match mode {
        OpenMode::Tab => format!(
            r#"tell application "{app}"
  activate
  if (count of windows) = 0 then
    make new window
    set URL of active tab of front window to "{url}"
  else
    tell front window to make new tab with properties {{URL:"{url}"}}
  end if
end tell"#
        ),
        OpenMode::Window => format!(
            r#"tell application "{app}"
  activate
  make new window
  set URL of active tab of front window to "{url}"
end tell"#
        ),
    }
}

fn validate_text(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.starts_with('-')
        || value
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '"' | '\\'))
    {
        return Err(format!("{label} cannot be passed to the browser"));
    }
    Ok(())
}

fn escape_apple(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn tab_script_reuses_chrome_and_window_script_opens_a_window() {
        let tab = apple_script("Google Chrome", "http://localhost:3000", OpenMode::Tab);
        assert!(tab.contains("make new tab with properties {URL:\"http://localhost:3000\"}"));
        let window = apple_script("Google Chrome", "http://localhost:4000", OpenMode::Window);
        assert!(window.contains("make new window"));
        assert!(!window.contains("make new tab"));
    }

    fn installed(names: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |name| {
            names
                .contains(&name)
                .then(|| Path::new("/usr/bin").join(name))
        }
    }

    #[test]
    fn linux_maps_mac_app_names_and_honours_the_open_mode() {
        let find = installed(&["google-chrome-stable", "xdg-open"]);
        let tab = linux_launch(
            "Google Chrome",
            "http://localhost:3000",
            OpenMode::Tab,
            &find,
        );
        assert_eq!(
            tab,
            Some(Launch {
                program: PathBuf::from("/usr/bin/google-chrome-stable"),
                args: vec!["http://localhost:3000".into()],
            })
        );
        let window = linux_launch(
            "Google Chrome",
            "http://localhost:3000",
            OpenMode::Window,
            &find,
        )
        .unwrap();
        assert_eq!(window.args, ["--new-window", "http://localhost:3000"]);
    }

    #[test]
    fn linux_takes_a_command_name_and_falls_back_to_xdg_open() {
        let find = installed(&["chromium", "xdg-open"]);
        let chromium = linux_launch("chromium", "http://a:1", OpenMode::Tab, &find).unwrap();
        assert_eq!(chromium.program, PathBuf::from("/usr/bin/chromium"));

        let fallback =
            linux_launch("Google Chrome", "http://a:1", OpenMode::Window, &find).unwrap();
        assert_eq!(fallback.program, PathBuf::from("/usr/bin/xdg-open"));
        assert_eq!(fallback.args, ["http://a:1"]);

        assert_eq!(
            linux_launch("firefox", "http://a:1", OpenMode::Tab, installed(&[])),
            None
        );
    }

    #[test]
    fn finds_profile_dir_by_display_name() {
        let state = r#"{"profile":{"info_cache":{"Default":{"name":"Jeff"},"Profile 7":{"name":"megan:Lucy"}}}}"#;
        assert_eq!(
            profile_dir_for_name(state, "megan:Lucy"),
            Some("Profile 7".to_string())
        );
        assert_eq!(profile_dir_for_name(state, "megan:lucy"), None);
        assert_eq!(profile_dir_for_name("not json", "megan:Lucy"), None);
    }

    #[test]
    fn mac_profile_launch_hands_the_profile_to_chrome() {
        let launch = mac_profile_launch("Google Chrome", "Profile 7", "http://jds4.local.me:3004");
        assert_eq!(launch.program, PathBuf::from("open"));
        assert_eq!(
            launch.args,
            [
                "-na",
                "Google Chrome",
                "--args",
                "--profile-directory=Profile 7",
                "http://jds4.local.me:3004"
            ]
        );
    }

    #[test]
    fn linux_profile_launch_uses_the_browser_command() {
        let find = installed(&["google-chrome-stable"]);
        let launch =
            linux_profile_launch("Google Chrome", "Profile 7", "http://a:1", &find).unwrap();
        assert_eq!(
            launch.program,
            PathBuf::from("/usr/bin/google-chrome-stable")
        );
        assert_eq!(launch.args, ["--profile-directory=Profile 7", "http://a:1"]);
        assert_eq!(
            linux_profile_launch("Google Chrome", "P", "http://a:1", installed(&[])),
            None
        );
    }

    #[test]
    fn rejects_profile_names_that_could_become_flags() {
        assert!(open_in_profile("Google Chrome", "--x", "http://a:1").is_err());
        assert!(open_in_profile("Google Chrome", "megan:Lucy", "--x").is_err());
    }

    #[test]
    fn rejects_text_that_could_become_a_flag_or_break_a_script() {
        assert!(validate_text("--evil", "browser_app").is_err());
        assert!(validate_text("Google \"Chrome", "browser_app").is_err());
        assert!(validate_text("Google Chrome", "browser_app").is_ok());
    }
}
