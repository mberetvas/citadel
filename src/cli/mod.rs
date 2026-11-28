use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;

pub use crate::skill::SkillTarget;

use crate::config::VaultConfig;
use crate::db::{execute_readonly_query, QueryResult};
use crate::ingest::{self, parse_duration_secs, SourceDetail, SourceSummary, SyncSummary};
use crate::setup::agents::{AgentKind, McpTransport, SetupComponents, SetupScope};
use crate::sources::{AddSourceParams, Removal, SourceLifecycle, SourceSyncResult, SyncOutcome};

/// Local knowledge vault & MCP server for documentation sites and Markdown folders.
#[derive(Debug, Parser)]
#[command(name = "citadel", version, about)]
pub struct Cli {
    /// Override the vault data directory (also respects CITADEL_HOME).
    #[arg(long, global = true, value_name = "DIR", env = "CITADEL_HOME")]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Register a documentation URL or local Markdown directory.
    Add {
        /// http(s):// root URL or local directory path.
        #[arg(value_name = "URL_OR_PATH")]
        origin: String,

        /// Source slug name (auto-derived if omitted).
        #[arg(long)]
        name: Option<String>,

        /// Comma-separated tags for filtering.
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,

        /// Refresh interval duration (e.g. 24h, 30m, 0).
        #[arg(long)]
        interval: Option<String>,

        /// Maximum number of pages to ingest.
        #[arg(long)]
        max_pages: Option<u32>,

        /// Maximum crawl depth.
        #[arg(long)]
        max_depth: Option<u32>,

        /// Glob or prefix patterns to include.
        #[arg(long = "include", value_name = "PAT")]
        include: Vec<String>,

        /// Glob or prefix patterns to exclude.
        #[arg(long = "exclude", value_name = "PAT")]
        exclude: Vec<String>,

        /// Render JavaScript pages using headless Chrome (requires `chrome` feature).
        #[arg(long)]
        render_js: bool,

        /// Register the source without running an immediate initial sync.
        #[arg(long)]
        no_sync: bool,
    },

    /// List configured sources and health metrics.
    List {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show source details, sync history, and stale/error pages.
    Show {
        /// Source slug name.
        name: String,
    },

    /// Sync one or all sources manually.
    Sync {
        /// Source slug name (optional when --all is used).
        name: Option<String>,

        /// Sync all registered sources.
        #[arg(long)]
        all: bool,

        /// Remove stale pages instead of retaining them with status='stale'.
        #[arg(long)]
        prune: bool,
    },

    /// Remove a source and cascade delete its stored pages.
    Rm {
        /// Source slug name.
        name: String,

        /// Skip confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Run a read-only SQLite/FTS5 query against the vault.
    Query {
        /// Single read-only SQL SELECT statement.
        sql: String,

        /// Maximum rows to return (default 50, max 200).
        #[arg(long, default_value_t = 50)]
        limit: usize,

        /// Output format.
        #[arg(long, value_enum, default_value_t = QueryFormat::Table)]
        format: QueryFormat,
    },

    /// Run the all-in-one server (daemon + Web UI + MCP Streamable HTTP on /mcp).
    Serve {
        /// Socket address to bind (also respects CITADEL_BIND).
        #[arg(long, env = "CITADEL_BIND", default_value = "0.0.0.0:4317")]
        bind: String,
    },

    /// Start the stdio MCP server (newline-delimited JSON-RPC 2.0).
    Mcp,

    /// Launch the embedded local Web UI server.
    Ui {
        /// Port to bind on 127.0.0.1.
        #[arg(long)]
        port: Option<u16>,

        /// Do not automatically open the browser.
        #[arg(long)]
        no_open: bool,
    },

    /// Manage the background sync daemon and OS service registration.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },

    /// Check whether citadel is up to date, running and set up.
    Doctor {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Install the embedded agent router skill.
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },

    /// Interactive wizard (or flag-driven setup) to configure skill & MCP for AI agents.
    Setup(SetupArgs),

    /// Check for or install the latest GitHub Release binary.
    Update {
        /// Check for available updates without downloading or modifying the binary.
        #[arg(long)]
        check: bool,

        /// Skip confirmation prompt when installing an update.
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Internal detached helper to refresh `update-check.json` after short-lived CLI commands.
    #[command(name = "__update-check-refresh", hide = true)]
    UpdateCheckRefresh {
        #[arg(long, hide = true)]
        api_url: Option<String>,
    },
}

/// CLI arguments for `citadel setup`.
#[derive(Debug, Clone, Args)]
pub struct SetupArgs {
    /// Target agent(s) to configure (repeatable).
    #[arg(long = "agent", value_enum)]
    pub agents: Vec<AgentKind>,

    /// Installation scope (local folder or global user home).
    #[arg(long, value_enum)]
    pub scope: Option<SetupScope>,

    /// MCP transport mode.
    #[arg(long, value_enum)]
    pub transport: Option<TransportArg>,

    /// HTTP MCP endpoint URL (used when transport is `http`).
    #[arg(long, default_value = crate::setup::DEFAULT_HTTP_MCP_URL)]
    pub url: String,

    /// Which components to install/configure (default: both).
    #[arg(long, value_enum)]
    pub only: Option<SetupComponents>,

    /// Overwrite existing Citadel skill files, symlinks, or MCP entries.
    #[arg(long)]
    pub force: bool,

    /// Run non-interactively without prompting for confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TransportArg {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum QueryFormat {
    Table,
    Json,
    Csv,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommands {
    /// Start the background daemon.
    Start {
        /// Run in the foreground rather than detaching.
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running daemon.
    Stop,
    /// Check daemon status.
    Status,
    /// Install launchd (macOS) or systemd --user (Linux) service.
    Install,
    /// Uninstall the OS background service.
    Uninstall,
}

#[derive(Debug, Subcommand)]
pub enum SkillCommands {
    /// Install the embedded SKILL.md router file.
    Install {
        /// Target parent directory (installs into <dir>/citadel/SKILL.md).
        #[arg(long)]
        path: Option<PathBuf>,

        /// Install globally to user agent skill directories.
        #[arg(long)]
        global: bool,

        /// Target agent skill directory when installing globally.
        #[arg(long, value_enum, default_value_t = SkillTarget::All)]
        target: SkillTarget,

        /// Overwrite existing SKILL.md or symlinks.
        #[arg(long)]
        force: bool,
    },
}

/// Run a Source lifecycle future to completion on a fresh multi-threaded runtime.
fn block_on<T>(fut: impl std::future::Future<Output = T>) -> anyhow::Result<T> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    Ok(rt.block_on(fut))
}

/// Parse CLI arguments from the process environment and execute.
pub fn run_from_args() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run_cli(cli)
}

/// Execute a parsed [`Cli`] invocation, writing output to standard output.
pub fn run_cli(cli: Cli) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    run_cli_with_writer(cli, &mut out)
}

/// Execute a parsed [`Cli`] invocation, writing formatted output to `out`.
pub fn run_cli_with_writer<W: Write>(cli: Cli, out: &mut W) -> anyhow::Result<()> {
    let config = VaultConfig::resolve(cli.data_dir)?;
    tracing::debug!(data_dir = %config.data_dir.display(), "resolved vault config");

    let should_check_update = !matches!(
        &cli.command,
        Commands::Mcp
            | Commands::Update { .. }
            | Commands::UpdateCheckRefresh { .. }
            | Commands::Serve { .. }
            | Commands::Ui { .. }
            | Commands::Daemon { .. }
            | Commands::Doctor { .. }
            | Commands::List { json: true }
            | Commands::Query {
                format: QueryFormat::Json,
                ..
            }
    );

    let result = dispatch_command(cli.command, &config, out);
    if result.is_ok() && should_check_update {
        crate::update::check::maybe_print_cli_update_notice(&config);
    }
    result
}

fn dispatch_command<W: Write>(
    command: Commands,
    config: &VaultConfig,
    out: &mut W,
) -> anyhow::Result<()> {
    let config = config.clone();
    match command {
        Commands::Add {
            origin,
            name,
            tags,
            interval,
            max_pages,
            max_depth,
            include,
            exclude,
            render_js,
            no_sync,
        } => {
            let refresh_interval_secs = interval.as_deref().map(parse_duration_secs).transpose()?;
            let params = AddSourceParams {
                origin,
                name,
                tags,
                refresh_interval_secs,
                max_pages,
                max_depth,
                include_patterns: include,
                exclude_patterns: exclude,
                render_js,
            };
            let lifecycle = SourceLifecycle::new(config);
            let added = block_on(lifecycle.add(params, !no_sync))??;
            let (source, summary) = (added.source, added.summary);
            writeln!(
                out,
                "Added {} source '{}' ({})",
                source.kind.as_str(),
                source.name,
                source.origin
            )?;
            if let Some(sum) = summary {
                write_sync_summary(out, &sum)?;
            }
            Ok(())
        }
        Commands::List { json } => {
            let sources = ingest::list_sources(&config)?;
            if json {
                serde_json::to_writer_pretty(&mut *out, &sources)?;
                writeln!(out)?;
            } else {
                write_sources_table(out, &sources)?;
            }
            Ok(())
        }
        Commands::Show { name } => {
            let detail = ingest::show_source(&config, &name)?;
            write_source_detail(out, &detail)?;
            Ok(())
        }
        Commands::Sync { name, all, prune } => {
            let lifecycle = SourceLifecycle::new(config);
            match (name, all) {
                (Some(_), true) => {
                    anyhow::bail!("cannot specify both a source <name> and --all")
                }
                (None, false) => anyhow::bail!("specify a source <name> or pass --all"),
                (Some(name), false) => match block_on(lifecycle.sync(&name, prune))?? {
                    SyncOutcome::Synced(summary) => write_sync_summary(out, &summary)?,
                    SyncOutcome::AlreadySyncing => {
                        writeln!(out, "Source '{name}' is already syncing; nothing started.")?;
                    }
                },
                (None, true) => {
                    let results = block_on(lifecycle.sync_all(prune))??;
                    if results.is_empty() {
                        writeln!(out, "No sources configured.")?;
                    }
                    let mut failed = 0usize;
                    for SourceSyncResult { name, result } in &results {
                        match result {
                            Ok(SyncOutcome::Synced(summary)) => write_sync_summary(out, summary)?,
                            Ok(SyncOutcome::AlreadySyncing) => {
                                writeln!(out, "Source '{name}' is already syncing; skipped.")?;
                            }
                            Err(err) => {
                                failed += 1;
                                writeln!(out, "Failed '{name}': {err}")?;
                            }
                        }
                    }
                    if failed > 0 {
                        anyhow::bail!("{failed} of {} sources failed to sync", results.len());
                    }
                }
            }
            Ok(())
        }
        Commands::Rm { name, yes } => {
            if !yes {
                // Verify the source exists before prompting so missing names fail fast.
                let _ = ingest::show_source(&config, &name)?;
                if !std::io::stdin().is_terminal() {
                    anyhow::bail!(
                        "refusing to remove source '{name}' without confirmation; pass --yes to skip prompt"
                    );
                }
                write!(out, "Remove source '{name}' and all indexed pages? [y/N]: ")?;
                out.flush()?;
                let mut answer = String::new();
                std::io::stdin().lock().read_line(&mut answer)?;
                let trimmed = answer.trim().to_ascii_lowercase();
                if trimmed != "y" && trimmed != "yes" {
                    writeln!(out, "Aborted.")?;
                    return Ok(());
                }
            }

            let removed = match block_on(SourceLifecycle::new(config).remove(&name))?? {
                Removal::Removed(removed) => removed,
                Removal::AlreadySyncing => anyhow::bail!(
                    "source '{name}' is syncing; nothing was removed, try again when it finishes"
                ),
            };
            writeln!(
                out,
                "Removed source '{}' ({} pages deleted).",
                removed.name, removed.pages_deleted
            )?;
            Ok(())
        }
        Commands::Query { sql, limit, format } => {
            let result = execute_readonly_query(&config.db_path(), &sql, Some(limit))?;
            write_query_result(out, &result, format)?;
            Ok(())
        }
        Commands::Serve { bind } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(crate::serve::run_serve_until_signal(config, &bind))?;
            Ok(())
        }
        Commands::Mcp => {
            crate::mcp::run_stdio_with_io(&config, std::io::stdin().lock(), &mut *out)?;
            Ok(())
        }
        Commands::Ui { port, no_open } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(crate::ui::run_ui_server(config, port, no_open))?;
            Ok(())
        }
        Commands::Doctor { json } => {
            let opts = crate::doctor::DoctorOpts {
                cwd: std::env::current_dir()?,
                home: directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()),
                current_exe: std::env::current_exe()?,
                releases_api_url: crate::update::check::DEFAULT_RELEASES_API_URL.to_string(),
                serve_url: crate::setup::DEFAULT_HTTP_MCP_URL.to_string(),
                live_release_check: !config.no_update_check,
                in_container: crate::update::is_running_in_container(),
            };
            let report = block_on(crate::doctor::run_doctor(&config, &opts))?;
            if json {
                report.render_json(out)?;
            } else {
                report.render_text(out)?;
            }
            let failed = report.fail_count();
            if failed > 0 {
                anyhow::bail!("{failed} doctor check(s) failed");
            }
            Ok(())
        }
        Commands::Daemon { command } => match command {
            DaemonCommands::Start { foreground: true } => {
                crate::daemon::run_daemon(&config)?;
                Ok(())
            }
            DaemonCommands::Start { foreground: false } => {
                use crate::daemon::supervision::{Started, Supervision};
                let supervision = Supervision::for_vault(&config, std::env::current_exe()?);
                let (pid, how) = match supervision.start()? {
                    Started::Service(pid) => (pid, " through its OS service"),
                    Started::Spawned(pid) => (pid, ""),
                };
                writeln!(
                    out,
                    "Started citadel daemon{how} (pid {pid}, log: {}).",
                    config.log_path().display()
                )?;
                Ok(())
            }
            DaemonCommands::Stop => {
                use crate::daemon::supervision::{Stopped, Supervision, Wait};
                let supervision = Supervision::for_vault(&config, std::env::current_exe()?);
                let interrupted = ctrl_c_flag();
                let mut last_line: Option<std::time::Instant> = None;
                let stopped = supervision.stop(&mut |draining| {
                    if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
                        return Wait::Abort;
                    }
                    let due = last_line.map_or(draining.waited >= WAIT_LINE_AFTER, |at| {
                        at.elapsed() >= WAIT_LINE_EVERY
                    });
                    if due {
                        let sources = syncing_sources(&config, draining.pid);
                        // Best effort: a broken pipe must not end the wait.
                        let _ = writeln!(out, "{}", waiting_line(&sources, draining));
                        let _ = out.flush();
                        last_line = Some(std::time::Instant::now());
                    }
                    Wait::Continue
                })?;
                match stopped {
                    Stopped::Stopped(pid) => writeln!(out, "Stopped citadel daemon (pid {pid}).")?,
                    Stopped::Relaunched { pid, by } => writeln!(
                        out,
                        "Stopped citadel daemon (pid {pid}), but another daemon took over at \
                         once (pid {by}), most likely relaunched by a service manager."
                    )?,
                    Stopped::NotRunning => writeln!(out, "Daemon is not running.")?,
                    Stopped::StillDraining { pid, supervised } => {
                        writeln!(out, "{}", still_draining_message(pid, supervised))?
                    }
                }
                Ok(())
            }
            DaemonCommands::Status => {
                match crate::daemon::daemon_status(&config)? {
                    crate::daemon::DaemonStatus::Running(pid) => {
                        writeln!(out, "Daemon is running (pid {pid}).")?;
                    }
                    crate::daemon::DaemonStatus::Stopped => {
                        writeln!(out, "Daemon is stopped.")?;
                    }
                    crate::daemon::DaemonStatus::Stale => {
                        writeln!(
                            out,
                            "Daemon is not running (stale lockfile at {}).",
                            config.pid_path().display()
                        )?;
                    }
                }
                Ok(())
            }
            DaemonCommands::Install => {
                let path = crate::daemon::install_service(&config)?;
                writeln!(out, "Installed background service at {}.", path.display())?;
                Ok(())
            }
            DaemonCommands::Uninstall => {
                match crate::daemon::uninstall_service()? {
                    Some(path) => {
                        writeln!(
                            out,
                            "Uninstalled background service from {}.",
                            path.display()
                        )?;
                    }
                    None => writeln!(out, "No background service was installed.")?,
                }
                Ok(())
            }
        },
        Commands::Skill { command } => match command {
            SkillCommands::Install {
                path,
                global,
                target,
                force,
            } => {
                let report = crate::skill::install_skill(path.as_deref(), global, target, force)?;
                writeln!(
                    out,
                    "Installed skill to {}",
                    report.canonical_path.display()
                )?;
                for link in &report.symlinks {
                    writeln!(out, "Linked {}", link.display())?;
                }
                Ok(())
            }
        },
        Commands::Setup(args) => {
            let cwd = std::env::current_dir()?;
            let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
            let mode = setup_mode(
                &args,
                std::io::stdin().is_terminal(),
                std::io::stderr().is_terminal(),
            );

            if mode == SetupMode::Wizard {
                block_on(crate::setup::run_interactive_wizard(
                    &cwd,
                    home.as_deref(),
                    &args.url,
                ))??;
                return Ok(());
            }

            let skipped = mode.skip_reason();
            if args.agents.is_empty() {
                anyhow::bail!(
                    "{skipped}: add --agent <claude|agy|codex> and --scope <local|global>, or run `citadel setup` with no flags in a terminal for the wizard"
                );
            }
            let scope = args.scope.ok_or_else(|| {
                anyhow::anyhow!(
                    "{skipped}: add --scope <local|global>, or run `citadel setup` with no flags in a terminal for the wizard"
                )
            })?;
            let transport = args.transport.map(|t| match t {
                TransportArg::Stdio => McpTransport::Stdio,
                TransportArg::Http => McpTransport::Http(args.url.clone()),
            });

            let report = block_on(crate::setup::run_non_interactive(
                crate::setup::NonInteractiveSetupOpts {
                    cwd,
                    home,
                    agents: args.agents,
                    scope,
                    components: args.only.unwrap_or(SetupComponents::Both),
                    transport,
                    probe_url: args.url,
                    force: args.force,
                },
            ))??;
            for path in &report.written_paths {
                writeln!(out, "Configured {}", path.display())?;
            }
            Ok(())
        }
        Commands::Update { check, yes } => {
            let current_exe = std::env::current_exe()?;
            let opts = crate::update::UpdateOpts {
                releases_api_url: config.update_api_url.clone(),
                config,
                check_only: check,
                yes,
                current_version: env!("CARGO_PKG_VERSION").to_string(),
                current_exe,
                force_container: None,
                stdin_is_tty: None,
            };
            block_on(crate::update::run_update(opts, out))??;
            Ok(())
        }
        Commands::UpdateCheckRefresh { api_url } => {
            if !config.no_update_check {
                let url = api_url.as_deref().unwrap_or(&config.update_api_url);
                let cache_path = config.update_check_path();
                let _ = block_on(crate::update::check::refresh_cache(url, &cache_path));
            }
            Ok(())
        }
    }
}

/// How `citadel setup` runs, and why the wizard was skipped when it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupMode {
    Wizard,
    FlagsGiven,
    NoTerminal,
}

impl SetupMode {
    fn skip_reason(self) -> &'static str {
        match self {
            Self::Wizard => "the wizard was not run",
            Self::FlagsGiven => "setup flags were given, so the wizard is skipped",
            Self::NoTerminal => "no terminal is attached, so the wizard is skipped",
        }
    }
}

/// Open the `citadel setup` wizard only in a terminal and only when no setup choice was
/// given as a flag; otherwise the wizard would silently replace the user's flags.
fn setup_mode(args: &SetupArgs, stdin_is_tty: bool, stderr_is_tty: bool) -> SetupMode {
    let has_explicit_choice = args.yes
        || args.force
        || !args.agents.is_empty()
        || args.scope.is_some()
        || args.transport.is_some()
        || args.only.is_some();
    if has_explicit_choice {
        SetupMode::FlagsGiven
    } else if stdin_is_tty && stderr_is_tty {
        SetupMode::Wizard
    } else {
        SetupMode::NoTerminal
    }
}

fn write_sync_summary<W: Write>(out: &mut W, sum: &SyncSummary) -> std::io::Result<()> {
    write!(
        out,
        "Synced '{}': +{} added, ~{} updated, ={} unchanged, {} stale, {} failed",
        sum.source_name,
        sum.pages_added,
        sum.pages_updated,
        sum.pages_unchanged,
        sum.pages_stale,
        sum.pages_failed
    )?;
    if sum.pruned > 0 {
        write!(out, ", {} pruned", sum.pruned)?;
    }
    writeln!(out)?;
    for warning in &sum.warnings {
        writeln!(out, "Warning: {warning}")?;
    }
    Ok(())
}

fn write_sources_table<W: Write>(out: &mut W, sources: &[SourceSummary]) -> std::io::Result<()> {
    if sources.is_empty() {
        return writeln!(out, "No sources registered.");
    }

    let headers = vec![
        "NAME".to_string(),
        "KIND".to_string(),
        "STATUS".to_string(),
        "ACTIVE".to_string(),
        "STALE".to_string(),
        "ERROR".to_string(),
        "WORDS".to_string(),
        "LAST_SYNCED".to_string(),
        "ORIGIN".to_string(),
    ];

    let rows: Vec<Vec<String>> = sources
        .iter()
        .map(|s| {
            vec![
                s.name.clone(),
                s.kind.as_str().to_string(),
                s.status.clone(),
                s.active_pages.to_string(),
                s.stale_pages.to_string(),
                s.error_pages.to_string(),
                s.total_words.to_string(),
                s.last_synced_at.as_deref().unwrap_or("-").to_string(),
                s.origin.clone(),
            ]
        })
        .collect();

    render_ascii_table(out, &headers, &rows)
}

fn write_source_detail<W: Write>(out: &mut W, detail: &SourceDetail) -> std::io::Result<()> {
    let s = &detail.source;
    writeln!(out, "Name:        {}", s.name)?;
    writeln!(out, "Kind:        {}", s.kind.as_str())?;
    writeln!(out, "Origin:      {}", s.origin)?;
    writeln!(out, "Status:      {}", s.status)?;
    writeln!(
        out,
        "Tags:        {}",
        if s.tags.is_empty() {
            "-".to_string()
        } else {
            s.tags.join(", ")
        }
    )?;
    writeln!(out, "Interval:    {}s", s.refresh_interval_secs)?;
    writeln!(
        out,
        "Limits:      max_pages={}, max_depth={}, render_js={}",
        s.max_pages, s.max_depth, s.render_js
    )?;
    writeln!(
        out,
        "Pages:       {} active, {} stale, {} error ({} words)",
        s.active_pages, s.stale_pages, s.error_pages, s.total_words
    )?;
    writeln!(
        out,
        "Last Synced: {}",
        s.last_synced_at.as_deref().unwrap_or("-")
    )?;
    if let Some(err) = &s.last_error {
        writeln!(out, "Last Error:  {err}")?;
    }

    writeln!(out, "\nRecent Sync Runs:")?;
    if detail.sync_runs.is_empty() {
        writeln!(out, "  (none)")?;
    } else {
        let headers = vec![
            "ID".to_string(),
            "STATUS".to_string(),
            "STARTED_AT".to_string(),
            "ADDED".to_string(),
            "UPDATED".to_string(),
            "UNCHANGED".to_string(),
            "STALE".to_string(),
            "FAILED".to_string(),
        ];
        let rows: Vec<Vec<String>> = detail
            .sync_runs
            .iter()
            .map(|r| {
                vec![
                    r.id.to_string(),
                    r.status.clone(),
                    r.started_at.clone(),
                    r.pages_added.to_string(),
                    r.pages_updated.to_string(),
                    r.pages_unchanged.to_string(),
                    r.pages_stale.to_string(),
                    r.pages_failed.to_string(),
                ]
            })
            .collect();
        render_ascii_table(out, &headers, &rows)?;
    }

    writeln!(out, "\nStale / Error Pages:")?;
    if detail.issue_pages.is_empty() {
        writeln!(out, "  (none)")?;
    } else {
        let headers = vec![
            "STATUS".to_string(),
            "URI".to_string(),
            "LAST_CHECKED".to_string(),
            "ERROR".to_string(),
        ];
        let rows: Vec<Vec<String>> = detail
            .issue_pages
            .iter()
            .map(|p| {
                vec![
                    p.status.clone(),
                    p.uri.clone(),
                    p.last_checked_at.clone(),
                    p.error_message.as_deref().unwrap_or("-").to_string(),
                ]
            })
            .collect();
        render_ascii_table(out, &headers, &rows)?;
    }

    Ok(())
}

fn write_query_result<W: Write>(
    out: &mut W,
    result: &QueryResult,
    format: QueryFormat,
) -> anyhow::Result<()> {
    match format {
        QueryFormat::Json => {
            serde_json::to_writer_pretty(&mut *out, result)?;
            writeln!(out)?;
        }
        QueryFormat::Csv => {
            write_csv_row(out, result.columns.iter().map(String::as_str))?;
            for row in &result.rows {
                let cells: Vec<String> = row.iter().map(format_csv_cell).collect();
                write_csv_row(out, cells.iter().map(String::as_str))?;
            }
        }
        QueryFormat::Table => {
            if result.columns.is_empty() {
                writeln!(out, "(0 rows)")?;
                return Ok(());
            }
            let formatted_rows: Vec<Vec<String>> = result
                .rows
                .iter()
                .map(|row| row.iter().map(format_table_cell).collect())
                .collect();
            render_ascii_table(out, &result.columns, &formatted_rows)?;
            writeln!(
                out,
                "({} row{}, {} ms)",
                result.row_count,
                if result.row_count == 1 { "" } else { "s" },
                result.elapsed_ms
            )?;
            if result.truncated {
                if let Some(hint) = &result.truncation_hint {
                    writeln!(out, "Note: {hint}")?;
                }
            }
        }
    }
    Ok(())
}

fn render_ascii_table<W: Write>(
    out: &mut W,
    headers: &[String],
    rows: &[Vec<String>],
) -> std::io::Result<()> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(idx) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }

    write_table_line(out, headers, &widths)?;
    for (idx, width) in widths.iter().enumerate() {
        if idx > 0 {
            write!(out, "-+-")?;
        }
        for _ in 0..*width {
            write!(out, "-")?;
        }
    }
    writeln!(out)?;

    for row in rows {
        write_table_line(out, row, &widths)?;
    }
    Ok(())
}

fn write_table_line<W: Write>(
    out: &mut W,
    cells: &[String],
    widths: &[usize],
) -> std::io::Result<()> {
    for (idx, width) in widths.iter().enumerate() {
        if idx > 0 {
            write!(out, " | ")?;
        }
        let val = cells.get(idx).map(String::as_str).unwrap_or("");
        let char_len = val.chars().count();
        write!(out, "{val}")?;
        for _ in char_len..*width {
            write!(out, " ")?;
        }
    }
    writeln!(out)
}

fn format_table_cell(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.replace('\r', "").replace('\n', "\\n"),
        other => other.to_string(),
    }
}

fn format_csv_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn write_csv_row<'a, W: Write, I: Iterator<Item = &'a str>>(
    out: &mut W,
    fields: I,
) -> std::io::Result<()> {
    for (idx, field) in fields.enumerate() {
        if idx > 0 {
            write!(out, ",")?;
        }
        if field.contains([',', '"', '\n', '\r']) {
            write!(out, "\"{}\"", field.replace('"', "\"\""))?;
        } else {
            write!(out, "{field}")?;
        }
    }
    writeln!(out)
}

/// How long `citadel daemon stop` waits quietly before it says what it waits for.
const WAIT_LINE_AFTER: std::time::Duration = std::time::Duration::from_secs(1);
/// How often it says it again.
const WAIT_LINE_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// Set once the user presses Ctrl-C. While it is watched, Ctrl-C no longer ends the process.
fn ctrl_c_flag() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let set = std::sync::Arc::clone(&flag);
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        if rt.block_on(tokio::signal::ctrl_c()).is_ok() {
            set.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    flag
}

/// Names of the Sources whose Sync claim daemon `pid` holds; empty if the vault cannot be read.
fn syncing_sources(config: &VaultConfig, pid: u32) -> Vec<String> {
    let path = config.db_path();
    if !path.exists() {
        return Vec::new();
    }
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };
    let names = conn
        .prepare(
            "SELECT s.name FROM sync_claims c JOIN sources s ON s.id = c.source_id \
             WHERE c.pid = ?1 ORDER BY s.name",
        )
        .and_then(|mut stmt| {
            stmt.query_map([pid], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<String>>>()
        });
    names.unwrap_or_default()
}

/// The line `citadel daemon stop` prints while the daemon finishes its Syncs.
fn waiting_line(sources: &[String], draining: &crate::daemon::supervision::Draining) -> String {
    let what = match sources {
        [] => format!("the daemon (pid {}) to exit", draining.pid),
        [one] => format!("{one} to finish its Sync"),
        many => format!("{} to finish their Syncs", many.join(", ")),
    };
    format!(
        "Waiting for {what} ({}s so far; Ctrl-C to stop waiting)...",
        draining.waited.as_secs()
    )
}

/// What a `citadel daemon stop` left behind when the user stopped waiting.
fn still_draining_message(pid: u32, supervised: bool) -> String {
    let after = if supervised {
        "Its OS service is still running and relaunches it then: run `citadel daemon stop` \
         again to stop it for good."
    } else {
        "Nothing restarts it."
    };
    format!(
        "Stopped waiting. The daemon (pid {pid}) got SIGTERM and is still finishing its Sync; \
         it exits once the Sync is done. {after}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::QueryResult;
    use crate::ingest::SourceSummary;
    use tempfile::tempdir;

    #[test]
    fn daemon_stop_names_the_sources_it_waits_for() {
        let dir = tempdir().expect("tempdir");
        let config = VaultConfig::new(dir.path());
        assert!(syncing_sources(&config, 7).is_empty(), "no vault.db yet");
        let conn = crate::db::open_rw_db(&config.db_path()).expect("db");
        for (id, name) in [(1, "dlthub"), (2, "duckdb"), (3, "other")] {
            conn.execute(
                "INSERT INTO sources (id, name, kind, origin) VALUES (?1, ?2, 'web', ?3)",
                rusqlite::params![id, name, format!("https://{name}.example.com")],
            )
            .expect("source");
        }
        for (source, pid) in [(2, 7), (1, 7), (3, 8)] {
            conn.execute(
                "INSERT INTO sync_claims (source_id, pid, instance_token, claimed_at) \
                 VALUES (?1, ?2, 't', '2026-11-28T10:00:00Z')",
                rusqlite::params![source, pid],
            )
            .expect("claim");
        }
        assert_eq!(syncing_sources(&config, 7), ["dlthub", "duckdb"]);

        let draining = crate::daemon::supervision::Draining {
            pid: 7,
            waited: std::time::Duration::from_secs(12),
        };
        assert_eq!(
            waiting_line(&["dlthub".to_string()], &draining),
            "Waiting for dlthub to finish its Sync (12s so far; Ctrl-C to stop waiting)..."
        );
        assert!(waiting_line(&[], &draining).starts_with("Waiting for the daemon (pid 7) to exit"));
    }

    #[test]
    fn daemon_stop_says_how_to_finish_after_ctrl_c() {
        let supervised = still_draining_message(7, true);
        assert!(supervised.contains("pid 7") && supervised.contains("still finishing"));
        assert!(
            supervised.contains("run `citadel daemon stop` again"),
            "{supervised}"
        );
        assert!(still_draining_message(7, false).contains("Nothing restarts it."));
    }

    fn run_args(vault_dir: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
        let vault_str = vault_dir.to_string_lossy().into_owned();
        let mut argv = vec!["citadel".to_string(), "--data-dir".to_string(), vault_str];
        argv.extend(args.iter().map(|s| (*s).to_string()));
        let cli = Cli::try_parse_from(argv)?;
        let mut buf = Vec::new();
        run_cli_with_writer(cli, &mut buf)?;
        Ok(String::from_utf8(buf)?)
    }

    #[test]
    fn sync_all_exits_nonzero_when_any_source_fails() {
        let vault_dir = tempdir().expect("vault tempdir");
        let good = tempdir().expect("good docs");
        let gone = tempdir().expect("gone docs");
        std::fs::write(good.path().join("a.md"), "# A\n\nAlpha.\n").expect("write");
        let good_path = good.path().to_string_lossy().into_owned();
        let gone_path = gone.path().to_string_lossy().into_owned();
        run_args(
            vault_dir.path(),
            &["add", &good_path, "--name", "good", "--no-sync"],
        )
        .expect("add good");
        run_args(
            vault_dir.path(),
            &["add", &gone_path, "--name", "gone", "--no-sync"],
        )
        .expect("add gone");
        drop(gone);

        let err = run_args(vault_dir.path(), &["sync", "--all"])
            .expect_err("a failed source makes sync --all fail");
        assert!(err.to_string().contains("1 of 2"), "{err}");

        let show = run_args(vault_dir.path(), &["show", "good"]).expect("show good");
        assert!(show.contains("ok"), "good source still synced: {show}");
    }

    #[test]
    fn add_rejects_private_origin() {
        let vault_dir = tempdir().expect("vault tempdir");
        let err = run_args(vault_dir.path(), &["add", "http://127.0.0.1/", "--no-sync"])
            .expect_err("loopback origin must be refused");
        let vault_err = err
            .downcast_ref::<crate::error::VaultError>()
            .expect("VaultError");
        assert!(
            matches!(vault_err, crate::error::VaultError::Validation(_)),
            "{vault_err:?}"
        );
    }

    #[test]
    fn parses_setup_subcommand_flags() {
        let cli = Cli::try_parse_from([
            "citadel",
            "setup",
            "--agent",
            "claude",
            "--agent",
            "agy",
            "--scope",
            "local",
            "--transport",
            "stdio",
            "--yes",
        ])
        .expect("setup flags should parse");

        match cli.command {
            Commands::Setup(args) => {
                assert_eq!(args.agents, vec![AgentKind::Claude, AgentKind::Agy]);
                assert_eq!(args.scope, Some(SetupScope::Local));
                assert_eq!(args.transport, Some(TransportArg::Stdio));
                assert_eq!(args.only, None);
                assert!(!args.force);
                assert!(args.yes);
            }
            other => panic!("expected Setup command, got {other:?}"),
        }
    }

    #[test]
    fn setup_opens_wizard_only_without_explicit_flags_in_a_terminal() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["citadel", "setup"];
            argv.extend_from_slice(extra);
            match Cli::try_parse_from(argv).expect("parse").command {
                Commands::Setup(args) => args,
                other => panic!("expected Setup command, got {other:?}"),
            }
        };

        assert_eq!(setup_mode(&parse(&[]), true, true), SetupMode::Wizard);
        assert_eq!(setup_mode(&parse(&[]), false, true), SetupMode::NoTerminal);
        for flags in [
            &["--only", "mcp"][..],
            &["--transport", "http"][..],
            &["--force"][..],
            &["--yes"][..],
            &["--agent", "claude"][..],
            &["--scope", "global"][..],
        ] {
            assert_eq!(
                setup_mode(&parse(flags), true, true),
                SetupMode::FlagsGiven,
                "{flags:?} must not be silently replaced by wizard answers"
            );
        }
    }

    #[test]
    fn cli_parse_smoke() {
        let cases: &[&[&str]] = &[
            &[
                "citadel",
                "add",
                "https://example.com/docs",
                "--name",
                "example-docs",
                "--tags",
                "rust,sqlite",
                "--interval",
                "24h",
                "--max-pages",
                "200",
                "--max-depth",
                "4",
                "--include",
                "docs/*",
                "--exclude",
                "blog/*",
                "--render-js",
                "--no-sync",
            ],
            &["citadel", "list", "--json"],
            &["citadel", "show", "example-docs"],
            &["citadel", "sync", "example-docs", "--prune"],
            &["citadel", "sync", "--all"],
            &["citadel", "rm", "example-docs", "--yes"],
            &[
                "citadel",
                "query",
                "SELECT name FROM v_sources",
                "--limit",
                "25",
                "--format",
                "json",
            ],
            &["citadel", "serve", "--bind", "0.0.0.0:4317"],
            &["citadel", "mcp"],
            &["citadel", "ui", "--port", "4317", "--no-open"],
            &["citadel", "daemon", "start", "--foreground"],
            &["citadel", "daemon", "stop"],
            &["citadel", "daemon", "status"],
            &["citadel", "daemon", "install"],
            &["citadel", "daemon", "uninstall"],
            &[
                "citadel", "skill", "install", "--global", "--target", "claude", "--force",
            ],
            &["citadel", "skill", "install", "--path", "/tmp/skills"],
        ];

        for args in cases {
            Cli::try_parse_from(*args)
                .unwrap_or_else(|e| panic!("failed to parse {:?}: {e}", args));
        }
    }

    #[test]
    fn end_to_end_add_sync_query_rm() {
        let vault_dir = tempdir().expect("vault tempdir");
        let docs_dir = tempdir().expect("docs tempdir");

        std::fs::write(
            docs_dir.path().join("wal.md"),
            "# SQLite WAL\n\n## Concurrency\n\nWAL mode enables concurrent readers in SQLite.\n",
        )
        .expect("write wal.md");

        let docs_str = docs_dir.path().to_string_lossy().into_owned();
        let add_out = run_args(
            vault_dir.path(),
            &[
                "add",
                &docs_str,
                "--name",
                "sqlite-book",
                "--tags",
                "sqlite,db",
                "--interval",
                "0",
            ],
        )
        .expect("add should succeed");
        assert!(add_out.contains("sqlite-book"));

        let list_json = run_args(vault_dir.path(), &["list", "--json"]).expect("list --json");
        let listed: Vec<SourceSummary> =
            serde_json::from_str(&list_json).expect("valid JSON from list --json");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "sqlite-book");
        assert_eq!(listed[0].active_pages, 1);

        let show_out = run_args(vault_dir.path(), &["show", "sqlite-book"]).expect("show");
        assert!(show_out.contains("sqlite-book"));
        assert!(show_out.contains("ok"));

        // Update file and run sync
        std::fs::write(
            docs_dir.path().join("wal.md"),
            "# SQLite WAL\n\n## Concurrency\n\nWAL mode enables concurrent readers and checkpointing.\n",
        )
        .expect("update wal.md");
        let sync_out = run_args(vault_dir.path(), &["sync", "sqlite-book"]).expect("sync");
        assert!(sync_out.contains("sqlite-book"));

        let fts_sql = "SELECT src.name AS source, p.uri, s.heading_path \
                       FROM sections_fts \
                       JOIN sections s ON s.id = sections_fts.rowid \
                       JOIN pages p ON p.id = s.page_id \
                       JOIN sources src ON src.id = s.source_id \
                       WHERE sections_fts MATCH 'checkpointing'";

        let query_json = run_args(vault_dir.path(), &["query", fts_sql, "--format", "json"])
            .expect("query json");
        let qr: QueryResult = serde_json::from_str(&query_json).expect("valid JSON QueryResult");
        assert_eq!(qr.row_count, 1);
        assert_eq!(
            qr.columns,
            vec!["source", "uri", "heading_path"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(qr.rows[0][0], serde_json::json!("sqlite-book"));
        assert_eq!(qr.rows[0][1], serde_json::json!("wal.md"));
        assert_eq!(qr.rows[0][2], serde_json::json!("SQLite WAL > Concurrency"));

        let query_csv =
            run_args(vault_dir.path(), &["query", fts_sql, "--format", "csv"]).expect("query csv");
        assert!(query_csv.contains("source,uri,heading_path"));
        assert!(query_csv.contains("sqlite-book,wal.md,SQLite WAL > Concurrency"));

        let query_table = run_args(vault_dir.path(), &["query", fts_sql, "--format", "table"])
            .expect("query table");
        assert!(query_table.contains("sqlite-book"));
        assert!(query_table.contains("SQLite WAL > Concurrency"));

        let rm_out = run_args(vault_dir.path(), &["rm", "sqlite-book", "--yes"]).expect("rm --yes");
        assert!(rm_out.contains("sqlite-book"));

        let list_after = run_args(vault_dir.path(), &["list", "--json"]).expect("list after rm");
        let after: Vec<SourceSummary> =
            serde_json::from_str(&list_after).expect("valid JSON after rm");
        assert!(after.is_empty());
    }

    #[test]
    fn query_rejects_write_statements_through_cli() {
        let vault_dir = tempdir().expect("vault tempdir");
        let docs_dir = tempdir().expect("docs tempdir");
        std::fs::write(docs_dir.path().join("doc.md"), "# Doc\n\nText.\n").expect("write");
        let docs_str = docs_dir.path().to_string_lossy().into_owned();
        run_args(vault_dir.path(), &["add", &docs_str, "--name", "docs"]).expect("add");

        let err = run_args(vault_dir.path(), &["query", "DELETE FROM pages"])
            .expect_err("write query must fail");
        assert!(err.to_string().contains("invalid query"));
    }

    #[test]
    fn skill_install_path_and_force_through_cli() {
        let vault_dir = tempdir().expect("vault tempdir");
        let skills_dir = tempdir().expect("skills tempdir");
        let skills_str = skills_dir.path().to_string_lossy().into_owned();

        let out = run_args(
            vault_dir.path(),
            &["skill", "install", "--path", &skills_str],
        )
        .expect("skill install --path should succeed");
        let installed = skills_dir.path().join("citadel/SKILL.md");
        assert!(out.contains(&installed.display().to_string()));
        assert_eq!(
            std::fs::read_to_string(&installed).expect("read installed skill"),
            crate::skill::ROUTER_SKILL_MD
        );

        let overwrite_err = run_args(
            vault_dir.path(),
            &["skill", "install", "--path", &skills_str],
        )
        .expect_err("repeat install without --force must fail");
        assert!(overwrite_err.to_string().contains("--force"));

        run_args(
            vault_dir.path(),
            &["skill", "install", "--path", &skills_str, "--force"],
        )
        .expect("repeat install with --force should succeed");
    }
}
