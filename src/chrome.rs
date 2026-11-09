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
    validate_apple_text(app, "browser_app")?;
    validate_apple_text(url, "url")?;
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

fn validate_apple_text(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '"' | '\\'))
    {
        return Err(format!("{label} cannot be passed to Chrome"));
    }
    Ok(())
}

fn escape_apple(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_script_reuses_chrome_and_window_script_opens_a_window() {
        let tab = apple_script("Google Chrome", "http://localhost:3000", OpenMode::Tab);
        assert!(tab.contains("make new tab with properties {URL:\"http://localhost:3000\"}"));
        let window = apple_script("Google Chrome", "http://localhost:4000", OpenMode::Window);
        assert!(window.contains("make new window"));
        assert!(!window.contains("make new tab"));
    }
}
