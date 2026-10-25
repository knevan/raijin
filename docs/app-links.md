# Windows App Links and Single Instance

Status: implemented for Windows; browser/desktop end-to-end smoke testing remains required.

## Scope and decisions

The extension's Windows-only **Open Raijin Settings** link navigates to
`raijin://settings/browser-integration`. It does not provision credentials.
The user still copies the masked token from Settings and pastes it into the
extension. Links never reveal, copy, regenerate, or transport the token.

Windows was selected first because the application has no installer/updater
or macOS application bundle yet. macOS/Linux protocol registration, URL-open
events, packaging, and single-instance forwarding remain deferred.

Only this exact URI is accepted, through `--open-url <uri>`. Extra arguments,
queries, fragments, alternative destinations, and bare URI arguments are
rejected before engine startup. The scheme is public: any website may request
navigation, so it is not an authentication mechanism. Repeated requests reuse
the same Settings window and explicitly select Browser Integration, including
when the existing window currently shows General. No generic routing framework
or new local HTTP endpoint is needed.

## Registration without an installer

Copy the built executable to a stable installation folder before registration.
Registration is explicit and per-user (`HKCU\Software\Classes\raijin`); it
requires no administrator privileges and never runs automatically at startup
or during `cargo run`. Like autostart, registration rejects paths containing a
`target` component. That heuristic also rejects a legitimate folder named
`target` and cannot identify custom Cargo output directories.

From PowerShell, using the executable's actual installed path:

```powershell
$exe = 'C:\Apps\Raijin\raijin.exe'
$p = Start-Process -FilePath $exe -ArgumentList '--register-protocol' -Wait -PassThru
$p.ExitCode # 0 means success
Start-Process 'raijin://settings/browser-integration'
```

To remove the handler before moving/deleting the executable:

```powershell
$p = Start-Process -FilePath $exe -ArgumentList '--unregister-protocol' -Wait -PassThru
$p.ExitCode
```

Both commands exit before engine/database startup. Errors are written through
the existing fatal startup logging path. The registry command quotes the
executable and URI separately. Explicit registration replaces the current
per-user handler; unregistration refuses to remove a handler owned by a
different executable and is idempotent when the handler is absent.

Moving or deleting the executable leaves a stale registration; unregister
before moving and register again at the new location. An eventual installer
can call these same commands. No installer or updater is introduced here.

## Launch lifecycle and IPC boundary

On Windows, every normal launch claims an exclusive named pipe before engine
or database bootstrap. The pipe name uses a deterministic hash of the
canonical application data directory, so separate data directories remain
independent. No lock file or durable sidecar is written.

- First app-link launch boots the engine and opens Settings only, not the main
  downloads window. Browser integration need not be running.
- A later launch forwards its request and exits without bootstrapping another
  engine/database writer. This also works with the primary instance in the tray.
- Ordinary relaunch activates existing windows or opens the main window when
  none exist. Duplicate `--startup` / `--minimized` launches stay in the background.
- Existing `--add` launches forward the validated HTTP(S) URL to the same
  confirmation form as the standalone CLI path; they never start downloads
  without user confirmation. This remains a local same-user CLI capability,
  not an external app-link route.

The pipe rejects remote clients and uses the Windows default pipe security
descriptor. Its read/write client requires the creator's permissions (or a
privileged local principal); the name hash is namespacing, not authentication.
This replaces the previously proposed loopback forwarding endpoint with local
OS-controlled IPC. It does not expose a network port, credentials, destination
paths, engine commands, or silent downloads. As before, software running as
the user is not sandboxed by Raijin.

Requests are size-limited, validated, and queued in a bounded channel. Pipe I/O
has a timeout, and busy forwarding has a finite deadline. Acceptance means
queued, not proof that the window has rendered. The server prepares its next
pipe instance before releasing the previous one, preserving exclusive ownership
without reusing disconnected Tokio pipe state. An unrecoverable listener error
keeps ownership until exit rather than permitting a duplicate engine. Runtime
shutdown/instance drop cancels the worker and releases the pipe.

The GPUI foreground dispatcher in `crates/ui/src/bridge.rs` drains launch
commands; Tokio workers never mutate entities or open windows.

## Browser behavior and fallback

The options page uses a direct user-clicked link, without new extension
permissions. Browser confirmation is expected. After clicking, a neutral hint
explains how to install/register or open Settings manually. The extension does
not infer installation from a timeout, focus change, or local API health check,
and never claims that Settings opened or that Raijin is missing.

## Verification

Automated coverage exercises strict argument validation, registry command
quoting and isolated-key registration/unregistration, exclusive pipe ownership,
sequential forwarding, malformed/oversized request recovery, and queue limits.
Registry tests never modify the real `raijin` protocol handler.

Before release, smoke test Chromium and Firefox on Windows:

1. No handler: click the extension link; fallback stays usable without a false
   success or an unsupported "not installed" assertion.
2. Register from a stable folder containing spaces; verify OS/browser launch.
3. App stopped: only Settings opens, with the token still masked.
4. App in tray: Settings opens on the same process, without another tray icon.
5. Settings on General: app link selects Browser Integration in that window.
6. Repeat clicks, including while starting: no duplicate Settings windows or
   engine processes. Cancel browser confirmation and verify manual fallback.
7. Disable browser integration: app links still work.
8. Exercise ordinary, `--startup`, `--minimized`, and `--add` relaunches.
9. Unregister twice; verify no handler remains. Verify a different executable
   cannot unregister the active installation's handler.
