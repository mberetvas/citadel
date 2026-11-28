//! Running checks: daemon state and the OS background service.

use std::path::Path;

use super::report::{Check, Section};
use crate::daemon::supervision::ServiceState;
use crate::daemon::DaemonStatus;

/// Map the daemon pid-file status to a check row.
pub fn daemon_running_check(status: DaemonStatus, pid_path: &Path) -> Check {
    match status {
        DaemonStatus::Running(pid) => Check::ok(
            Section::Running,
            "daemon",
            format!("daemon running (pid {pid})"),
        ),
        DaemonStatus::Stopped => Check::info(Section::Running, "daemon", "daemon not running")
            .with_hint("citadel daemon start"),
        DaemonStatus::Stale => Check::warn(
            Section::Running,
            "daemon",
            format!(
                "stale daemon.pid at {}; no daemon holds it",
                pid_path.display()
            ),
        )
        .with_hint("citadel daemon start"),
    }
}

/// Turn the gathered service state into check rows.
pub fn service_checks(state: &ServiceState, cli_data_dir: &Path, cli_exe: &Path) -> Vec<Check> {
    const NAME: &str = "service";
    match state {
        ServiceState::Unsupported => vec![Check::info(
            Section::Running,
            NAME,
            "not supported on this platform",
        )],
        ServiceState::NotInstalled => {
            vec![
                Check::info(Section::Running, NAME, "background service not installed")
                    .with_hint("citadel daemon install"),
            ]
        }
        ServiceState::Unknown { reason } => vec![Check::warn(
            Section::Running,
            NAME,
            format!("cannot locate the service file: {reason}"),
        )
        .with_hint("set HOME, then re-run citadel doctor")],
        ServiceState::Installed { file, loaded } => {
            let mut checks = Vec::new();
            if *loaded {
                checks.push(Check::ok(
                    Section::Running,
                    NAME,
                    format!("installed and loaded ({})", file.path.display()),
                ));
            } else {
                // `citadel daemon stop` unloads the service; `start` loads it again.
                checks.push(
                    Check::info(
                        Section::Running,
                        NAME,
                        format!(
                            "service file present but not loaded ({})",
                            file.path.display()
                        ),
                    )
                    .with_hint("citadel daemon start"),
                );
            }
            if let Some(reason) = &file.unreadable {
                checks.push(
                    Check::warn(
                        Section::Running,
                        "service file",
                        format!(
                            "service file unreadable ({}): {reason}",
                            file.path.display()
                        ),
                    )
                    .with_hint("fix the file permissions or run citadel daemon install"),
                );
            }
            if let Some(dir) = &file.data_dir {
                if !crate::fsutil::same_path(dir, cli_data_dir) {
                    checks.push(
                        Check::warn(
                            Section::Running,
                            "service data dir",
                            format!(
                                "service uses data dir {}; this CLI uses {}",
                                dir.display(),
                                cli_data_dir.display()
                            ),
                        )
                        .with_hint("citadel daemon install"),
                    );
                }
            }
            if let Some(bin) = &file.binary {
                if !crate::fsutil::same_path(bin, cli_exe) {
                    checks.push(
                        Check::warn(
                            Section::Running,
                            "service binary",
                            format!(
                                "service runs {}; this CLI is {}",
                                bin.display(),
                                cli_exe.display()
                            ),
                        )
                        .with_hint("citadel daemon install"),
                    );
                }
            }
            checks
        }
    }
}

/// Kind of a running citadel process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    Daemon,
    Serve,
}

/// A running `citadel daemon start` or `citadel serve` process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CitadelProcess {
    pub pid: u32,
    pub kind: ProcessKind,
}

/// Parse `ps -axo pid=,command=` output into citadel daemon/serve processes.
pub fn parse_ps_output(text: &str) -> Vec<CitadelProcess> {
    text.lines().filter_map(parse_ps_line).collect()
}

fn parse_ps_line(line: &str) -> Option<CitadelProcess> {
    let mut tokens = line.split_whitespace();
    let pid = tokens.next()?.parse::<u32>().ok()?;
    let argv0 = tokens.next()?;
    if Path::new(argv0).file_name()? != "citadel" {
        return None;
    }
    let args: Vec<&str> = tokens.collect();
    let kind = if args.windows(2).any(|w| w == ["daemon", "start"]) {
        ProcessKind::Daemon
    } else if args.contains(&"serve") {
        ProcessKind::Serve
    } else {
        return None;
    };
    Some(CitadelProcess { pid, kind })
}

/// Run `ps` and parse it; any failure yields an empty list.
pub fn gather_processes() -> Vec<CitadelProcess> {
    std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| parse_ps_output(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
}

/// Summarize running citadel processes.
pub fn process_check(procs: &[CitadelProcess]) -> Check {
    let count = |kind| procs.iter().filter(|p| p.kind == kind).count();
    let (daemons, serves) = (count(ProcessKind::Daemon), count(ProcessKind::Serve));
    let plural = |n: usize| if n == 1 { "" } else { "es" };
    let summary = format!(
        "{daemons} daemon process{}, {serves} serve process{}",
        plural(daemons),
        plural(serves)
    );
    if daemons > 1 {
        Check::warn(
            Section::Running,
            "processes",
            format!("{summary}; may be different data dirs"),
        )
        .with_hint("check each daemon's data dir (CITADEL_HOME) and stop the extra one")
    } else if daemons >= 1 && serves >= 1 {
        Check::warn(
            Section::Running,
            "processes",
            format!("{summary}; if they share a vault, both run scheduled Syncs"),
        )
        .with_hint("stop one of them (citadel daemon stop, or stop the serve process)")
    } else {
        Check::ok(Section::Running, "processes", summary)
    }
}

/// Result of probing the serve MCP endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeProbe {
    NotAnswering,
    Answering { version: Option<String> },
}

/// POST an MCP `initialize` to `url` and read the server version.
pub async fn probe_serve(url: &str) -> ServeProbe {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "citadel-doctor",
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "citadel-doctor", "version": env!("CARGO_PKG_VERSION") },
        },
    });
    match crate::setup::mcp_post(url, body.to_string(), std::time::Duration::from_secs(1)).await {
        Some(reply) => ServeProbe::Answering {
            version: reply
                .pointer("/result/serverInfo/version")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        },
        None => ServeProbe::NotAnswering,
    }
}

/// Map a serve probe to a check row.
pub fn serve_check(url: &str, probe: &ServeProbe, http_entry_points_here: bool) -> Check {
    match probe {
        ServeProbe::Answering { .. } => Check::ok(
            Section::Running,
            "serve",
            format!("serve answering at {url}"),
        ),
        ServeProbe::NotAnswering if http_entry_points_here => Check::warn(
            Section::Running,
            "serve",
            format!("nothing answering at {url}, but an MCP entry points there"),
        )
        .with_hint(
            "start it with make run or citadel serve, or re-run citadel setup --transport stdio",
        ),
        ServeProbe::NotAnswering => Check::info(
            Section::Running,
            "serve",
            format!("serve not running at {url}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::supervision::ServiceFile;
    use crate::doctor::report::CheckStatus;
    use std::path::PathBuf;

    fn p(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    fn installed(binary: &str, data_dir: &str, loaded: bool) -> ServiceState {
        ServiceState::Installed {
            file: ServiceFile {
                path: PathBuf::from("/svc/file"),
                binary: p(binary),
                data_dir: p(data_dir),
                unreadable: None,
            },
            loaded,
        }
    }

    #[test]
    fn daemon_running_check_maps_each_status() {
        let pid = Path::new("/data/daemon.pid");
        let running = daemon_running_check(DaemonStatus::Running(42), pid);
        assert_eq!(running.status, CheckStatus::Ok);
        assert_eq!(running.detail, "daemon running (pid 42)");

        let stopped = daemon_running_check(DaemonStatus::Stopped, pid);
        assert_eq!(stopped.status, CheckStatus::Info);
        assert_eq!(stopped.hint.as_deref(), Some("citadel daemon start"));

        let stale = daemon_running_check(DaemonStatus::Stale, pid);
        assert_eq!(stale.status, CheckStatus::Warn);
        assert_eq!(
            stale.detail,
            "stale daemon.pid at /data/daemon.pid; no daemon holds it"
        );
        assert_eq!(stale.hint.as_deref(), Some("citadel daemon start"));
    }

    #[test]
    fn service_unsupported_is_one_info_row() {
        let checks = service_checks(
            &ServiceState::Unsupported,
            Path::new("/data"),
            Path::new("/bin/c"),
        );
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Info);
        assert_eq!(checks[0].detail, "not supported on this platform");
    }

    #[test]
    fn service_not_installed_is_info_without_fail() {
        let checks = service_checks(
            &ServiceState::NotInstalled,
            Path::new("/data"),
            Path::new("/bin/c"),
        );
        assert!(!checks.is_empty());
        assert!(checks.iter().all(|c| c.status == CheckStatus::Info));
    }

    #[test]
    fn service_installed_and_matching_is_ok() {
        let checks = service_checks(
            &installed("/bin/c", "/data", true),
            Path::new("/data"),
            Path::new("/bin/c"),
        );
        assert!(checks.iter().all(|c| c.status == CheckStatus::Ok));
    }

    #[test]
    fn service_installed_not_loaded_is_info_with_start_hint() {
        // `citadel daemon stop` unloads the job: a normal state, not a warning.
        let checks = service_checks(
            &installed("/bin/c", "/data", false),
            Path::new("/data"),
            Path::new("/bin/c"),
        );
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].status, CheckStatus::Info);
        assert!(checks[0].detail.contains("not loaded"));
        assert_eq!(checks[0].hint.as_deref(), Some("citadel daemon start"));
    }

    #[test]
    fn service_other_data_dir_warns() {
        let checks = service_checks(
            &installed("/bin/c", "/other", true),
            Path::new("/data"),
            Path::new("/bin/c"),
        );
        let warn = checks
            .iter()
            .find(|c| c.status == CheckStatus::Warn)
            .expect("a warn row");
        assert!(warn
            .detail
            .contains("service uses data dir /other; this CLI uses /data"));
        assert!(warn.hint.is_some());
    }

    #[test]
    fn service_other_binary_warns_with_install_hint() {
        let checks = service_checks(
            &installed("/old/c", "/data", true),
            Path::new("/data"),
            Path::new("/bin/c"),
        );
        let warn = checks
            .iter()
            .find(|c| c.status == CheckStatus::Warn)
            .expect("a warn row");
        assert!(warn.detail.contains("/old/c"));
        assert_eq!(warn.hint.as_deref(), Some("citadel daemon install"));
    }

    #[test]
    fn service_unknown_location_warns_with_reason_and_hint() {
        let state = ServiceState::Unknown {
            reason: "no home directory".to_string(),
        };
        let checks = service_checks(&state, Path::new("/data"), Path::new("/bin/c"));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert!(checks[0].detail.contains("no home directory"));
        assert!(checks[0].hint.is_some());
    }

    #[test]
    fn service_unreadable_file_warns_with_hint() {
        let ServiceState::Installed { mut file, loaded } = installed("/bin/c", "/data", true)
        else {
            unreachable!()
        };
        file.binary = None;
        file.data_dir = None;
        file.unreadable = Some("permission denied".to_string());
        let state = ServiceState::Installed { file, loaded };
        let checks = service_checks(&state, Path::new("/data"), Path::new("/bin/c"));
        let warn = checks
            .iter()
            .find(|c| c.status == CheckStatus::Warn)
            .expect("a warn row");
        assert!(warn.detail.contains("unreadable"));
        assert!(warn.detail.contains("permission denied"));
        assert!(warn.hint.is_some());
    }

    fn procs(d: usize, s: usize) -> Vec<CitadelProcess> {
        let mut v = Vec::new();
        for i in 0..d {
            v.push(CitadelProcess {
                pid: 100 + i as u32,
                kind: ProcessKind::Daemon,
            });
        }
        for i in 0..s {
            v.push(CitadelProcess {
                pid: 200 + i as u32,
                kind: ProcessKind::Serve,
            });
        }
        v
    }

    #[test]
    fn ps_parser_finds_daemon_and_serve_only() {
        let text = "37556 /Users/x/.cargo/bin/citadel daemon start --foreground\n\
16826 /Users/x/.cargo/bin/citadel mcp\n\
16824 /App/disclaimer --pgroup -- /Users/x/.cargo/bin/citadel mcp\n\
900 citadel serve --bind 0.0.0.0:4317\n\
5 /usr/bin/vim citadel\n";
        assert_eq!(
            parse_ps_output(text),
            vec![
                CitadelProcess {
                    pid: 37556,
                    kind: ProcessKind::Daemon
                },
                CitadelProcess {
                    pid: 900,
                    kind: ProcessKind::Serve
                },
            ]
        );
    }

    #[test]
    fn two_daemons_warn_may_differ() {
        let c = process_check(&procs(2, 0));
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.contains("2 daemon processes, 0 serve processes"));
        assert!(c.detail.contains("may be different data dirs"));
        assert!(c.hint.is_some());
    }

    #[test]
    fn daemon_and_serve_warn_both_sync() {
        let c = process_check(&procs(1, 1));
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c
            .detail
            .contains("if they share a vault, both run scheduled Syncs"));
        assert_eq!(
            c.hint.as_deref(),
            Some("stop one of them (citadel daemon stop, or stop the serve process)")
        );
    }

    #[test]
    fn one_daemon_is_ok() {
        let c = process_check(&procs(1, 0));
        assert_eq!(c.status, CheckStatus::Ok);
        assert_eq!(c.detail, "1 daemon process, 0 serve processes");
    }

    /// Serve `app` on an ephemeral loopback port.
    async fn spawn_router(
        app: axum::Router,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, server)
    }

    #[tokio::test]
    async fn probe_serve_reads_version_from_real_router() {
        let vault = tempfile::tempdir().unwrap();
        let (addr, server) = spawn_router(crate::mcp::http::mcp_router(
            crate::config::VaultConfig::new(vault.path()),
        ))
        .await;
        let probe = probe_serve(&format!("http://{addr}/mcp")).await;
        server.abort();
        assert_eq!(
            probe,
            ServeProbe::Answering {
                version: Some(env!("CARGO_PKG_VERSION").to_string())
            }
        );
    }

    #[tokio::test]
    async fn probe_serve_closed_port_is_not_answering() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert_eq!(
            probe_serve(&format!("http://{addr}/mcp")).await,
            ServeProbe::NotAnswering
        );
    }

    #[test]
    fn serve_not_answering_warns_only_when_an_entry_uses_it() {
        let url = "http://127.0.0.1:4317/mcp";
        let quiet = serve_check(url, &ServeProbe::NotAnswering, false);
        assert_eq!(quiet.status, CheckStatus::Info);
        let loud = serve_check(url, &ServeProbe::NotAnswering, true);
        assert_eq!(loud.status, CheckStatus::Warn);
        assert_eq!(
            loud.hint.as_deref(),
            Some("start it with make run or citadel serve, or re-run citadel setup --transport stdio")
        );
        let up = serve_check(url, &ServeProbe::Answering { version: None }, true);
        assert_eq!(up.status, CheckStatus::Ok);
    }
}
