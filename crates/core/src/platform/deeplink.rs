use std::io;
use std::path::Path;

/// The only supported external navigation destination.
pub const SETTINGS_URI: &str = "raijin://settings/browser-integration";

/// Explicit protocol commands, handled before engine startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolAction {
    Register,
    Unregister,
    OpenSettings,
}

/// Parses protocol arguments without accepting extra flags or URI parameters.
pub fn parse_protocol_args(args: &[String]) -> io::Result<Option<ProtocolAction>> {
    let protocol_arg = args.iter().skip(1).any(|arg| {
        matches!(
            arg.as_str(),
            "--register-protocol" | "--unregister-protocol" | "--open-url"
        ) || arg.to_ascii_lowercase().starts_with("raijin:")
    });
    if !protocol_arg {
        return Ok(None);
    }
    match args.get(1).map(String::as_str) {
        Some("--register-protocol") if args.len() == 2 => Ok(Some(ProtocolAction::Register)),
        Some("--unregister-protocol") if args.len() == 2 => Ok(Some(ProtocolAction::Unregister)),
        Some("--open-url") if args.len() == 3 && args[2] == SETTINGS_URI => {
            Ok(Some(ProtocolAction::OpenSettings))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported protocol command or destination",
        )),
    }
}

/// Registers or removes the current user's Windows protocol handler.
pub fn set_registered(executable: &Path, enabled: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        if enabled && super::autostart::is_build_output_path(executable) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy Raijin out of the Cargo target directory before registering the protocol handler",
            ));
        }
        set_registered_at(
            &winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER),
            r"Software\Classes\raijin",
            executable,
            enabled,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (executable, enabled);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "app links are currently supported only on Windows",
        ))
    }
}

#[cfg(windows)]
fn handler_command(executable: &Path) -> io::Result<String> {
    let path = executable
        .to_str()
        .filter(|path| {
            executable.is_absolute() && !path.chars().any(|c| c == '"' || c.is_control())
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "protocol handler requires an absolute UTF-8 executable path",
            )
        })?;
    Ok(format!("\"{path}\" --open-url \"%1\""))
}

#[cfg(windows)]
fn set_registered_at(
    root: &winreg::RegKey,
    key_path: &str,
    executable: &Path,
    enabled: bool,
) -> io::Result<()> {
    use winreg::enums::KEY_READ;

    if enabled {
        let command = handler_command(executable)?;
        let (key, _) = root.create_subkey(key_path)?;
        key.set_value("", &"URL:Raijin Protocol")?;
        key.set_value("URL Protocol", &"")?;
        let (command_key, _) = key.create_subkey(r"shell\open\command")?;
        command_key.set_value("", &command)
    } else {
        let command_key = match root
            .open_subkey_with_flags(format!(r"{key_path}\shell\open\command"), KEY_READ)
        {
            Ok(key) => key,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let registered: String = command_key.get_value("")?;
        if registered != handler_command(executable)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "protocol handler belongs to another executable; refusing to remove it",
            ));
        }
        drop(command_key);
        root.delete_subkey_all(key_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        std::iter::once("raijin")
            .chain(values.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn protocol_args_should_accept_only_explicit_commands() {
        assert_eq!(
            parse_protocol_args(&args(&["--open-url", SETTINGS_URI])).unwrap(),
            Some(ProtocolAction::OpenSettings)
        );
        assert_eq!(
            parse_protocol_args(&args(&["--register-protocol"])).unwrap(),
            Some(ProtocolAction::Register)
        );
        assert_eq!(
            parse_protocol_args(&args(&["--unregister-protocol"])).unwrap(),
            Some(ProtocolAction::Unregister)
        );
        assert_eq!(parse_protocol_args(&args(&["--startup"])).unwrap(), None);
        assert_eq!(
            parse_protocol_args(&args(&["--add", "https://example.com/a"])).unwrap(),
            None
        );
    }

    #[test]
    fn protocol_args_should_reject_parameters_and_flag_injection() {
        for values in [
            vec!["--open-url"],
            vec![
                "--open-url",
                "raijin://settings/browser-integration?token=secret",
            ],
            vec!["--open-url", "raijin://settings/browser-integration#token"],
            vec!["--open-url", "raijin://settings/general"],
            vec!["--open-url", SETTINGS_URI, "--add", "https://example.com"],
            vec!["--register-protocol", "--startup"],
            vec![SETTINGS_URI],
        ] {
            assert!(parse_protocol_args(&args(&values)).is_err(), "{values:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn registration_should_quote_paths_and_preserve_other_installations() {
        let root = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        let key_path = format!(r"Software\RaijinProtocolTest\{}", std::process::id());
        struct Cleanup<'a>(&'a winreg::RegKey, &'a str);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.delete_subkey_all(self.1);
            }
        }
        let _cleanup = Cleanup(&root, &key_path);
        let exe = Path::new(r"C:\Program Files\Raijin\raijin.exe");
        set_registered_at(&root, &key_path, exe, true).unwrap();
        let key = root.open_subkey(&key_path).unwrap();
        assert_eq!(key.get_value::<String, _>("URL Protocol").unwrap(), "");
        let command = key
            .open_subkey(r"shell\open\command")
            .unwrap()
            .get_value::<String, _>("")
            .unwrap();
        assert_eq!(
            command,
            r#""C:\Program Files\Raijin\raijin.exe" --open-url "%1""#
        );
        assert!(
            set_registered_at(&root, &key_path, Path::new(r"C:\Other\raijin.exe"), false).is_err()
        );
        set_registered_at(&root, &key_path, exe, false).unwrap();
        set_registered_at(&root, &key_path, exe, false).unwrap();
        assert!(root.open_subkey(&key_path).is_err());
        assert!(handler_command(Path::new("relative.exe")).is_err());
    }
}
