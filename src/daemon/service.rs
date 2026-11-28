use std::path::{Path, PathBuf};

use super::supervision::{ServiceFile, ServiceManager, ServiceState};
use crate::config::VaultConfig;
use crate::error::{Result, VaultError};

/// macOS `launchd` LaunchAgent label.
pub const LAUNCHD_LABEL: &str = "com.citadel.daemon";
/// Linux `systemd --user` service unit filename.
pub const SYSTEMD_SERVICE_NAME: &str = "citadel.service";

/// Generate an Apple `launchd` XML plist running `citadel daemon start --foreground`.
pub fn generate_launchd_plist(bin_path: &Path, data_dir: &Path) -> String {
    let bin_escaped = xml_escape(&bin_path.to_string_lossy());
    let data_dir_escaped = xml_escape(&data_dir.to_string_lossy());
    let log_escaped = xml_escape(&data_dir.join("daemon.log").to_string_lossy());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin_escaped}</string>
        <string>daemon</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>CITADEL_HOME</key>
        <string>{data_dir_escaped}</string>
    </dict>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_escaped}</string>
    <key>StandardErrorPath</key>
    <string>{log_escaped}</string>
</dict>
</plist>
"#
    )
}

/// Generate a Linux `systemd --user` `.service` unit running `citadel daemon start --foreground`.
pub fn generate_systemd_unit(bin_path: &Path, data_dir: &Path) -> String {
    let bin_str = bin_path.to_string_lossy();
    let exec_bin = if bin_str.contains(char::is_whitespace) || bin_str.contains('"') {
        format!("\"{}\"", systemd_escape(&bin_str))
    } else {
        bin_str.into_owned()
    };
    let env_dir = systemd_escape(&data_dir.to_string_lossy());

    format!(
        "[Unit]\n\
         Description=citadel background sync daemon\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_bin} daemon start --foreground\n\
         Environment=\"CITADEL_HOME={env_dir}\"\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Install the OS background service for `config`'s vault and (re)start it on this binary
/// (`launchd` on macOS, `systemd --user` on Linux).
pub fn install_service(config: &VaultConfig) -> Result<PathBuf> {
    config.ensure_data_dir()?;
    let bin_path = std::env::current_exe()?;
    install_service_for_platform(&bin_path, &config.data_dir)
}

/// Stop and remove the OS background service (`launchd` on macOS, `systemd --user` on Linux).
pub fn uninstall_service() -> Result<Option<PathBuf>> {
    uninstall_service_for_platform()
}

/// This platform's service manager, for `Supervision`.
pub fn platform_service_manager() -> Box<dyn ServiceManager> {
    #[cfg(target_os = "macos")]
    {
        match launchd_plist_path() {
            Ok(plist) => Box::new(Launchd { plist }),
            Err(err) => Box::new(NoServiceManager(ServiceState::Unknown {
                reason: err.to_string(),
            })),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match systemd_unit_path() {
            Ok(unit) => Box::new(Systemd { unit }),
            Err(err) => Box::new(NoServiceManager(ServiceState::Unknown {
                reason: err.to_string(),
            })),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Box::new(NoServiceManager(ServiceState::Unsupported))
    }
}

/// No usable service manager: it reports `state` and cannot start or stop anything.
struct NoServiceManager(ServiceState);

impl ServiceManager for NoServiceManager {
    fn inspect(&self) -> ServiceState {
        self.0.clone()
    }

    fn file_data_dir(&self) -> Option<PathBuf> {
        None
    }

    fn start(&self) -> Result<()> {
        Err(VaultError::Other(format!(
            "no OS service manager to start the daemon: {:?}",
            self.0
        )))
    }

    fn stop(&self) -> Result<()> {
        Ok(())
    }
}

/// The `CITADEL_HOME` a service file names; `None` when it is missing or unreadable.
fn file_data_dir(
    path: &Path,
    parse: fn(&str) -> (Option<PathBuf>, Option<PathBuf>),
) -> Option<PathBuf> {
    parse(&std::fs::read_to_string(path).ok()?).1
}

/// Read a service file and parse `(binary, CITADEL_HOME)` from it.
fn inspect_file(
    path: &Path,
    parse: fn(&str) -> (Option<PathBuf>, Option<PathBuf>),
    loaded: impl FnOnce() -> bool,
) -> ServiceState {
    let (text, unreadable) = match std::fs::read_to_string(path) {
        Ok(text) => (text, None),
        Err(_) if !path.exists() => return ServiceState::NotInstalled,
        Err(e) => (String::new(), Some(e.to_string())),
    };
    let (binary, data_dir) = parse(&text);
    ServiceState::Installed {
        file: ServiceFile {
            path: path.to_path_buf(),
            binary,
            data_dir,
            unreadable,
        },
        loaded: loaded(),
    }
}

/// How long `launchctl bootstrap` is retried while launchd finishes unloading the job.
const LAUNCHD_RELOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The macOS LaunchAgent `com.citadel.daemon`, defined by `plist`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Launchd {
    plist: PathBuf,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Launchd {
    /// Load the job again rather than `launchctl kickstart` it: after the binary is replaced,
    /// launchd can refuse to spawn the loaded job (`EX_CONFIG`, "needs LWCR update"), and
    /// `kickstart` then exits 0 without starting it. Bootstrapping starts it (`RunAtLoad`).
    /// The legacy `load` is not used: it exits 0 even when it prints "Load failed".
    fn reload(&self) -> Result<()> {
        let [bootout, enable, bootstrap] = launchd_reload_args(current_uid()?, &self.plist);
        // Fails when the job is not loaded, which is fine.
        let _ = run_service_command("launchctl", &as_strs(&bootout));
        // `load -w` cleared a disabled override; `bootstrap` refuses a disabled job.
        run_service_command("launchctl", &as_strs(&enable))?;
        // `bootout` returns while launchd may still be stopping a running instance (up to its
        // exit timeout); `bootstrap` fails (5: Input/output error) until the job is gone.
        retry_for(
            LAUNCHD_RELOAD_TIMEOUT,
            std::time::Duration::from_millis(250),
            || run_service_command("launchctl", &as_strs(&bootstrap)),
        )
    }

    fn loaded() -> bool {
        std::process::Command::new("launchctl")
            .args(["list", LAUNCHD_LABEL])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

impl ServiceManager for Launchd {
    fn inspect(&self) -> ServiceState {
        inspect_file(&self.plist, parse_launchd_plist, Self::loaded)
    }

    fn file_data_dir(&self) -> Option<PathBuf> {
        file_data_dir(&self.plist, parse_launchd_plist)
    }

    fn start(&self) -> Result<()> {
        self.reload()
    }

    /// `bootout` unloads the job, so `KeepAlive` does not relaunch the daemon. launchd loads
    /// it again at the next login, or with `start`.
    fn stop(&self) -> Result<()> {
        let [bootout, _, _] = launchd_reload_args(current_uid()?, &self.plist);
        match run_service_command("launchctl", &as_strs(&bootout)) {
            Err(_) if !Self::loaded() => Ok(()),
            result => result,
        }
    }
}

/// The Linux `systemd --user` unit `citadel.service`, defined by `unit`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Systemd {
    unit: PathBuf,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Systemd {
    /// Running under systemd: `active`. Whether this vault's daemon is supervised does not
    /// depend on it; it is what doctor reports.
    fn loaded() -> bool {
        std::process::Command::new("systemctl")
            .args(SYSTEMD_LOADED_QUERY)
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
    }
}

impl ServiceManager for Systemd {
    fn inspect(&self) -> ServiceState {
        inspect_file(&self.unit, parse_systemd_unit, Self::loaded)
    }

    fn file_data_dir(&self) -> Option<PathBuf> {
        file_data_dir(&self.unit, parse_systemd_unit)
    }

    fn start(&self) -> Result<()> {
        run_service_command("systemctl", &systemd_args("start"))
    }

    fn stop(&self) -> Result<()> {
        run_service_command("systemctl", &systemd_args("stop"))
    }
}

/// Asks systemd whether the unit runs: `citadel daemon stop` leaves it enabled but inactive,
/// as it leaves a launchd job booted out.
const SYSTEMD_LOADED_QUERY: [&str; 3] = ["--user", "is-active", SYSTEMD_SERVICE_NAME];

/// `systemctl --user <verb> citadel.service`.
fn systemd_args(verb: &str) -> [&str; 3] {
    ["--user", verb, SYSTEMD_SERVICE_NAME]
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launchd_plist_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().ok_or_else(|| {
        VaultError::Config("could not determine user home directory for LaunchAgents".into())
    })?;
    Ok(base
        .home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn systemd_unit_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().ok_or_else(|| {
        VaultError::Config("could not determine user config directory for systemd".into())
    })?;
    Ok(base
        .config_dir()
        .join("systemd")
        .join("user")
        .join(SYSTEMD_SERVICE_NAME))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Launchd {
    fn install(bin_path: &Path, data_dir: &Path) -> Result<PathBuf> {
        let plist = launchd_plist_path()?;
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&plist, generate_launchd_plist(bin_path, data_dir))?;
        Launchd {
            plist: plist.clone(),
        }
        .reload()?;
        Ok(plist)
    }

    fn uninstall() -> Result<Option<PathBuf>> {
        let plist = launchd_plist_path()?;
        if !plist.exists() {
            return Ok(None);
        }
        let _ = Launchd {
            plist: plist.clone(),
        }
        .stop();
        std::fs::remove_file(&plist)?;
        Ok(Some(plist))
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Systemd {
    fn install(bin_path: &Path, data_dir: &Path) -> Result<PathBuf> {
        let unit_path = systemd_unit_path()?;
        if let Some(parent) = unit_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&unit_path, generate_systemd_unit(bin_path, data_dir))?;
        run_service_command("systemctl", &["--user", "daemon-reload"])?;
        run_service_command("systemctl", &systemd_args("enable"))?;
        // `restart` starts a stopped unit and replaces a running one, so a changed unit or
        // binary takes effect (`enable --now` leaves a running unit alone).
        run_service_command("systemctl", &systemd_args("restart"))?;
        Ok(unit_path)
    }

    fn uninstall() -> Result<Option<PathBuf>> {
        let unit_path = systemd_unit_path()?;
        if !unit_path.exists() {
            return Ok(None);
        }
        let _ = run_service_command(
            "systemctl",
            &["--user", "disable", "--now", SYSTEMD_SERVICE_NAME],
        );
        std::fs::remove_file(&unit_path)?;
        let _ = run_service_command("systemctl", &["--user", "daemon-reload"]);
        Ok(Some(unit_path))
    }
}

#[cfg(target_os = "macos")]
fn install_service_for_platform(bin_path: &Path, data_dir: &Path) -> Result<PathBuf> {
    Launchd::install(bin_path, data_dir)
}

#[cfg(target_os = "macos")]
fn uninstall_service_for_platform() -> Result<Option<PathBuf>> {
    Launchd::uninstall()
}

#[cfg(target_os = "linux")]
fn install_service_for_platform(bin_path: &Path, data_dir: &Path) -> Result<PathBuf> {
    Systemd::install(bin_path, data_dir)
}

#[cfg(target_os = "linux")]
fn uninstall_service_for_platform() -> Result<Option<PathBuf>> {
    Systemd::uninstall()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install_service_for_platform(_bin_path: &Path, _data_dir: &Path) -> Result<PathBuf> {
    Err(VaultError::Other(
        "OS service install is only supported on macOS (launchd) and Linux (systemd --user)".into(),
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn uninstall_service_for_platform() -> Result<Option<PathBuf>> {
    Err(VaultError::Other(
        "OS service uninstall is only supported on macOS (launchd) and Linux (systemd --user)"
            .into(),
    ))
}

/// Run a service manager command, failing with its stderr when it cannot run or exits non-zero.
fn run_service_command(program: &str, args: &[&str]) -> Result<()> {
    let command = format!("{program} {}", args.join(" "));
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|err| VaultError::Other(format!("could not run `{command}`: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(VaultError::Other(format!(
            "`{command}` failed (exit status {}): {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn as_strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// The current user's id, from `id -u`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn current_uid() -> Result<u32> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|err| VaultError::Other(format!("could not run `id -u`: {err}")))?;
    let text = String::from_utf8_lossy(&output.stdout);
    match text.trim().parse() {
        Ok(uid) if output.status.success() => Ok(uid),
        _ => Err(VaultError::Other(format!(
            "could not read the user id from `id -u` (exit status {}, output {:?})",
            output.status,
            text.trim()
        ))),
    }
}

/// Run `attempt` until it succeeds, pausing `pause` between tries; after `timeout`, return
/// its last error.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn retry_for(
    timeout: std::time::Duration,
    pause: std::time::Duration,
    mut attempt: impl FnMut() -> Result<()>,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(err) if std::time::Instant::now() >= deadline => return Err(err),
            Err(_) => std::thread::sleep(pause),
        }
    }
}

/// `launchctl` arguments that unload the citadel job of user `uid`, clear a disabled
/// override, then load it from `plist`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launchd_reload_args(uid: u32, plist: &Path) -> [Vec<String>; 3] {
    let target = format!("gui/{uid}/{LAUNCHD_LABEL}");
    [
        vec!["bootout".to_string(), target.clone()],
        vec!["enable".to_string(), target],
        vec![
            "bootstrap".to_string(),
            format!("gui/{uid}"),
            plist.to_string_lossy().into_owned(),
        ],
    ]
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn systemd_escape(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

fn xml_unescape(input: &str) -> String {
    input
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The `<string>` value that follows `<key>{key}</key>`, XML-unescaped.
fn plist_string_after(text: &str, key: &str) -> Option<String> {
    let marker = format!("<key>{key}</key>");
    let rest = &text[text.find(&marker)? + marker.len()..];
    let rest = &rest[rest.find("<string>")? + "<string>".len()..];
    let value = &rest[..rest.find("</string>")?];
    Some(xml_unescape(value))
}

/// Extract `(binary, CITADEL_HOME)` from a launchd plist.
pub fn parse_launchd_plist(text: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    (
        plist_string_after(text, "ProgramArguments").map(PathBuf::from),
        plist_string_after(text, "CITADEL_HOME").map(PathBuf::from),
    )
}

fn systemd_unescape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// First token of an `ExecStart=` value; supports the double-quoted form.
fn first_exec_token(value: &str) -> String {
    let value = value.trim();
    let Some(quoted) = value.strip_prefix('"') else {
        return value.split_whitespace().next().unwrap_or("").to_string();
    };
    let mut raw = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => {
                raw.push(c);
                if let Some(next) = chars.next() {
                    raw.push(next);
                }
            }
            _ => raw.push(c),
        }
    }
    systemd_unescape(&raw)
}

/// Extract `(binary, CITADEL_HOME)` from a systemd user unit.
pub fn parse_systemd_unit(text: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut binary = None;
    let mut data_dir = None;
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("ExecStart=") {
            let token = first_exec_token(value);
            if !token.is_empty() {
                binary = Some(PathBuf::from(token));
            }
        } else if let Some(value) = line.strip_prefix("Environment=\"CITADEL_HOME=") {
            if let Some(value) = value.strip_suffix('"') {
                data_dir = Some(PathBuf::from(systemd_unescape(value)));
            }
        }
    }
    (binary, data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn generates_valid_launchd_plist_and_systemd_unit() {
        let bin = Path::new("/usr/local/bin/citadel");
        let data_dir = Path::new("/Users/alice/Library/Application Support/citadel");

        let plist = generate_launchd_plist(bin, data_dir);
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(plist.contains("<plist version=\"1.0\">"));
        assert!(plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
        assert!(plist.contains("<string>/usr/local/bin/citadel</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<string>start</string>"));
        assert!(plist.contains("<string>--foreground</string>"));
        assert!(plist.contains("<key>CITADEL_HOME</key>"));
        assert!(plist.contains(&format!("<string>{}</string>", data_dir.display())));
        assert!(plist.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("daemon.log"));

        let unit = generate_systemd_unit(bin, data_dir);
        assert!(unit.contains("[Unit]"));
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("ExecStart=/usr/local/bin/citadel daemon start --foreground"));
        assert!(unit.contains("CITADEL_HOME=/Users/alice/Library/Application Support/citadel"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("[Install]"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_escapes_xml_special_characters() {
        let bin = Path::new("/opt/a&b/<vault>/citadel");
        let data_dir = Path::new("/tmp/data&'dir\"");

        let plist = generate_launchd_plist(bin, data_dir);
        assert!(plist.contains("/opt/a&amp;b/&lt;vault&gt;/citadel"));
        assert!(plist.contains("/tmp/data&amp;&apos;dir&quot;"));
    }

    fn p(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    #[test]
    fn plist_round_trips_through_generator() {
        let text = generate_launchd_plist(Path::new("/a & b/citadel"), Path::new("/data"));
        assert_eq!(
            parse_launchd_plist(&text),
            (p("/a & b/citadel"), p("/data"))
        );
    }

    #[test]
    fn unit_round_trips_through_generator() {
        let text = generate_systemd_unit(Path::new("/opt/my citadel/citadel"), Path::new("/data"));
        assert_eq!(
            parse_systemd_unit(&text),
            (p("/opt/my citadel/citadel"), p("/data"))
        );
    }

    #[test]
    fn unit_unquoted_binary_parses() {
        let text = generate_systemd_unit(Path::new("/usr/bin/citadel"), Path::new("/data"));
        assert_eq!(
            parse_systemd_unit(&text),
            (p("/usr/bin/citadel"), p("/data"))
        );
    }

    #[test]
    fn inspect_reads_a_missing_file_as_not_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("com.citadel.daemon.plist");
        let state = inspect_file(&missing, parse_launchd_plist, || unreachable!());
        assert_eq!(state, ServiceState::NotInstalled);

        std::fs::write(
            &missing,
            generate_launchd_plist(Path::new("/bin/citadel"), dir.path()),
        )
        .expect("write plist");
        let ServiceState::Installed { file, loaded } =
            inspect_file(&missing, parse_launchd_plist, || true)
        else {
            panic!("installed");
        };
        assert!(loaded);
        assert_eq!(file.data_dir.as_deref(), Some(dir.path()));
        assert_eq!(
            file_data_dir(&missing, parse_launchd_plist).as_deref(),
            Some(dir.path())
        );
    }

    #[test]
    fn systemd_loaded_means_active() {
        // `citadel daemon stop` leaves the unit enabled but inactive; doctor must then show
        // it as not loaded, as it does for a booted-out launchd job.
        assert_eq!(
            SYSTEMD_LOADED_QUERY,
            ["--user", "is-active", "citadel.service"]
        );
    }

    #[test]
    fn systemd_commands_name_the_user_unit() {
        assert_eq!(systemd_args("stop"), ["--user", "stop", "citadel.service"]);
    }

    #[test]
    fn retry_for_retries_until_the_attempt_succeeds() {
        // `launchctl bootstrap` fails (5: Input/output error) while launchd still tears down
        // the job that `bootout` just stopped.
        let mut attempts = 0;
        retry_for(Duration::from_secs(5), Duration::from_millis(1), || {
            attempts += 1;
            if attempts < 3 {
                Err(VaultError::Other(format!("attempt {attempts} failed")))
            } else {
                Ok(())
            }
        })
        .expect("the third attempt succeeds");
        assert_eq!(attempts, 3);
    }

    #[test]
    fn retry_for_returns_the_last_error_once_the_time_is_up() {
        let mut attempts = 0;
        let err = retry_for(Duration::from_millis(30), Duration::from_millis(5), || {
            attempts += 1;
            Err(VaultError::Other(format!("attempt {attempts} failed")))
        })
        .expect_err("every attempt fails");
        assert!(attempts > 1, "retried before giving up");
        assert_eq!(err.to_string(), format!("attempt {attempts} failed"));
    }

    #[test]
    fn launchd_start_reloads_the_existing_plist() {
        // After `citadel update` swaps the binary, launchd can refuse to spawn the loaded job
        // (`EX_CONFIG`, "needs LWCR update") and `kickstart` exits 0 without starting it.
        // Loading the job again starts the new binary (`RunAtLoad`).
        let plist = Path::new("/Users/alice/Library/LaunchAgents/com.citadel.daemon.plist");
        let [bootout, enable, bootstrap] = launchd_reload_args(501, plist);
        assert_eq!(bootout, ["bootout", "gui/501/com.citadel.daemon"]);
        assert_eq!(enable, ["enable", "gui/501/com.citadel.daemon"]);
        assert_eq!(
            bootstrap,
            [
                "bootstrap",
                "gui/501",
                "/Users/alice/Library/LaunchAgents/com.citadel.daemon.plist"
            ]
        );
    }
}
