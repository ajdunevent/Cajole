use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub use crate::APP_ID;

/// Contents of the XDG autostart `.desktop` entry that starts Cajole hidden
/// (scheduling only, no window; see `--hidden` in `main.rs`) at login.
///
/// `exec_path` is an absolute path to the binary, quoted in case it contains
/// spaces.
pub fn desktop_entry_contents(exec_path: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Cajole\n\
         Comment=Gentle wake-up alarms\n\
         Exec=\"{exec_path}\" --hidden\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         Hidden=false\n"
    )
}

pub fn desktop_file_name() -> String {
    format!("{APP_ID}.desktop")
}

/// Whether the autostart entry exists in `autostart_dir` (normally
/// `$XDG_CONFIG_HOME/autostart`).
pub fn is_enabled_at(autostart_dir: &Path) -> bool {
    autostart_dir.join(desktop_file_name()).exists()
}

/// Writes the autostart entry into `autostart_dir`, creating the directory if
/// needed.
pub fn enable_at(autostart_dir: &Path, exec_path: &str) -> io::Result<()> {
    fs::create_dir_all(autostart_dir)?;
    fs::write(
        autostart_dir.join(desktop_file_name()),
        desktop_entry_contents(exec_path),
    )
}

/// Removes the autostart entry if present. No UI calls this yet.
pub fn disable_at(autostart_dir: &Path) -> io::Result<()> {
    let path = autostart_dir.join(desktop_file_name());
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Whether the autostart prompt was dismissed, so it is not shown on every
/// launch.
pub fn prompt_dismissed_at(marker_path: &Path) -> bool {
    marker_path.exists()
}

/// Records that the autostart prompt was dismissed.
pub fn dismiss_prompt_at(marker_path: &Path) -> io::Result<()> {
    if let Some(parent) = marker_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(marker_path, "")
}

/// `$XDG_CONFIG_HOME/autostart`, falling back to `~/.config/autostart`.
pub fn default_autostart_dir() -> PathBuf {
    xdg_config_home().join("autostart")
}

fn xdg_config_home() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".config")
}

/// Whether autostart is on. In a Flatpak the real entry lives on the host
/// and cannot be read from the sandbox, so the entry written to the
/// sandbox's private autostart directory after the portal grants it stands
/// in as the flag. Nothing else reads that file.
pub fn is_enabled() -> bool {
    is_enabled_at(&default_autostart_dir())
}

/// Turns autostart on: a `.desktop` entry, or the Background portal inside a
/// Flatpak.
pub async fn enable() -> anyhow::Result<()> {
    let dir = default_autostart_dir();
    if crate::sandbox::is_flatpak() {
        if !crate::sandbox::request_background(true).await? {
            anyhow::bail!("the portal did not grant autostart");
        }
        enable_at(&dir, "cajole")?;
    } else {
        let exec_path = std::env::current_exe()
            .map_err(|err| anyhow::anyhow!("could not determine executable path: {err}"))?;
        enable_at(&dir, &exec_path.to_string_lossy())?;
    }
    Ok(())
}

/// Turns autostart off, mirroring [`enable`].
pub async fn disable() -> anyhow::Result<()> {
    if crate::sandbox::is_flatpak() {
        crate::sandbox::request_background(false).await?;
    }
    disable_at(&default_autostart_dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cajole-autostart-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn desktop_entry_contains_hidden_flag_and_exec_path() {
        let contents = desktop_entry_contents("/usr/bin/cajole");
        assert!(contents.contains("Exec=\"/usr/bin/cajole\" --hidden"));
        assert!(contents.contains("X-GNOME-Autostart-enabled=true"));
        assert!(contents.contains("Type=Application"));
    }

    #[test]
    fn not_enabled_until_the_file_is_written() {
        let dir = tempdir();
        assert!(!is_enabled_at(&dir));
        enable_at(&dir, "/usr/bin/cajole").unwrap();
        assert!(is_enabled_at(&dir));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enable_creates_missing_autostart_directory() {
        let dir = tempdir().join("does-not-exist-yet").join("autostart");
        assert!(!dir.exists());
        enable_at(&dir, "/usr/bin/cajole").unwrap();
        assert!(is_enabled_at(&dir));
        fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn disable_removes_the_entry_and_is_a_no_op_if_already_absent() {
        let dir = tempdir();
        enable_at(&dir, "/usr/bin/cajole").unwrap();
        assert!(is_enabled_at(&dir));

        disable_at(&dir).unwrap();
        assert!(!is_enabled_at(&dir));

        // A second call with nothing present does not error.
        disable_at(&dir).unwrap();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dismiss_prompt_is_tracked_via_marker_file() {
        let dir = tempdir();
        let marker = dir.join("autostart_prompt_dismissed");
        assert!(!prompt_dismissed_at(&marker));
        dismiss_prompt_at(&marker).unwrap();
        assert!(prompt_dismissed_at(&marker));
        fs::remove_dir_all(&dir).ok();
    }
}
