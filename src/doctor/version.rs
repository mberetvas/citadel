//! Version checks: CLI against the latest release, daemon against the CLI.

use std::path::Path;
use std::time::Duration;

use crate::config::VaultConfig;
use crate::daemon::info::{file_mtime_secs, read_daemon_info, DaemonInfo};
use crate::daemon::supervision::RESTART_HINT;
use crate::daemon::{daemon_status, DaemonStatus};
use crate::doctor::report::{Check, Section};
use crate::doctor::running::ServeProbe;
use crate::error::Result;
use crate::update::check::{
    fetch_release, is_newer_version, normalize_version_str, read_cache_entry, release_http_client,
};

const LIVE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// Latest release known to doctor, and where that knowledge came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestRelease {
    Live(String),
    Cached { version: String, checked_at: String },
    Unknown,
}

/// Look up the latest release: live (3 s timeout) when `live`, else or on failure the cache.
/// Doctor is read-only: it never writes the cache.
pub async fn latest_release(
    cache_path: &Path,
    api_url: &str,
    current: &str,
    live: bool,
) -> LatestRelease {
    if live {
        let check = async {
            let client = release_http_client(current, LIVE_CHECK_TIMEOUT)?;
            fetch_release(&client, api_url).await
        };
        if let Ok(Ok((_, latest))) = tokio::time::timeout(LIVE_CHECK_TIMEOUT, check).await {
            return LatestRelease::Live(latest);
        }
    }
    match read_cache_entry(cache_path) {
        Some((cache, version)) => LatestRelease::Cached {
            version,
            checked_at: cache.checked_at,
        },
        None => LatestRelease::Unknown,
    }
}

/// Compare the running CLI with the latest known release.
pub fn cli_version_check(current: &str, latest: &LatestRelease) -> Check {
    let name = "cli version";
    let (version, source) = match latest {
        LatestRelease::Live(version) => (version, String::new()),
        LatestRelease::Cached {
            version,
            checked_at,
        } => {
            let day = checked_at.get(..10).unwrap_or(checked_at);
            (version, format!(" (cached, checked {day})"))
        }
        LatestRelease::Unknown => {
            return Check::info(
                Section::Version,
                name,
                format!("citadel {current}, latest version unknown (offline)"),
            );
        }
    };
    let detail = format!("citadel {current}, latest: {version}{source}");
    if is_newer_version(current, version) {
        Check::warn(Section::Version, name, detail).with_hint("citadel update")
    } else {
        Check::ok(Section::Version, name, detail)
    }
}

/// What doctor knows about the daemon process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonFacts {
    pub status: DaemonStatus,
    /// `None` unless `daemon.json` describes the pid that currently holds the lock.
    pub info: Option<DaemonInfo>,
    /// Current mtime (unix secs) of the file at `info.exe`; `None` if it is gone.
    pub exe_mtime_now: Option<i64>,
}

pub fn gather_daemon_facts(config: &VaultConfig) -> Result<DaemonFacts> {
    let status = daemon_status(config)?;
    let info = match status {
        DaemonStatus::Running(pid) => {
            read_daemon_info(&config.daemon_info_path()).filter(|info| info.pid == pid)
        }
        _ => None,
    };
    let exe_mtime_now = info.as_ref().and_then(|info| file_mtime_secs(&info.exe));
    Ok(DaemonFacts {
        status,
        info,
        exe_mtime_now,
    })
}

/// Compare the running daemon with this CLI; `None` unless a daemon is running.
pub fn daemon_version_check(current: &str, facts: &DaemonFacts) -> Option<Check> {
    if !matches!(facts.status, DaemonStatus::Running(_)) {
        return None;
    }
    let name = "daemon version";
    let warn = |detail: String| Check::warn(Section::Version, name, detail).with_hint(RESTART_HINT);
    let Some(info) = &facts.info else {
        return Some(warn(
            "daemon started by an older citadel (version unknown)".to_string(),
        ));
    };
    let version = &info.version;
    let check = if normalize_version_str(version) != normalize_version_str(current) {
        warn(format!("daemon runs {version}, cli is {current}"))
    } else if facts.exe_mtime_now.is_none() {
        warn(format!("daemon runs {version}, binary no longer exists"))
    } else if info
        .exe_mtime_secs
        .is_some_and(|started| Some(started) != facts.exe_mtime_now)
    {
        warn(format!(
            "daemon runs {version}, binary replaced since the daemon started"
        ))
    } else {
        Check::ok(Section::Version, name, format!("daemon runs {version}"))
    };
    Some(check)
}

/// Compare the serve process version with the CLI; `None` unless it answered.
pub fn serve_version_check(current: &str, probe: &ServeProbe) -> Option<Check> {
    let ServeProbe::Answering { version } = probe else {
        return None;
    };
    let check = match version {
        Some(v) if normalize_version_str(v) == normalize_version_str(current) => Check::ok(
            Section::Version,
            "serve",
            format!("serve {v} matches the CLI"),
        ),
        Some(v) => Check::warn(
            Section::Version,
            "serve",
            format!("serve runs {v}, CLI is {current}"),
        )
        .with_hint("rebuild and restart the serve process or container (make build && make run)"),
        None => Check::info(Section::Version, "serve", "serve version unknown"),
    };
    Some(check)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::info::DaemonInfoGuard;
    use crate::daemon::PidLock;
    use crate::doctor::report::CheckStatus;
    use std::path::PathBuf;

    fn info(version: &str, mtime: Option<i64>) -> DaemonInfo {
        DaemonInfo {
            pid: 5,
            version: version.to_string(),
            exe: PathBuf::from("/bin/citadel"),
            exe_mtime_secs: mtime,
            started_at: "2026-11-25T10:00:00Z".to_string(),
        }
    }

    fn facts(info: Option<DaemonInfo>, now: Option<i64>) -> DaemonFacts {
        DaemonFacts {
            status: DaemonStatus::Running(5),
            info,
            exe_mtime_now: now,
        }
    }

    #[test]
    fn cli_up_to_date_is_ok() {
        let check = cli_version_check("0.3.0", &LatestRelease::Live("0.3.0".into()));
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.detail.contains("latest: 0.3.0"));
    }

    #[test]
    fn cli_behind_is_warn_with_update_hint() {
        let check = cli_version_check("0.3.0", &LatestRelease::Live("0.4.0".into()));
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.hint.as_deref(), Some("citadel update"));
    }

    #[test]
    fn cli_cached_says_cached() {
        let latest = LatestRelease::Cached {
            version: "0.3.0".into(),
            checked_at: "2026-11-24T10:00:00Z".into(),
        };
        let check = cli_version_check("0.3.0", &latest);
        assert!(check.detail.contains("cached, checked 2026-11-24"));
    }

    #[test]
    fn cli_unknown_is_info() {
        let check = cli_version_check("0.3.0", &LatestRelease::Unknown);
        assert_eq!(check.status, CheckStatus::Info);
        assert!(check.detail.contains("latest version unknown (offline)"));
    }

    #[tokio::test]
    async fn latest_release_falls_back_to_cache_on_http_error() {
        let app = axum::Router::new().route(
            "/latest",
            axum::routing::get(|| async { axum::http::StatusCode::FORBIDDEN }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("update-check.json");
        let content = r#"{"checked_at":"2026-11-24T10:00:00Z","latest_version":"0.3.0","release_url":"https://example.invalid/r"}"#;
        std::fs::write(&cache_path, content).unwrap();

        let started = std::time::Instant::now();
        let result =
            latest_release(&cache_path, &format!("http://{addr}/latest"), "0.3.0", true).await;
        assert!(started.elapsed() < Duration::from_millis(3500));
        assert!(matches!(result, LatestRelease::Cached { .. }));
        assert_eq!(std::fs::read_to_string(&cache_path).unwrap(), content);
    }

    #[test]
    fn daemon_same_version_same_binary_is_ok() {
        let check =
            daemon_version_check("0.3.0", &facts(Some(info("0.3.0", Some(100))), Some(100)))
                .expect("a version row");
        assert_eq!(check.status, CheckStatus::Ok);
    }

    #[test]
    fn daemon_other_version_warns_with_restart_hint() {
        let check =
            daemon_version_check("0.3.0", &facts(Some(info("0.2.0", Some(100))), Some(100)))
                .expect("a version row");
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(
            check.hint.as_deref(),
            Some("citadel daemon stop && citadel daemon start")
        );
    }

    #[test]
    fn daemon_binary_replaced_warns() {
        let check =
            daemon_version_check("0.3.0", &facts(Some(info("0.3.0", Some(100))), Some(200)))
                .expect("a version row");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check
            .detail
            .contains("binary replaced since the daemon started"));
    }

    #[test]
    fn restart_hint_never_kickstarts_a_swapped_binary() {
        // Every warn row means the binary changed under the daemon; launchd can then refuse to
        // spawn the job (`EX_CONFIG`) while `kickstart -k` exits 0.
        for (info, now) in [
            (Some(info("0.2.0", Some(100))), Some(100)),
            (Some(info("0.3.0", Some(100))), None),
            (Some(info("0.3.0", Some(100))), Some(200)),
            (None, None),
        ] {
            let check = daemon_version_check("0.3.0", &facts(info, now)).expect("a version row");
            assert_eq!(check.status, CheckStatus::Warn);
            // `citadel daemon stop && start` go through the daemon's service when it has one.
            assert_eq!(
                check.hint.as_deref(),
                Some(RESTART_HINT),
                "{}",
                check.detail
            );
        }
    }

    #[test]
    fn daemon_binary_missing_warns() {
        let check = daemon_version_check("0.3.0", &facts(Some(info("0.3.0", Some(100))), None))
            .expect("a version row");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("binary no longer exists"));
    }

    #[test]
    fn daemon_without_info_warns_older_citadel() {
        let check = daemon_version_check("0.3.0", &facts(None, None)).expect("a version row");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("started by an older citadel"));
    }

    #[test]
    fn stopped_daemon_has_no_version_rows() {
        let mut f = facts(None, None);
        f.status = DaemonStatus::Stopped;
        assert!(daemon_version_check("0.3.0", &f).is_none());
    }

    #[test]
    fn daemon_info_with_other_pid_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let config = VaultConfig::new(dir.path());
        let _lock = PidLock::acquire(&config.pid_path()).unwrap();
        let mut stale = info("0.3.0", Some(100));
        stale.pid = 1;
        let _guard = DaemonInfoGuard::write(&config.daemon_info_path(), &stale).unwrap();

        let facts = gather_daemon_facts(&config).unwrap();
        assert_eq!(facts.status, DaemonStatus::Running(std::process::id()));
        assert_eq!(facts.info, None);
    }

    #[test]
    fn serve_other_version_warns() {
        let probe = ServeProbe::Answering {
            version: Some("0.2.0".into()),
        };
        let c = serve_version_check("0.3.0", &probe).unwrap();
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.contains("0.2.0") && c.detail.contains("0.3.0"));
        assert_eq!(
            c.hint.as_deref(),
            Some("rebuild and restart the serve process or container (make build && make run)")
        );
        let same = ServeProbe::Answering {
            version: Some("0.3.0".into()),
        };
        assert_eq!(
            serve_version_check("0.3.0", &same).unwrap().status,
            CheckStatus::Ok
        );
        assert!(serve_version_check("0.3.0", &ServeProbe::NotAnswering).is_none());
    }
}
