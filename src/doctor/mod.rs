//! `citadel doctor`: read-only diagnostics over the installation and vault.

pub mod report;
pub mod running;
pub mod setup;
pub mod version;

use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::VaultConfig;
use crate::daemon::supervision::Supervision;
use report::{Check, Report, Section};
use running::ServeProbe;
use setup::{McpEntry, SetupLocation};

/// Inputs doctor reads from its environment, so tests and the CLI can set them.
#[derive(Debug, Clone)]
pub struct DoctorOpts {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub current_exe: PathBuf,
    pub releases_api_url: String,
    pub serve_url: String,
    pub live_release_check: bool,
    pub in_container: bool,
}

/// What doctor read at one setup location.
struct Inspected {
    loc: SetupLocation,
    skill: setup::SkillState,
    mcp: std::result::Result<Option<McpEntry>, String>,
}

impl Inspected {
    fn http_url(&self) -> Option<&str> {
        self.mcp.as_ref().ok()?.as_ref()?.http_url()
    }
}

/// Probe every distinct URL concurrently.
async fn probe_all(urls: Vec<String>) -> HashMap<String, ServeProbe> {
    let mut set = tokio::task::JoinSet::new();
    for url in urls {
        set.spawn(async move {
            let probe = running::probe_serve(&url).await;
            (url, probe)
        });
    }
    let mut probes = HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((url, probe)) = joined {
            probes.insert(url, probe);
        }
    }
    probes
}

/// Run every check and collect the report. Read-only: creates no file or directory.
pub async fn run_doctor(config: &VaultConfig, opts: &DoctorOpts) -> Report {
    let current = env!("CARGO_PKG_VERSION");
    let mut checks = Vec::new();

    // Gather what the checks need.
    let service = if opts.in_container {
        None
    } else {
        Some(Supervision::for_vault(config, opts.current_exe.clone()).service())
    };
    let home_is_cwd = opts
        .home
        .as_deref()
        .is_some_and(|home| crate::fsutil::resolves_to_same_dir(home, &opts.cwd));
    let inspected: Vec<Inspected> = setup::setup_locations(&opts.cwd, opts.home.as_deref())
        .into_iter()
        .filter(|loc| !(home_is_cwd && loc.scope == crate::setup::agents::SetupScope::Local))
        .map(|loc| Inspected {
            skill: setup::inspect_skill(&loc.skill_path),
            mcp: setup::read_mcp_entry(&loc.mcp_path, loc.mcp_is_toml),
            loc,
        })
        .collect();

    // Network checks run concurrently: the release check and each distinct HTTP URL once.
    let mut urls: Vec<String> = std::iter::once(opts.serve_url.clone())
        .chain(
            inspected
                .iter()
                .filter_map(|i| i.http_url().map(str::to_string)),
        )
        .collect();
    urls.sort();
    urls.dedup();
    let cache_path = config.update_check_path();
    let (latest, probes) = tokio::join!(
        version::latest_release(
            &cache_path,
            &opts.releases_api_url,
            current,
            opts.live_release_check,
        ),
        probe_all(urls),
    );
    let serve_probe = probes
        .get(&opts.serve_url)
        .cloned()
        .unwrap_or(ServeProbe::NotAnswering);
    let http_entry_points_here = inspected
        .iter()
        .any(|i| i.http_url() == Some(opts.serve_url.as_str()));

    // Daemon state feeds a Version row and a Running row.
    let (daemon_version_rows, daemon_running_row) = if opts.in_container {
        (None, None)
    } else {
        match version::gather_daemon_facts(config) {
            Ok(facts) => (
                version::daemon_version_check(current, &facts),
                Some(running::daemon_running_check(
                    facts.status,
                    &config.pid_path(),
                )),
            ),
            Err(err) => (
                None,
                Some(
                    Check::fail(
                        Section::Running,
                        "daemon",
                        format!("cannot read daemon state: {err}"),
                    )
                    .with_hint(&format!(
                        "check permissions on {}, then run citadel daemon status",
                        config.pid_path().display()
                    )),
                ),
            ),
        }
    };

    // Version.
    checks.push(version::cli_version_check(current, &latest));
    checks.extend(daemon_version_rows);
    checks.extend(version::serve_version_check(current, &serve_probe));

    // Running.
    checks.extend(daemon_running_row);
    if let Some(service) = &service {
        checks.extend(running::service_checks(
            service,
            &config.data_dir,
            &opts.current_exe,
        ));
        checks.push(running::process_check(&running::gather_processes()));
    } else {
        checks.push(Check::info(
            Section::Running,
            "container",
            "running in a container: serve is the daemon",
        ));
    }
    checks.push(running::serve_check(
        &opts.serve_url,
        &serve_probe,
        http_entry_points_here,
    ));

    // Setup.
    if home_is_cwd {
        checks.push(Check::info(
            Section::Setup,
            "local scope",
            "local scope skipped: current directory is the home directory",
        ));
    } else {
        checks.push(setup::local_scope_note(&opts.cwd));
    }
    let path_env = std::env::var_os("PATH");
    // Resolve (and version-probe) each distinct stdio command once.
    let mut resolved: HashMap<String, setup::StdioResolution> = HashMap::new();
    for item in &inspected {
        let stdio = match &item.mcp {
            Ok(Some(McpEntry::Stdio { command, .. })) => Some(
                resolved
                    .entry(command.clone())
                    .or_insert_with(|| {
                        setup::resolve_stdio_command(
                            command,
                            path_env.as_deref(),
                            &opts.current_exe,
                        )
                    })
                    .clone(),
            ),
            _ => None,
        };
        let http = item.http_url().and_then(|url| probes.get(url));
        checks.extend(setup::setup_checks(
            &item.loc,
            &item.skill,
            &item.mcp,
            stdio.as_ref(),
            http,
            current,
        ));
    }

    // Data.
    let mut data = format!("data dir {}", config.data_dir.display());
    if !config.data_dir.exists() {
        data.push_str(" (not created yet)");
    }
    checks.push(Check::info(Section::Data, "data dir", data));

    Report { checks }
}
