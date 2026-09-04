use std::path::{Path, PathBuf};

use thiserror::Error;

/// Result type for autostart operations.
pub type AutostartResult<T> = Result<T, AutostartError>;

/// Autostart errors.
#[derive(Debug, Error)]
pub enum AutostartError {
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Executable path is empty.
    #[error("autostart executable path is empty")]
    EmptyExePath,
    /// Platform has no supported autostart mechanism.
    #[error("autostart is not supported on this platform")]
    Unsupported,
    /// Windows registry operation failed.
    #[error("windows autostart registry failed: {0}")]
    Registry(String),
}

/// Per-OS autostart backend contract.
pub trait AutostartManager {
    /// Application display name (registry value / file stem).
    fn app_name(&self) -> &str;
    /// True when autostart is currently installed.
    fn is_enabled(&self) -> bool;
    /// Installs (`true`) or removes (`false`) autostart.
    ///
    /// # Errors
    ///
    /// Returns an error when the OS mechanism rejects the operation.
    fn set_enabled(&self, enabled: bool) -> AutostartResult<()>;
}

/// Builds the OS-default autostart manager for this platform.
///
/// `exe_path` is the current executable; `args` are appended (e.g.
/// `--minimized`). Headless/unknown platforms get a no-op manager.
#[must_use]
pub fn system_autostart_manager(
    app_name: &str,
    exe_path: &Path,
    args: &[String],
) -> Box<dyn AutostartManager> {
    #[cfg(windows)]
    {
        Box::new(WindowsRunKeyAutostartManager::new(app_name, exe_path, args))
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(LaunchAgentAutostartManager::new(app_name, exe_path, args))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Box::new(XdgAutostartManager::new(app_name, exe_path, args))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (exe_path, args);
        Box::new(NoopAutostartManager::new(app_name))
    }
}

/// No-op fallback for headless runs and unsupported platforms.
#[derive(Debug, Clone)]
pub struct NoopAutostartManager {
    app_name: String,
}

impl NoopAutostartManager {
    /// Creates a no-op manager.
    #[must_use]
    pub fn new(app_name: &str) -> Self {
        Self {
            app_name: app_name.to_owned(),
        }
    }
}

impl AutostartManager for NoopAutostartManager {
    fn app_name(&self) -> &str {
        &self.app_name
    }

    fn is_enabled(&self) -> bool {
        false
    }

    fn set_enabled(&self, _enabled: bool) -> AutostartResult<()> {
        Ok(())
    }
}

/// Linux XDG Autostart backend (`~/.config/autostart/<file>.desktop`).
#[derive(Debug, Clone)]
pub struct XdgAutostartManager {
    app_name: String,
    desktop_file_name: String,
    exec_line: String,
    autostart_dir: PathBuf,
}

impl XdgAutostartManager {
    /// Creates a manager using the default autostart dir.
    #[must_use]
    pub fn new(app_name: &str, exe_path: &Path, args: &[String]) -> Self {
        Self::with_dir(app_name, exe_path, args, default_xdg_autostart_dir())
    }

    /// Creates a manager with an explicit dir (tests inject a temp dir).
    #[must_use]
    pub fn with_dir(
        app_name: &str,
        exe_path: &Path,
        args: &[String],
        autostart_dir: PathBuf,
    ) -> Self {
        let desktop_file_name = format!("{}.desktop", sanitize_file_stem(app_name));
        Self {
            app_name: app_name.to_owned(),
            desktop_file_name,
            exec_line: build_exec_line(exe_path, args),
            autostart_dir,
        }
    }

    fn desktop_path(&self) -> PathBuf {
        self.autostart_dir.join(&self.desktop_file_name)
    }

    fn desktop_entry(&self) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName={}\nExec={}\nTerminal=false\nNoDisplay=true\n",
            self.app_name, self.exec_line
        )
    }
}

impl AutostartManager for XdgAutostartManager {
    fn app_name(&self) -> &str {
        &self.app_name
    }

    fn is_enabled(&self) -> bool {
        self.desktop_path().is_file()
    }

    fn set_enabled(&self, enabled: bool) -> AutostartResult<()> {
        let path = self.desktop_path();
        if enabled {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Write atomically: temp file + rename avoids a half-written
            // entry when the process dies mid-write.
            let tmp = path.with_extension("desktop.tmp");
            std::fs::write(&tmp, self.desktop_entry())?;
            std::fs::rename(&tmp, &path)?;
        } else if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

fn default_xdg_autostart_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        return dir.join("autostart");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.map(|home| home.join(".config/autostart"))
        .unwrap_or_else(|| PathBuf::from(".config/autostart"))
}

/// macOS LaunchAgent backend (`~/Library/LaunchAgents/<name>.plist`).
#[derive(Debug, Clone)]
pub struct LaunchAgentAutostartManager {
    app_name: String,
    program_path: PathBuf,
    args: Vec<String>,
    agents_dir: PathBuf,
}

impl LaunchAgentAutostartManager {
    /// Creates a manager using the default agents dir.
    #[must_use]
    pub fn new(app_name: &str, exe_path: &Path, args: &[String]) -> Self {
        Self::with_dir(app_name, exe_path, args, default_launch_agents_dir())
    }

    /// Creates a manager with an explicit dir (tests inject a temp dir).
    #[must_use]
    pub fn with_dir(app_name: &str, exe_path: &Path, args: &[String], agents_dir: PathBuf) -> Self {
        Self {
            app_name: app_name.to_owned(),
            program_path: exe_path.to_path_buf(),
            args: args.to_vec(),
            agents_dir,
        }
    }

    fn plist_path(&self) -> PathBuf {
        self.agents_dir
            .join(format!("{}.plist", sanitize_file_stem(&self.app_name)))
    }

    fn label(&self) -> String {
        format!("{}.startup", sanitize_file_stem(&self.app_name))
    }

    fn plist_content(&self) -> String {
        let mut out = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n",
        );
        out.push_str(&format!("\t<string>{}</string>\n", self.label()));
        out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
        // Mirror ABDM: open the app bundle via `/usr/bin/open -a <path>`.
        out.push_str("\t\t<string>/usr/bin/open</string>\n\t\t<string>-a</string>\n");
        out.push_str(&format!(
            "\t\t<string>{}</string>\n",
            xml_escape(&self.program_path.to_string_lossy())
        ));
        out.push_str("\t\t<string>--args</string>\n");
        for arg in &self.args {
            out.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(arg)));
        }
        out.push_str(
            "\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>\n</dict>\n</plist>\n",
        );
        out
    }
}

impl AutostartManager for LaunchAgentAutostartManager {
    fn app_name(&self) -> &str {
        &self.app_name
    }

    fn is_enabled(&self) -> bool {
        self.plist_path().is_file()
    }

    fn set_enabled(&self, enabled: bool) -> AutostartResult<()> {
        let path = self.plist_path();
        if enabled {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("plist.tmp");
            std::fs::write(&tmp, self.plist_content())?;
            std::fs::rename(&tmp, &path)?;
            // Best effort: a headless/CI box may lack `launchctl`; the plist
            // file itself is the durable state, so never fail the install.
            let _ = std::process::Command::new("launchctl")
                .args(["load", &path.to_string_lossy()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        } else {
            if path.exists() {
                let _ = std::process::Command::new("launchctl")
                    .args(["unload", &path.to_string_lossy()])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
}

fn default_launch_agents_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.map(|home| home.join("Library/LaunchAgents"))
        .unwrap_or_else(|| PathBuf::from("Library/LaunchAgents"))
}

/// Windows Run-key backend (`HKCU\...\Run`, CurrentUser default).
///
/// ABDM `WindowsStartupDesktop` behavior: per-user key, no elevation.
#[derive(Debug, Clone)]
pub struct WindowsRunKeyAutostartManager {
    app_name: String,
    command: String,
}

impl WindowsRunKeyAutostartManager {
    /// Creates a manager for the per-user Run key.
    #[must_use]
    pub fn new(app_name: &str, exe_path: &Path, args: &[String]) -> Self {
        Self {
            app_name: app_name.to_owned(),
            command: build_exec_line(exe_path, args),
        }
    }

    /// Registry value data that would be written (testable on any OS).
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }
}

#[cfg(windows)]
mod windows_impl {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    use super::*;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    impl AutostartManager for WindowsRunKeyAutostartManager {
        fn app_name(&self) -> &str {
            &self.app_name
        }

        fn is_enabled(&self) -> bool {
            RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey(RUN_KEY)
                .and_then(|key| key.get_value::<String, _>(&self.app_name))
                .is_ok_and(|value| value == self.command)
        }

        fn set_enabled(&self, enabled: bool) -> AutostartResult<()> {
            let root = RegKey::predef(HKEY_CURRENT_USER);
            if enabled {
                let (key, _) = root
                    .create_subkey(RUN_KEY)
                    .map_err(|error| AutostartError::Registry(error.to_string()))?;
                key.set_value(&self.app_name, &self.command)
                    .map_err(|error| AutostartError::Registry(error.to_string()))?;
            } else {
                let key = root
                    .open_subkey(RUN_KEY)
                    .map_err(|error| AutostartError::Registry(error.to_string()))?;
                match key.delete_value(&self.app_name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(AutostartError::Registry(error.to_string())),
                }
            }
            Ok(())
        }
    }
}

#[cfg(not(windows))]
impl AutostartManager for WindowsRunKeyAutostartManager {
    fn app_name(&self) -> &str {
        &self.app_name
    }

    fn is_enabled(&self) -> bool {
        false
    }

    fn set_enabled(&self, _enabled: bool) -> AutostartResult<()> {
        Err(AutostartError::Unsupported)
    }
}

/// Builds the executable + args line with quoting for paths with spaces.
fn build_exec_line(exe_path: &Path, args: &[String]) -> String {
    let exe = exe_path.to_string_lossy();
    let mut parts = vec![quote_arg(&exe)];
    parts.extend(args.iter().map(|arg| quote_arg(arg)));
    parts.join(" ")
}

fn quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_owned();
    }
    if arg.contains([' ', '\t', '"']) {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_owned()
    }
}

fn sanitize_file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|char| {
            if char.is_alphanumeric() || char == '-' || char == '_' || char == '.' {
                char
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim_matches(['-', '.', ' ']);
    if stem.is_empty() {
        "raijin".to_owned()
    } else {
        stem.to_owned()
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_line_should_quote_paths_with_spaces() {
        let line = build_exec_line(
            Path::new("C:/Program Files/Raijin/raijin.exe"),
            &["--minimized".to_owned()],
        );
        assert_eq!(line, "\"C:/Program Files/Raijin/raijin.exe\" --minimized");
    }

    #[test]
    fn xdg_manager_should_round_trip_enable_disable_in_temp_dir() {
        let dir = tempfile::tempdir().expect("temp dir must exist");
        let autostart = dir.path().join("autostart");
        let manager = XdgAutostartManager::with_dir(
            "Raijin",
            Path::new("/usr/bin/raijin"),
            &["--minimized".to_owned()],
            autostart.clone(),
        );

        assert!(!manager.is_enabled());
        manager.set_enabled(true).expect("install must succeed");
        assert!(manager.is_enabled());
        let content = std::fs::read_to_string(autostart.join("Raijin.desktop"))
            .expect("desktop file must exist");
        assert!(content.contains("[Desktop Entry]"));
        assert!(content.contains("Name=Raijin"));
        assert!(content.contains("Exec=/usr/bin/raijin --minimized"));

        manager.set_enabled(false).expect("uninstall must succeed");
        assert!(!manager.is_enabled());
    }

    #[test]
    fn xdg_disable_should_succeed_when_nothing_installed() {
        let dir = tempfile::tempdir().expect("temp dir must exist");
        let manager = XdgAutostartManager::with_dir(
            "Raijin",
            Path::new("/usr/bin/raijin"),
            &[],
            dir.path().to_path_buf(),
        );
        manager
            .set_enabled(false)
            .expect("redundant uninstall must succeed");
    }

    #[test]
    fn launch_agent_should_write_valid_plist_and_remove() {
        let dir = tempfile::tempdir().expect("temp dir must exist");
        let manager = LaunchAgentAutostartManager::with_dir(
            "Raijin",
            Path::new("/Applications/Raijin.app"),
            &["--minimized".to_owned()],
            dir.path().to_path_buf(),
        );

        manager.set_enabled(true).expect("install must succeed");
        assert!(manager.is_enabled());
        let content =
            std::fs::read_to_string(dir.path().join("Raijin.plist")).expect("plist must exist");
        assert!(content.contains("<string>Raijin.startup</string>"));
        assert!(content.contains("<string>/usr/bin/open</string>"));
        assert!(content.contains("--minimized"));
        assert!(content.contains("<true/>"));

        manager.set_enabled(false).expect("uninstall must succeed");
        assert!(!manager.is_enabled());
    }

    #[test]
    fn windows_command_should_quote_exe_and_join_args() {
        let manager = WindowsRunKeyAutostartManager::new(
            "Raijin",
            Path::new("C:/Program Files/Raijin/raijin.exe"),
            &["--minimized".to_owned()],
        );
        assert_eq!(
            manager.command(),
            "\"C:/Program Files/Raijin/raijin.exe\" --minimized"
        );
        assert_eq!(manager.app_name(), "Raijin");
        #[cfg(not(windows))]
        assert!(!manager.is_enabled());
    }

    #[test]
    fn noop_manager_should_stay_disabled_but_accept_writes() {
        let manager = NoopAutostartManager::new("Raijin");
        manager.set_enabled(true).expect("noop must accept writes");
        assert!(!manager.is_enabled());
    }

    #[test]
    fn sanitize_should_replace_unsafe_filename_chars() {
        assert_eq!(sanitize_file_stem("Raijin Dev/Test"), "Raijin-Dev-Test");
        assert_eq!(sanitize_file_stem(""), "raijin");
    }
}
