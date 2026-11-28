pub mod check;

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flate2::read::GzDecoder;

use crate::config::VaultConfig;
use crate::daemon::DaemonStatus;
use crate::doctor::running::ServiceState;
use crate::error::{Result, VaultError};
use crate::ingest::normalize::sha256_hex;
use crate::update::check::{
    current_unix_secs, fetch_release, is_newer_version, normalize_version_str, read_cached_status,
    write_cache_atomic, UpdateCheckCache,
};

/// Options for [`run_update`].
#[derive(Debug, Clone)]
pub struct UpdateOpts {
    pub config: VaultConfig,
    pub check_only: bool,
    pub yes: bool,
    pub current_version: String,
    pub current_exe: PathBuf,
    pub releases_api_url: String,
    pub force_container: Option<bool>,
    pub stdin_is_tty: Option<bool>,
}

/// Detect whether Citadel is running inside an OCI container.
pub fn is_running_in_container() -> bool {
    if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
        return true;
    }
    crate::config::parse_flag(std::env::var("CITADEL_CONTAINER").ok().as_deref())
}

/// Map the compile-time target OS and architecture to the GitHub Release target triple.
pub fn target_triple() -> Result<&'static str> {
    release_target_triple(
        std::env::consts::OS,
        std::env::consts::ARCH,
        cfg!(target_env = "musl"),
    )
}

/// Release target triple for a build on `os`/`arch` (`musl` when built for the musl libc).
///
/// Linux releases are glibc builds, so a musl build has no tarball it can run.
fn release_target_triple(os: &str, arch: &str, musl: bool) -> Result<&'static str> {
    match (os, arch, musl) {
        ("macos", "aarch64", _) => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64", _) => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64", false) => Ok("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64", false) => Ok("x86_64-unknown-linux-gnu"),
        (os, arch, musl) => Err(VaultError::Validation(format!(
            "unsupported platform for prebuilt self-update: {os}/{arch}{}",
            if musl { " (musl)" } else { "" }
        ))),
    }
}

/// Parse a `checksums.sha256` manifest and verify `bytes` matches the expected digest for `archive_name`.
pub fn verify_sha256_manifest(manifest: &str, archive_name: &str, bytes: &[u8]) -> Result<()> {
    let mut expected_hex: Option<&str> = None;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(digest) = parts.next() else {
            continue;
        };
        let Some(raw_name) = parts.next() else {
            continue;
        };
        let normalized_name = raw_name.trim_start_matches('*').trim_start_matches("./");
        if normalized_name == archive_name {
            expected_hex = Some(digest);
            break;
        }
    }

    let expected = expected_hex.ok_or_else(|| {
        VaultError::Validation(format!(
            "checksums.sha256 does not contain an entry for '{archive_name}'"
        ))
    })?;

    let actual = sha256_hex(bytes);

    if !actual.eq_ignore_ascii_case(expected) {
        return Err(VaultError::Validation(format!(
            "SHA-256 checksum mismatch for '{archive_name}': expected {expected}, got {actual}"
        )));
    }

    Ok(())
}

/// Safely extract the `citadel` binary from a `.tar.gz` archive into `dest`.
pub fn extract_binary_from_tar_gz(archive_bytes: &[u8], dest: &Path) -> Result<()> {
    let decoder = GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);

    for entry_res in archive.entries()? {
        let mut entry = entry_res?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let entry_path = entry.path()?;
        for comp in entry_path.components() {
            if matches!(
                comp,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            ) {
                return Err(VaultError::Validation(format!(
                    "unsafe path in release archive: {}",
                    entry_path.display()
                )));
            }
        }

        let is_citadel_bin = entry_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name == "citadel");

        if is_citadel_bin {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out_file = std::fs::File::create(dest)?;
            std::io::copy(&mut entry, &mut out_file)?;
            out_file.sync_all()?;
            return Ok(());
        }
    }

    Err(VaultError::Validation(
        "release archive does not contain a `citadel` binary".to_string(),
    ))
}

/// Atomically replace `current_bin` with `new_bin`, preserving executable permissions.
///
/// `new_bin` must live on the same filesystem as `current_bin`. A single `rename` over
/// the target is atomic on POSIX: there is never a moment without a binary at
/// `current_bin`, and running processes keep the old inode.
pub fn atomic_replace_binary(new_bin: &Path, current_bin: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(current_bin)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o755);
        let exec_mode = if mode & 0o111 == 0 { 0o755 } else { mode };
        std::fs::set_permissions(new_bin, std::fs::Permissions::from_mode(exec_mode))?;
    }

    std::fs::rename(new_bin, current_bin)?;
    Ok(())
}

/// Note to show before replacing a binary whose Cargo features differ from release binaries,
/// which are built with the default features (including `chrome`).
fn feature_build_warning(built_with_chrome: bool) -> Option<&'static str> {
    (!built_with_chrome).then_some(
        "this citadel is a lite build without the `chrome` feature, but release binaries \
         include it: after the update, Sources added with --render-js use JavaScript rendering \
         (Chrome or Chromium required). To stay lite, skip this update and run \
         `make install-lite` from a source checkout instead.",
    )
}

/// Time allowed to open a connection for an update request.
const UPDATE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest pause between received bytes before an update request counts as stalled.
const UPDATE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP client for `citadel update` requests.
///
/// Uses a connect timeout and a read (inactivity) timeout rather than a total deadline:
/// the release tarball is several MB, and a slow but steady link must still finish.
fn update_http_client(user_agent: &str, read_timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(UPDATE_CONNECT_TIMEOUT)
        .read_timeout(read_timeout)
        .user_agent(user_agent)
        .build()?)
}

/// Execute `citadel update`.
pub async fn run_update<W: Write>(opts: UpdateOpts, out: &mut W) -> Result<()> {
    let current = normalize_version_str(&opts.current_version).to_string();
    let in_container = opts.force_container.unwrap_or_else(is_running_in_container);

    if in_container {
        return print_container_upgrade_guidance(&opts, &current, out).await;
    }

    let client = update_http_client(&format!("citadel/{current}"), UPDATE_READ_TIMEOUT)?;

    let (release, latest) = fetch_release(&client, &opts.releases_api_url).await?;
    let _ = write_cache_atomic(
        &opts.config.update_check_path(),
        &UpdateCheckCache::now(latest.clone(), release.html_url.clone()),
    );

    if !is_newer_version(&current, &latest) {
        writeln!(out, "citadel v{current} is already up to date")?;
        return Ok(());
    }

    if opts.check_only {
        writeln!(
            out,
            "Update available: v{current} -> v{latest} ({})",
            release.html_url
        )?;
        return Ok(());
    }

    if let Some(warning) = feature_build_warning(cfg!(feature = "chrome")) {
        writeln!(out, "Warning: {warning}")?;
    }

    let is_tty = opts
        .stdin_is_tty
        .unwrap_or_else(|| std::io::stdin().is_terminal());
    if !opts.yes {
        if !is_tty {
            return Err(VaultError::Validation(
                "refusing to upgrade without confirmation on non-interactive stdin; pass --yes to proceed"
                    .to_string(),
            ));
        }
        write!(out, "Upgrade citadel from v{current} to v{latest}? [y/N]: ")?;
        out.flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        let trimmed = answer.trim().to_ascii_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            writeln!(out, "Aborted.")?;
            return Ok(());
        }
    }

    let triple = target_triple()?;
    let archive_name = format!("citadel-v{latest}-{triple}.tar.gz");

    let archive_asset = release
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .ok_or_else(|| {
            VaultError::NotFound(format!(
                "release v{latest} does not include asset '{archive_name}'"
            ))
        })?;
    let checksums_asset = release
        .assets
        .iter()
        .find(|a| a.name == "checksums.sha256")
        .ok_or_else(|| {
            VaultError::NotFound(format!(
                "release v{latest} does not include 'checksums.sha256'"
            ))
        })?;

    let (manifest, archive_bytes) = tokio::try_join!(
        async {
            client
                .get(&checksums_asset.browser_download_url)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await
        },
        async {
            client
                .get(&archive_asset.browser_download_url)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await
        },
    )?;

    verify_sha256_manifest(&manifest, &archive_name, &archive_bytes)?;

    // Follow symlinks (Homebrew, `~/.local/bin`) so the real binary is replaced and the
    // temp file lands on the same filesystem as it.
    let install_path = std::fs::canonicalize(&opts.current_exe)?;
    let parent_dir = install_path.parent().ok_or_else(|| {
        VaultError::Config(format!(
            "cannot determine parent directory for executable {}",
            install_path.display()
        ))
    })?;
    let temp_bin = parent_dir.join(format!(".citadel.update.{}.tmp", std::process::id()));

    let installed = extract_binary_from_tar_gz(&archive_bytes, &temp_bin)
        .and_then(|()| atomic_replace_binary(&temp_bin, &install_path));
    if installed.is_err() {
        let _ = std::fs::remove_file(&temp_bin);
    }
    installed?;

    writeln!(out, "Updated citadel from v{current} to v{latest}.")?;

    // Restart running native daemon if active.
    if let Ok(DaemonStatus::Running(old_pid)) = crate::daemon::daemon_status(&opts.config) {
        // Decided before the stop: systemd reports a stopped service as inactive.
        let supervised = service_supervises(
            &crate::doctor::running::gather_service_state(),
            &opts.config.data_dir,
        );
        restart_daemon_after_update(
            out,
            old_pid,
            supervised,
            || crate::daemon::stop_daemon(&opts.config),
            || daemon_status_after_grace(&opts.config, DAEMON_RELAUNCH_GRACE),
            || crate::daemon::start_daemon_with_exe(&opts.config, &opts.current_exe),
            || start_service_and_wait(&opts.config),
        )?;
    }

    Ok(())
}

/// How long container mode waits for the release server (container DNS is often unreliable)
/// before falling back to the cached release.
const CONTAINER_RELEASE_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// The newest release known inside a container.
struct KnownRelease {
    /// SemVer version, without the leading `v`.
    version: String,
    /// Git tag to check out to build this release.
    tag: String,
    /// Comes from a live check or a fresh cache, not from a stale cache.
    confirmed: bool,
}

/// Latest release for container mode: a short live check, else the cached one.
async fn latest_release_for_container(opts: &UpdateOpts, current: &str) -> Option<KnownRelease> {
    let live_check = async {
        let client = update_http_client(&format!("citadel/{current}"), UPDATE_READ_TIMEOUT)?;
        fetch_release(&client, &opts.releases_api_url).await
    };
    if let Ok(Ok((release, latest))) =
        tokio::time::timeout(CONTAINER_RELEASE_CHECK_TIMEOUT, live_check).await
    {
        let _ = write_cache_atomic(
            &opts.config.update_check_path(),
            &UpdateCheckCache::now(latest.clone(), release.html_url),
        );
        return Some(KnownRelease {
            version: latest,
            tag: release.tag_name.trim().to_string(),
            confirmed: true,
        });
    }

    let (cached, is_stale) = read_cached_status(
        &opts.config.update_check_path(),
        current,
        current_unix_secs(),
    );
    cached.latest_version.map(|version| KnownRelease {
        tag: format!("v{version}"),
        version,
        confirmed: !is_stale,
    })
}

/// Container mode never replaces the binary: report the latest release and how to rebuild
/// the image from its source and recreate the container.
async fn print_container_upgrade_guidance<W: Write>(
    opts: &UpdateOpts,
    current: &str,
    out: &mut W,
) -> Result<()> {
    writeln!(out, "Running inside a container (citadel v{current}).")?;
    let known = latest_release_for_container(opts, current).await;
    let newer = known
        .as_ref()
        .filter(|release| is_newer_version(current, &release.version));
    match &known {
        Some(release) if release.confirmed && newer.is_none() => {
            writeln!(out, "citadel v{current} is already up to date")?;
            return Ok(());
        }
        Some(release) if release.confirmed => {
            writeln!(out, "Latest release: v{}", release.version)?;
        }
        Some(release) => writeln!(
            out,
            "Latest known release: v{} (could not check for a newer one)",
            release.version
        )?,
        None => writeln!(
            out,
            "Latest release: unknown (could not check the release server)"
        )?,
    }
    let checkout_tag = newer.map_or("<latest release tag>", |release| release.tag.as_str());

    write_container_rebuild_steps(
        out,
        container_cli(
            Path::new("/.dockerenv").exists(),
            Path::new("/run/.containerenv").exists(),
        ),
        checkout_tag,
    )?;
    Ok(())
}

/// CLI of the container runtime Citadel runs under: Docker creates `/.dockerenv`, Podman
/// creates `/run/.containerenv`, Apple's `container` creates no marker file (it is detected
/// through `CITADEL_CONTAINER=1`).
fn container_cli(dockerenv_exists: bool, containerenv_exists: bool) -> &'static str {
    if dockerenv_exists {
        "docker"
    } else if containerenv_exists {
        "podman"
    } else {
        "container"
    }
}

/// Steps to rebuild and recreate the container from the release source on the host.
fn write_container_rebuild_steps<W: Write>(
    out: &mut W,
    cli: &str,
    checkout_tag: &str,
) -> Result<()> {
    writeln!(
        out,
        "Self-updating in place is disabled inside containers. To upgrade, check out the release \
         in your Citadel source checkout on the host, then rebuild and recreate the container \
         with `{cli}`:"
    )?;
    writeln!(out, "  git fetch --tags && git checkout {checkout_tag}")?;
    writeln!(out, "  {cli} build --pull -t citadel:latest .")?;
    writeln!(out, "  {cli} stop citadel && {cli} rm citadel")?;
    writeln!(
        out,
        "  {cli} run -d --name citadel -p 127.0.0.1:4317:4317 -e CITADEL_HOME=/data \
         -e CITADEL_CONTAINER=1 -v citadel-data:/data citadel:latest"
    )?;
    writeln!(
        out,
        "Add any other options the container was started with (such as `--dns 1.1.1.1` or \
         `-v <folder>:/sources/books:ro`), or replace the `{cli}` commands with \
         `make build && make run`, passing the same FEATURES and BOOKS_DIR (and RUNTIME=container \
         for Apple's runtime)."
    )?;
    Ok(())
}

/// Whether the installed OS service supervises the daemon of the vault at `data_dir`: it is
/// loaded (so launchd or systemd relaunches it) and its `CITADEL_HOME` is that vault.
fn service_supervises(state: &ServiceState, data_dir: &Path) -> bool {
    matches!(
        state,
        ServiceState::Installed { file, loaded: true }
            if file
                .data_dir
                .as_deref()
                .is_some_and(|dir| crate::fsutil::same_path(dir, data_dir))
    )
}

/// How long a stopped daemon is given to be relaunched by its service manager (launchd's
/// `KeepAlive`) before `citadel update` starts the new binary itself.
const DAEMON_RELAUNCH_GRACE: Duration = Duration::from_secs(1);

/// How long the service manager is given to start the daemon before the update reports failure.
const SERVICE_START_GRACE: Duration = Duration::from_secs(10);

/// The daemon status as soon as a daemon holds the lock, or once `grace` has passed.
fn daemon_status_after_grace(config: &VaultConfig, grace: Duration) -> Result<DaemonStatus> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        let status = crate::daemon::daemon_status(config)?;
        if matches!(status, DaemonStatus::Running(_)) || std::time::Instant::now() >= deadline {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ask the OS service manager to start the daemon, then wait until it holds the daemon lock.
fn start_service_and_wait(config: &VaultConfig) -> Result<u32> {
    crate::daemon::start_service()?;
    match daemon_status_after_grace(config, SERVICE_START_GRACE)? {
        DaemonStatus::Running(pid) => Ok(pid),
        DaemonStatus::Stopped | DaemonStatus::Stale => Err(VaultError::Other(format!(
            "the service manager did not start the daemon within {} seconds; see {}",
            SERVICE_START_GRACE.as_secs(),
            config.log_path().display()
        ))),
    }
}

/// Stop the running daemon (`old_pid`) and start it on the new binary, reporting the outcome to `out`.
///
/// Who holds the daemon lock after the stop decides the next step. Nobody: start the new
/// binary, through `start_service` when an OS service supervises the daemon (`supervised`),
/// else with `spawn`. A daemon spawned beside a service leaves launchd or systemd failing on
/// the lock while it runs unsupervised. Another pid: a service manager already relaunched the
/// daemon, and a second one would only fight it for the lock. Still `old_pid`: the old daemon
/// has not exited (a SIGTERMed daemon first finishes its in-flight Syncs), so nothing
/// restarts it and the user is told how to restart it once it has stopped.
fn restart_daemon_after_update<W: Write>(
    out: &mut W,
    old_pid: u32,
    supervised: bool,
    stop: impl FnOnce() -> Result<Option<u32>>,
    status_after_stop: impl FnOnce() -> Result<DaemonStatus>,
    spawn: impl FnOnce() -> Result<Option<u32>>,
    start_service: impl FnOnce() -> Result<u32>,
) -> Result<()> {
    let stop_error = stop().err();
    match status_after_stop() {
        Ok(DaemonStatus::Running(pid)) if pid == old_pid => {
            let reason = stop_error
                .map(|err| format!(" ({err})"))
                .unwrap_or_default();
            let next_step = if supervised {
                "then run `citadel daemon install`, which restarts it through its service on \
                 the new version."
                    .to_string()
            } else {
                "when `citadel daemon status` reports it stopped, run `citadel daemon start` \
                 (if it keeps running, run `citadel daemon stop` first)."
                    .to_string()
            };
            writeln!(
                out,
                "Warning: the daemon has not stopped yet{reason}, so it was not restarted and \
                 still runs the old version. A daemon finishing a Sync exits once the Sync is \
                 done: {next_step}"
            )?;
        }
        Ok(DaemonStatus::Running(pid)) => writeln!(
            out,
            "The daemon is already running again (pid {pid}), most likely relaunched by its \
             service manager, so no second one was started."
        )?,
        Ok(DaemonStatus::Stopped | DaemonStatus::Stale) if supervised => match start_service() {
            Ok(pid) => writeln!(
                out,
                "Restarted citadel daemon through its OS service (pid {pid})."
            )?,
            Err(err) => writeln!(
                out,
                "Warning: could not restart the daemon through its OS service: {err}. Run \
                 `citadel doctor`; `citadel daemon install` re-registers the service."
            )?,
        },
        Ok(DaemonStatus::Stopped | DaemonStatus::Stale) => match spawn() {
            Ok(Some(pid)) => writeln!(out, "Restarted citadel daemon (pid {pid}).")?,
            Ok(None) => {}
            Err(err) => writeln!(
                out,
                "Warning: failed to restart daemon: {err}. Start it with `citadel daemon start`."
            )?,
        },
        Err(err) => {
            let start_command = if supervised {
                "citadel daemon install"
            } else {
                "citadel daemon start"
            };
            writeln!(
                out,
                "Warning: could not check whether the daemon stopped ({err}), so it was not \
                 restarted. Run `citadel daemon status`, then `{start_command}` if it is stopped."
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::VaultError;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tempfile::tempdir;

    fn build_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, *name, *data)
                .expect("append tar entry");
        }
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip")
    }

    /// Serve one HTTP response whose body arrives in `chunks`, each sent after `gap`.
    async fn spawn_trickle_server(chunks: Vec<&'static [u8]>, gap: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind trickle server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 4096];
            let _ = sock.read(&mut request).await;
            let len: usize = chunks.iter().map(|chunk| chunk.len()).sum();
            let head =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n");
            sock.write_all(head.as_bytes()).await.expect("write head");
            for chunk in chunks {
                tokio::time::sleep(gap).await;
                sock.write_all(chunk).await.expect("write chunk");
            }
        });
        format!("http://{addr}/citadel.tar.gz")
    }

    #[tokio::test]
    async fn update_client_lets_a_slow_but_steady_download_finish() {
        // Every pause (300 ms) is under the 500 ms timeout, but the whole body takes about
        // 900 ms: a total deadline would abort a download that keeps making progress.
        let url = spawn_trickle_server(vec![b"ab", b"cd", b"ef"], Duration::from_millis(300)).await;
        let client =
            update_http_client("citadel-test", Duration::from_millis(500)).expect("build client");
        let body = client
            .get(&url)
            .send()
            .await
            .expect("send")
            .bytes()
            .await
            .expect("a slow download that keeps making progress must finish");
        assert_eq!(&body[..], b"abcdef");
    }

    #[test]
    fn updating_a_lite_build_says_the_release_adds_js_rendering() {
        assert_eq!(feature_build_warning(true), None);
        let warning = feature_build_warning(false).expect("a lite build must be told");
        assert!(warning.contains("JavaScript rendering"), "{warning}");
        assert!(warning.contains("make install-lite"), "{warning}");
    }

    #[test]
    fn musl_builds_have_no_release_tarball() {
        assert_eq!(
            release_target_triple("linux", "x86_64", false).expect("gnu x86_64"),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            release_target_triple("macos", "aarch64", false).expect("apple silicon"),
            "aarch64-apple-darwin"
        );
        // Releases only ship glibc Linux binaries, which do not run on a musl host.
        for arch in ["x86_64", "aarch64"] {
            let err = release_target_triple("linux", arch, true)
                .expect_err("a musl build must not download the glibc tarball");
            assert!(matches!(err, VaultError::Validation(_)), "{err}");
        }
    }

    #[test]
    fn sha256_manifest_accepts_valid_digest_and_rejects_tampered_bytes() {
        let payload = b"fake-tarball-bytes";
        let digest = sha256_hex(payload);
        let manifest = format!(
            "0000000000000000000000000000000000000000000000000000000000000000  other.tar.gz\n{digest}  citadel-v0.2.0-aarch64-apple-darwin.tar.gz\n"
        );

        verify_sha256_manifest(
            &manifest,
            "citadel-v0.2.0-aarch64-apple-darwin.tar.gz",
            payload,
        )
        .expect("valid digest must pass");

        let err = verify_sha256_manifest(
            &manifest,
            "citadel-v0.2.0-aarch64-apple-darwin.tar.gz",
            b"tampered-bytes",
        )
        .expect_err("tampered bytes must fail");
        assert!(matches!(err, VaultError::Validation(_)));
    }

    #[test]
    fn extract_tar_gz_extracts_citadel_binary_and_rejects_missing_binary() {
        let dir = tempdir().expect("tempdir");
        let dest = dir.path().join("citadel.new");

        let valid_archive =
            build_tar_gz(&[("citadel-v0.2.0/citadel", b"#!/bin/sh\necho v0.2.0\n")]);
        extract_binary_from_tar_gz(&valid_archive, &dest).expect("extract should succeed");
        assert_eq!(
            std::fs::read(&dest).expect("read extracted"),
            b"#!/bin/sh\necho v0.2.0\n"
        );

        let missing_archive = build_tar_gz(&[("README.md", b"# Hello\n")]);
        let err = extract_binary_from_tar_gz(&missing_archive, &dir.path().join("missing"))
            .expect_err("archive without citadel binary must fail");
        assert!(matches!(err, VaultError::Validation(_)));
    }

    #[test]
    fn atomic_replace_swaps_binary_and_preserves_exec_mode() {
        let dir = tempdir().expect("tempdir");
        let current = dir.path().join("citadel");
        let replacement = dir.path().join("citadel.new");

        std::fs::write(&current, b"old-binary").expect("write old");
        std::fs::write(&replacement, b"new-binary").expect("write new");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o755))
                .expect("chmod old");
        }

        atomic_replace_binary(&replacement, &current).expect("atomic replace should succeed");
        assert_eq!(
            std::fs::read(&current).expect("read swapped"),
            b"new-binary"
        );
        assert!(!replacement.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&current)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn atomic_replace_does_not_depend_on_a_backup_rename() {
        // The swap must be a single rename over the target, so there is never a moment
        // without a binary at `current`. Occupy the old backup path to prove no
        // rename-to-backup step is attempted.
        let dir = tempdir().expect("tempdir");
        let current = dir.path().join("citadel");
        let replacement = dir.path().join("citadel.new");
        std::fs::write(&current, b"old-binary").expect("write old");
        std::fs::write(&replacement, b"new-binary").expect("write new");
        let backup = dir
            .path()
            .join(format!(".citadel.bak.{}", std::process::id()));
        std::fs::create_dir(&backup).expect("occupy backup path");
        std::fs::write(backup.join("keep"), b"x").expect("make backup dir non-empty");

        atomic_replace_binary(&replacement, &current).expect("swap must not need a backup");
        assert_eq!(std::fs::read(&current).expect("read"), b"new-binary");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn update_through_symlink_replaces_target_and_keeps_symlink() {
        let (addr, server) = spawn_mock_release_server().await;

        let dir = tempdir().expect("tempdir");
        let real_dir = dir.path().join("cellar");
        let link_dir = dir.path().join("bin");
        std::fs::create_dir_all(&real_dir).expect("mkdir real");
        std::fs::create_dir_all(&link_dir).expect("mkdir link");
        let real_bin = real_dir.join("citadel");
        let link_bin = link_dir.join("citadel");
        std::fs::write(&real_bin, b"old-v0.1.0-binary").expect("write old bin");
        std::os::unix::fs::symlink(&real_bin, &link_bin).expect("symlink");

        let mut out = Vec::new();
        run_update(
            UpdateOpts {
                config: VaultConfig::new(dir.path().join("data")),
                check_only: false,
                yes: true,
                current_version: "0.1.0".to_string(),
                current_exe: link_bin.clone(),
                releases_api_url: format!("http://{addr}/releases/latest"),
                force_container: Some(false),
                stdin_is_tty: Some(false),
            },
            &mut out,
        )
        .await
        .expect("update through symlink should succeed");
        server.abort();

        assert!(
            std::fs::symlink_metadata(&link_bin)
                .expect("link metadata")
                .file_type()
                .is_symlink(),
            "the PATH symlink must survive the update"
        );
        assert_eq!(
            std::fs::read(&real_bin).expect("read real bin"),
            b"upgraded-v0.2.0-binary"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&link_dir)
            .expect("read link dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("citadel")]);
    }

    const STOP_TIMEOUT: &str =
        "sent SIGTERM to daemon (pid 7), but it did not exit within 5 seconds";

    #[test]
    fn daemon_restart_explains_a_daemon_still_finishing_a_sync() {
        let mut out = Vec::new();
        let mut started = false;
        restart_daemon_after_update(
            &mut out,
            7,
            false,
            || Err(VaultError::Other(STOP_TIMEOUT.into())),
            || Ok(DaemonStatus::Running(7)),
            || {
                started = true;
                Ok(Some(42))
            },
            || unreachable!("no service supervises this daemon"),
        )
        .expect("restart reporting succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !started,
            "must not try to start while the old daemon is still running"
        );
        assert!(text.contains(STOP_TIMEOUT), "{text}");
        assert!(text.contains("has not stopped yet"), "{text}");
        assert!(
            text.contains("`citadel daemon status`") && text.contains("`citadel daemon start`"),
            "{text}"
        );
        assert!(
            !text.contains("citadel daemon stop && citadel daemon start"),
            "a second stop times out the same way while the Sync runs: {text}"
        );
    }

    #[test]
    fn daemon_restart_leaves_a_daemon_relaunched_by_its_service_manager_alone() {
        // launchd (`KeepAlive`) relaunches the job as soon as the old daemon exits, whether or
        // not the stop poll saw the gap in between.
        for supervised in [false, true] {
            for stop_result in [Ok(Some(7)), Err(VaultError::Other(STOP_TIMEOUT.into()))] {
                let mut out = Vec::new();
                let mut started = false;
                restart_daemon_after_update(
                    &mut out,
                    7,
                    supervised,
                    move || stop_result,
                    || Ok(DaemonStatus::Running(99)),
                    || {
                        started = true;
                        Ok(Some(42))
                    },
                    || unreachable!("the service manager already relaunched it"),
                )
                .expect("restart reporting succeeds");
                let text = String::from_utf8(out).expect("utf8");
                assert!(
                    !started,
                    "a second daemon would fight the relaunched one for the lock"
                );
                assert!(text.contains("already running again (pid 99)"), "{text}");
                assert!(!text.contains("Warning"), "{text}");
            }
        }
    }

    #[test]
    fn daemon_restart_starts_the_new_binary_once_the_old_daemon_is_gone() {
        // A clean stop, and a stop that timed out just before the daemon exited.
        for (stop_result, status) in [
            (Ok(Some(7)), DaemonStatus::Stopped),
            (
                Err(VaultError::Other(STOP_TIMEOUT.into())),
                DaemonStatus::Stale,
            ),
        ] {
            let mut out = Vec::new();
            restart_daemon_after_update(
                &mut out,
                7,
                false,
                move || stop_result,
                move || Ok(status),
                || Ok(Some(42)),
                || unreachable!("no service supervises this daemon"),
            )
            .expect("restart reporting succeeds");
            let text = String::from_utf8(out).expect("utf8");
            assert_eq!(text, "Restarted citadel daemon (pid 42).\n");
        }
    }

    #[test]
    fn daemon_restart_uses_the_service_manager_when_a_service_supervises_the_daemon() {
        // launchd throttles its relaunch (~10 s), so the lock is still free after the grace
        // period: the update must hand the start to the service manager, not spawn its own copy.
        for (stop_result, status) in [
            (Ok(Some(7)), DaemonStatus::Stopped),
            (
                Err(VaultError::Other(STOP_TIMEOUT.into())),
                DaemonStatus::Stale,
            ),
        ] {
            let mut out = Vec::new();
            let mut spawned = false;
            let mut service_started = false;
            restart_daemon_after_update(
                &mut out,
                7,
                true,
                move || stop_result,
                move || Ok(status),
                || {
                    spawned = true;
                    Ok(Some(42))
                },
                || {
                    service_started = true;
                    Ok(43)
                },
            )
            .expect("restart reporting succeeds");
            let text = String::from_utf8(out).expect("utf8");
            assert!(
                !spawned,
                "a detached daemon would leave the service manager failing on the lock"
            );
            assert!(service_started, "{text}");
            assert_eq!(
                text,
                "Restarted citadel daemon through its OS service (pid 43).\n"
            );
        }
    }

    #[test]
    fn daemon_restart_never_spawns_when_the_service_fails_to_start() {
        let mut out = Vec::new();
        let mut spawned = false;
        restart_daemon_after_update(
            &mut out,
            7,
            true,
            || Ok(Some(7)),
            || Ok(DaemonStatus::Stopped),
            || {
                spawned = true;
                Ok(Some(42))
            },
            || Err(VaultError::Other("launchctl kickstart failed".into())),
        )
        .expect("restart reporting succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !spawned,
            "falling back to a detached daemon recreates the unsupervised daemon"
        );
        assert!(text.starts_with("Warning:"), "{text}");
        assert!(text.contains("launchctl kickstart failed"), "{text}");
        assert!(text.contains("`citadel daemon install`"), "{text}");
    }

    #[test]
    fn daemon_restart_points_a_supervised_daemon_still_syncing_at_its_service() {
        let mut out = Vec::new();
        let mut spawned = false;
        let mut service_started = false;
        restart_daemon_after_update(
            &mut out,
            7,
            true,
            || Err(VaultError::Other(STOP_TIMEOUT.into())),
            || Ok(DaemonStatus::Running(7)),
            || {
                spawned = true;
                Ok(Some(42))
            },
            || {
                service_started = true;
                Ok(43)
            },
        )
        .expect("restart reporting succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !spawned && !service_started,
            "must not try to start while the old daemon is still running"
        );
        assert!(text.contains("has not stopped yet"), "{text}");
        assert!(
            text.contains("`citadel daemon install`"),
            "re-registering starts the new binary even when launchd refuses to spawn it: {text}"
        );
        assert!(
            !text.contains("`citadel daemon start`"),
            "a plain start would run a daemon outside the service: {text}"
        );
    }

    #[test]
    fn daemon_restart_status_error_points_a_supervised_daemon_at_its_service() {
        let mut out = Vec::new();
        restart_daemon_after_update(
            &mut out,
            7,
            true,
            || Ok(Some(7)),
            || Err(VaultError::Other("permission denied".into())),
            || unreachable!("the daemon state is unknown"),
            || unreachable!("the daemon state is unknown"),
        )
        .expect("restart reporting succeeds");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("permission denied"), "{text}");
        assert!(
            text.contains("`citadel daemon install`"),
            "re-registering starts the new binary even when launchd refuses to spawn it: {text}"
        );
        assert!(
            !text.contains("`citadel daemon start`"),
            "a plain start would run a daemon outside the service: {text}"
        );
    }

    /// An installed service whose file sets `CITADEL_HOME` to `data_dir`.
    fn installed_service(data_dir: Option<&Path>, loaded: bool) -> ServiceState {
        ServiceState::Installed {
            file: crate::doctor::running::ServiceFile {
                path: PathBuf::from("/Library/LaunchAgents/com.citadel.daemon.plist"),
                binary: Some(PathBuf::from("/usr/local/bin/citadel")),
                data_dir: data_dir.map(Path::to_path_buf),
                unreadable: None,
            },
            loaded,
        }
    }

    #[test]
    fn only_a_loaded_service_for_this_vault_supervises_its_daemon() {
        let dir = tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&vault).expect("mkdir vault");
        std::fs::create_dir_all(&other).expect("mkdir other");

        assert!(service_supervises(
            &installed_service(Some(&vault), true),
            &vault
        ));
        assert!(
            !service_supervises(&installed_service(Some(&vault), false), &vault),
            "an unloaded service relaunches nothing"
        );
        assert!(
            !service_supervises(&installed_service(Some(&other), true), &vault),
            "a service for another vault must not be restarted instead"
        );
        assert!(
            !service_supervises(&installed_service(None, true), &vault),
            "an unreadable service file names no vault"
        );
        for state in [
            ServiceState::NotInstalled,
            ServiceState::Unsupported,
            ServiceState::Unknown {
                reason: "HOME is not set".into(),
            },
        ] {
            assert!(!service_supervises(&state, &vault), "{state:?}");
        }

        #[cfg(unix)]
        {
            let link = dir.path().join("vault-link");
            std::os::unix::fs::symlink(&vault, &link).expect("symlink");
            assert!(
                service_supervises(&installed_service(Some(&vault), true), &link),
                "the same vault reached through a symlink"
            );
        }
    }

    #[test]
    fn status_after_grace_sees_a_daemon_relaunched_during_the_grace_period() {
        let dir = tempdir().expect("tempdir");
        let config = VaultConfig::new(dir.path());
        let pid_path = config.pid_path();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let relauncher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            // Retry in case a status probe holds the lock for an instant.
            let mut lock = None;
            for _ in 0..50 {
                if let Ok(acquired) = crate::daemon::PidLock::acquire(&pid_path) {
                    lock = Some(acquired);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(lock.is_some(), "relaunched daemon takes the lock");
            let _ = release_rx.recv();
        });

        let polling_since = std::time::Instant::now();
        let status = daemon_status_after_grace(&config, Duration::from_secs(10)).expect("status");
        assert_eq!(status, DaemonStatus::Running(std::process::id()));
        assert!(
            polling_since.elapsed() < Duration::from_secs(5),
            "returns as soon as the relaunched daemon holds the lock"
        );
        release_tx.send(()).expect("release the lock");
        relauncher.join().expect("relauncher thread");
    }

    #[tokio::test]
    async fn container_mode_prints_guidance_without_mutating_binary() {
        let dir = tempdir().expect("tempdir");
        let fake_bin = dir.path().join("citadel");
        std::fs::write(&fake_bin, b"container-binary").expect("write fake bin");

        let mut out = Vec::new();
        run_update(
            UpdateOpts {
                config: VaultConfig::new(dir.path()),
                check_only: false,
                yes: true,
                current_version: "0.1.0".to_string(),
                current_exe: fake_bin.clone(),
                releases_api_url: "http://127.0.0.1:9/unused".to_string(),
                force_container: Some(true),
                stdin_is_tty: Some(false),
            },
            &mut out,
        )
        .await
        .expect("container mode should exit cleanly");

        let text = String::from_utf8(out).expect("utf-8 output");
        assert!(
            text.contains("container "),
            "expected Apple container guidance: {text}"
        );
        assert!(
            !text.contains("docker "),
            "must never recommend docker: {text}"
        );
        assert_eq!(
            std::fs::read(&fake_bin).expect("read fake bin"),
            b"container-binary"
        );
    }

    /// Serve only the release JSON for `tag` at `/releases/latest`.
    async fn spawn_release_json_server(
        tag: &'static str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let app = axum::Router::new().route(
            "/releases/latest",
            axum::routing::get(move || async move {
                axum::Json(serde_json::json!({
                    "tag_name": tag,
                    "html_url": format!("https://github.com/MaximeBeretvas/citadel/releases/tag/{tag}"),
                    "assets": []
                }))
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, server)
    }

    /// Run `citadel update --yes` for v0.1.0 in container mode and return what it printed,
    /// checking that the binary was left untouched.
    async fn container_update_output(data_dir: &Path, api_url: &str) -> String {
        let fake_bin = data_dir.join("citadel");
        std::fs::write(&fake_bin, b"container-binary").expect("write fake bin");
        let mut out = Vec::new();
        run_update(
            UpdateOpts {
                config: VaultConfig::new(data_dir),
                check_only: false,
                yes: true,
                current_version: "0.1.0".to_string(),
                current_exe: fake_bin.clone(),
                releases_api_url: api_url.to_string(),
                force_container: Some(true),
                stdin_is_tty: Some(false),
            },
            &mut out,
        )
        .await
        .expect("container mode should exit cleanly");
        assert_eq!(
            std::fs::read(&fake_bin).expect("read fake bin"),
            b"container-binary"
        );
        String::from_utf8(out).expect("utf-8 output")
    }

    #[tokio::test]
    async fn container_mode_names_the_release_to_build() {
        let (addr, server) = spawn_release_json_server("v0.2.0").await;
        let dir = tempdir().expect("tempdir");
        let text =
            container_update_output(dir.path(), &format!("http://{addr}/releases/latest")).await;
        server.abort();

        assert!(text.contains("Latest release: v0.2.0"), "{text}");
        assert!(
            text.contains("git fetch --tags && git checkout v0.2.0"),
            "the image must be rebuilt from the release source: {text}"
        );
        assert!(text.contains("-e CITADEL_CONTAINER=1"), "{text}");
        assert!(text.contains("make build && make run"), "{text}");
    }

    #[test]
    fn rebuild_steps_name_the_cli_of_the_running_container_runtime() {
        assert_eq!(container_cli(true, false), "docker");
        assert_eq!(container_cli(false, true), "podman");
        assert_eq!(container_cli(false, false), "container");

        let mut out = Vec::new();
        write_container_rebuild_steps(&mut out, "docker", "v0.2.0").expect("write steps");
        let text = String::from_utf8(out).expect("utf-8");
        assert!(
            text.contains("git fetch --tags && git checkout v0.2.0"),
            "{text}"
        );
        assert!(
            text.contains("docker build --pull -t citadel:latest ."),
            "{text}"
        );
        assert!(
            text.contains("docker stop citadel && docker rm citadel"),
            "{text}"
        );
        assert!(text.contains("docker run -d --name citadel"), "{text}");
        assert!(text.contains("make build && make run"), "{text}");
        assert!(!text.contains("container build"), "{text}");
    }

    #[tokio::test]
    async fn container_mode_says_up_to_date_without_rebuild_steps() {
        let (addr, server) = spawn_release_json_server("v0.1.0").await;
        let dir = tempdir().expect("tempdir");
        let text =
            container_update_output(dir.path(), &format!("http://{addr}/releases/latest")).await;
        server.abort();

        assert!(
            text.contains("citadel v0.1.0 is already up to date"),
            "{text}"
        );
        assert!(!text.contains("container build"), "{text}");
    }

    #[tokio::test]
    async fn container_mode_falls_back_to_a_fresh_cache_when_offline() {
        let dir = tempdir().expect("tempdir");
        write_cache_atomic(
            &VaultConfig::new(dir.path()).update_check_path(),
            &UpdateCheckCache::now("0.2.0".into(), "https://example.invalid/v0.2.0".into()),
        )
        .expect("seed fresh cache");
        let text = container_update_output(dir.path(), "http://127.0.0.1:9/unused").await;

        assert!(text.contains("v0.2.0"), "{text}");
        assert!(text.contains("git checkout v0.2.0"), "{text}");
    }

    #[tokio::test]
    async fn container_mode_does_not_claim_up_to_date_from_a_stale_cache() {
        let dir = tempdir().expect("tempdir");
        let stale = UpdateCheckCache {
            checked_at: "2000-01-01T00:00:00Z".into(),
            latest_version: "0.1.0".into(),
            release_url: "https://example.invalid/v0.1.0".into(),
        };
        write_cache_atomic(&VaultConfig::new(dir.path()).update_check_path(), &stale)
            .expect("seed stale cache");
        let text = container_update_output(dir.path(), "http://127.0.0.1:9/unused").await;

        assert!(!text.contains("already up to date"), "{text}");
        assert!(text.contains("container build"), "{text}");
    }

    /// Serve a fake `v0.2.0` release whose archive holds `upgraded-v0.2.0-binary`.
    async fn spawn_mock_release_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let triple = target_triple().expect("supported test platform");
        let archive_name = format!("citadel-v0.2.0-{triple}.tar.gz");
        let archive_bytes = build_tar_gz(&[("citadel", b"upgraded-v0.2.0-binary")]);
        let checksum_line = format!("{}  {}\n", sha256_hex(&archive_bytes), archive_name);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");

        let archive_url = format!("http://{addr}/download/{archive_name}");
        let checksums_url = format!("http://{addr}/download/checksums.sha256");
        let archive_name_clone = archive_name.clone();
        let archive_bytes_clone = archive_bytes.clone();
        let checksum_clone = checksum_line.clone();

        let app = axum::Router::new()
            .route(
                "/releases/latest",
                axum::routing::get(move || {
                    let archive_name = archive_name_clone.clone();
                    let archive_url = archive_url.clone();
                    let checksums_url = checksums_url.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "tag_name": "v0.2.0",
                            "html_url": "https://github.com/MaximeBeretvas/citadel/releases/tag/v0.2.0",
                            "assets": [
                                { "name": archive_name, "browser_download_url": archive_url },
                                { "name": "checksums.sha256", "browser_download_url": checksums_url }
                            ]
                        }))
                    }
                }),
            )
            .route(
                &format!("/download/{archive_name}"),
                axum::routing::get(move || {
                    let bytes = archive_bytes_clone.clone();
                    async move { bytes }
                }),
            )
            .route(
                "/download/checksums.sha256",
                axum::routing::get(move || {
                    let text = checksum_clone.clone();
                    async move { text }
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, server)
    }

    #[tokio::test]
    async fn end_to_end_update_against_mock_release_server() {
        let (addr, server) = spawn_mock_release_server().await;

        let dir = tempdir().expect("tempdir");
        let fake_bin = dir.path().join("citadel");
        std::fs::write(&fake_bin, b"old-v0.1.0-binary").expect("write old bin");
        let config = VaultConfig::new(dir.path().join("data"));

        let mut out = Vec::new();
        run_update(
            UpdateOpts {
                config: config.clone(),
                check_only: false,
                yes: true,
                current_version: "0.1.0".to_string(),
                current_exe: fake_bin.clone(),
                releases_api_url: format!("http://{addr}/releases/latest"),
                force_container: Some(false),
                stdin_is_tty: Some(false),
            },
            &mut out,
        )
        .await
        .expect("end-to-end update should succeed");
        server.abort();

        assert_eq!(
            std::fs::read(&fake_bin).expect("read upgraded bin"),
            b"upgraded-v0.2.0-binary"
        );
        assert!(config.update_check_path().is_file());
    }

    #[tokio::test]
    async fn non_tty_update_without_yes_refuses_to_mutate_binary() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let app = axum::Router::new().route(
            "/releases/latest",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "tag_name": "v0.2.0",
                    "html_url": "https://github.com/MaximeBeretvas/citadel/releases/tag/v0.2.0",
                    "assets": []
                }))
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempdir().expect("tempdir");
        let fake_bin = dir.path().join("citadel");
        std::fs::write(&fake_bin, b"old-v0.1.0-binary").expect("write old bin");
        let config = VaultConfig::new(dir.path().join("data"));

        let mut out = Vec::new();
        let err = run_update(
            UpdateOpts {
                config,
                check_only: false,
                yes: false,
                current_version: "0.1.0".to_string(),
                current_exe: fake_bin.clone(),
                releases_api_url: format!("http://{addr}/releases/latest"),
                force_container: Some(false),
                stdin_is_tty: Some(false),
            },
            &mut out,
        )
        .await
        .expect_err("non-TTY update without --yes must refuse");
        server.abort();

        assert!(
            matches!(err, VaultError::Validation(_)),
            "expected Validation error, got: {err}"
        );
        assert_eq!(
            std::fs::read(&fake_bin).expect("read untouched bin"),
            b"old-v0.1.0-binary"
        );
    }
}
