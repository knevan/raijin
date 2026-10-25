# Download file actions

- A download row's **Open folder** reveals the downloaded file in its containing directory and selects it when supported by the platform. This is the download location, not an application's installation directory.
- If the final file is missing (including downloads not yet completed), open its destination directory without selection. If that directory is missing or inaccessible, return an error; the table context menu shows a notification.
- Directory-only callers (tray and Add Download) use `open_folder`, which opens the directory itself and never falls back to its parent.
- Windows uses separate `/select,` and path arguments. Combining the switch and a path containing spaces into one argument causes Rust's process argument quoting to wrap the switch too; Explorer can then open Documents instead. Canonical paths have verbatim prefixes removed before being passed to Explorer.
- macOS uses `open -R` for existing files. Linux uses `xdg-open` on the containing directory; portable file selection remains deferred.
- Keep these actions in the headless platform layer. Native Windows Shell API integration is unnecessary for this fix and is deferred unless a reproducible Explorer command-line limitation requires it.

Command construction and filesystem fallback have unit coverage. Process creation success is not proof that the file manager displayed or selected the target; verify that behavior with a native smoke test.
