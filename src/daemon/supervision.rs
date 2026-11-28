//! Daemon supervision: whether this vault's OS service runs the daemon, and how to start,
//! stop and restart the daemon so a replaced binary takes over.
//!
//! Two ports keep the rules testable: [`ServiceManager`] (launchd, systemd, or none) and
//! [`DaemonProcess`] (signal and spawn a process). The daemon lock itself is always read
//! through the real `daemon.pid`.

use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{daemon_status, DaemonStatus};
use crate::config::VaultConfig;
use crate::error::{Result, VaultError};

/// Restart command for the daemon. Right whether or not a service supervises it, because
/// `citadel daemon stop` and `start` go through this module.
pub const RESTART_HINT: &str = "citadel daemon stop && citadel daemon start";

/// How long a stop waits for the daemon to release its lock.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a service start waits for the daemon to take its lock.
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an unsupervised restart waits for a service manager to relaunch the daemon
/// before it spawns one.
const RELAUNCH_GRACE: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(100);

/// Parsed fields of an installed OS service definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceFile {
    pub path: PathBuf,
    pub binary: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    /// Why the file could not be read, when it exists but is unreadable.
    pub unreadable: Option<String>,
}

/// State of the OS background service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Unsupported,
    NotInstalled,
    /// The service file location could not be determined.
    Unknown {
        reason: String,
    },
    /// `loaded`: launchd has the job loaded; systemd runs the unit (`is-active`).
    Installed {
        file: ServiceFile,
        loaded: bool,
    },
}

/// The OS service manager that can run the daemon: launchd, systemd, or none.
pub trait ServiceManager {
    /// Read the service file and ask the manager whether it has the job. Read-only.
    fn inspect(&self) -> ServiceState;
    /// The data dir (`CITADEL_HOME`) the installed service file names. Reads only the file.
    fn file_data_dir(&self) -> Option<PathBuf>;
    /// Start the service so it runs the binary now on disk, even one replaced since the job
    /// was loaded.
    fn start(&self) -> Result<()>;
    /// Stop the service so the manager does not relaunch the daemon. Succeeds when the
    /// service is already stopped.
    fn stop(&self) -> Result<()>;
}

/// Signals and spawns daemon processes.
pub trait DaemonProcess {
    /// Ask the daemon with `pid` to shut down (`SIGTERM`).
    fn terminate(&self, pid: u32) -> Result<()>;
    /// Spawn a detached daemon; returns its pid.
    fn spawn(&self) -> Result<u32>;
}

/// How [`Supervision::start`] started the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// This vault's OS service started it; it holds the lock under `pid`.
    Service(u32),
    /// A detached daemon was spawned with `pid`.
    Spawned(u32),
}

/// What [`Supervision::stop`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    NotRunning,
    Stopped(u32),
    /// The daemon `pid` exited and another process (`by`) took the lock straight away.
    Relaunched {
        pid: u32,
        by: u32,
    },
    /// The caller stopped waiting: daemon `pid` got `SIGTERM` and is still finishing its
    /// Syncs; it exits once they are done. A `supervised` daemon's service was left running
    /// and relaunches it then.
    StillDraining {
        pid: u32,
        supervised: bool,
    },
}

/// A stop waiting for daemon `pid` to finish its in-flight Syncs and exit.
#[derive(Debug, Clone, Copy)]
pub struct Draining {
    pub pid: u32,
    pub waited: Duration,
}

/// Whether a stop keeps waiting for a draining daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Continue,
    Abort,
}

/// Outcome of [`Supervision::restart`].
#[derive(Debug)]
pub enum Restart {
    /// The old daemon still holds the lock (it finishes in-flight Syncs first).
    NotStopped {
        pid: u32,
        stop_error: Option<VaultError>,
    },
    /// Another daemon took the lock as soon as the old one exited.
    Relaunched(u32),
    Started(Started),
    StartFailed {
        supervised: bool,
        error: VaultError,
    },
    /// The daemon state could not be read after the stop.
    StatusUnknown(VaultError),
}

/// Daemon supervision for one vault.
pub struct Supervision {
    config: VaultConfig,
    manager: Box<dyn ServiceManager>,
    process: Box<dyn DaemonProcess>,
    stop_timeout: Duration,
    start_timeout: Duration,
    relaunch_grace: Duration,
}

impl Supervision {
    /// Supervision through this platform's service manager; spawned daemons run `exe`.
    pub fn for_vault(config: &VaultConfig, exe: PathBuf) -> Self {
        Self::new(
            config,
            super::service::platform_service_manager(),
            Box::new(OsProcess {
                config: config.clone(),
                exe,
            }),
        )
    }

    pub fn new(
        config: &VaultConfig,
        manager: Box<dyn ServiceManager>,
        process: Box<dyn DaemonProcess>,
    ) -> Self {
        Self {
            config: config.clone(),
            manager,
            process,
            stop_timeout: STOP_TIMEOUT,
            start_timeout: SERVICE_START_TIMEOUT,
            relaunch_grace: RELAUNCH_GRACE,
        }
    }

    /// The OS service state, as the service manager reports it.
    pub fn service(&self) -> ServiceState {
        self.manager.inspect()
    }

    /// Whether this vault's OS service runs the daemon: the installed service file names this
    /// vault. Whether the manager runs it right now does not matter: starting it does.
    pub fn supervised(&self) -> bool {
        self.manager
            .file_data_dir()
            .is_some_and(|dir| crate::fsutil::same_path(&dir, &self.config.data_dir))
    }

    /// Start the daemon: through this vault's service when one is installed, else detached.
    pub fn start(&self) -> Result<Started> {
        self.start_as(self.supervised())
    }

    /// Stop the daemon for good: SIGTERM it, wait (with no time limit) for it to finish its
    /// in-flight Syncs and exit, then stop a supervised daemon's service so `KeepAlive` does
    /// not relaunch it. `on_wait` is called while the daemon drains, about every 100 ms;
    /// [`Wait::Abort`] stops waiting and leaves the daemon draining
    /// ([`Stopped::StillDraining`]).
    pub fn stop(&self, on_wait: &mut dyn FnMut(&Draining) -> Wait) -> Result<Stopped> {
        self.stop_as(self.supervised(), None, on_wait)
    }

    /// Stop the daemon `old_pid` and start it again on the binary now on disk.
    pub fn restart(&self, old_pid: u32) -> Restart {
        let supervised = self.supervised();
        // An update does not wait out a long Sync: past the stop timeout it reports the daemon
        // as not stopped, and leaves it (and its service) to finish.
        let stop_error = self
            .stop_as(supervised, Some(self.stop_timeout), &mut |_| Wait::Continue)
            .err();
        self.restart_after_stop(old_pid, supervised, stop_error)
    }

    /// The second half of [`Supervision::restart`], once the stop returned `stop_error`.
    fn restart_after_stop(
        &self,
        old_pid: u32,
        supervised: bool,
        stop_error: Option<VaultError>,
    ) -> Restart {
        match daemon_status(&self.config) {
            Ok(DaemonStatus::Running(pid)) if pid == old_pid => {
                Restart::NotStopped { pid, stop_error }
            }
            Ok(DaemonStatus::Running(pid)) => Restart::Relaunched(pid),
            Ok(DaemonStatus::Stopped | DaemonStatus::Stale) => {
                // A service manager this vault does not recognise as its own (a plist that
                // names no vault, a moved vault) may still relaunch the daemon: give it a
                // moment before a spawned daemon would fight it for the lock, also when the
                // daemon exited just after the stop timed out. A supervised daemon is
                // started again by reloading its service, whose `bootout` also ends a daemon
                // the service relaunched after this check (one that relaunched before it is
                // reported by `start_as` as already running).
                if !supervised {
                    match self.relaunched_within(self.relaunch_grace) {
                        Ok(Some(pid)) => return Restart::Relaunched(pid),
                        Ok(None) => {}
                        Err(err) => return Restart::StatusUnknown(err),
                    }
                }
                match self.start_as(supervised) {
                    Ok(started) => Restart::Started(started),
                    Err(error) => Restart::StartFailed { supervised, error },
                }
            }
            Err(err) => Restart::StatusUnknown(err),
        }
    }

    fn start_as(&self, supervised: bool) -> Result<Started> {
        if let DaemonStatus::Running(pid) = daemon_status(&self.config)? {
            return Err(VaultError::AlreadyRunning(format!(
                "daemon is already running (pid {pid})"
            )));
        }
        if !supervised {
            return self.process.spawn().map(Started::Spawned);
        }
        self.manager.start().map_err(|err| {
            VaultError::Other(format!(
                "could not start the daemon through its OS service: {err}. To run it without \
                 the service, use `citadel daemon start --foreground`"
            ))
        })?;
        let deadline = Instant::now() + self.start_timeout;
        loop {
            // 0: the daemon took the lock but has not written its pid yet.
            match daemon_status(&self.config)? {
                DaemonStatus::Running(pid) if pid != 0 => return Ok(Started::Service(pid)),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(VaultError::Other(format!(
                    "the service manager did not start the daemon within {} seconds; see {}",
                    self.start_timeout.as_secs(),
                    self.config.log_path().display()
                )));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Drain first: SIGTERM the daemon and wait for it to finish its in-flight Syncs and
    /// exit; only then stop the service, so `KeepAlive` does not relaunch it. Stopping the
    /// service first would have launchd (`ExitTimeOut`) or systemd (`TimeoutStopSec`)
    /// SIGKILL a daemon that is still finishing a Sync. A daemon still running when `limit`
    /// runs out, or when `on_wait` aborts, is left alone, service and all.
    fn stop_as(
        &self,
        supervised: bool,
        limit: Option<Duration>,
        on_wait: &mut dyn FnMut(&Draining) -> Wait,
    ) -> Result<Stopped> {
        let pid = match self.settled_status()? {
            DaemonStatus::Stopped => {
                self.stop_service(supervised);
                return Ok(Stopped::NotRunning);
            }
            DaemonStatus::Stale => {
                let _ = fs::remove_file(self.config.pid_path());
                let _ = fs::remove_file(self.config.daemon_info_path());
                self.stop_service(supervised);
                return Ok(Stopped::NotRunning);
            }
            DaemonStatus::Running(0) => {
                return Err(VaultError::Other(
                    "daemon lock is held, but daemon.pid does not contain a valid PID".into(),
                ))
            }
            DaemonStatus::Running(pid) => pid,
        };
        if let Err(err) = self.process.terminate(pid) {
            if daemon_status(&self.config)? == DaemonStatus::Running(pid) {
                return Err(err);
            }
        }
        let relaunched = match self.wait_for_exit(pid, limit, on_wait)? {
            Exit::Exited => None,
            Exit::Relaunched(by) => Some(by),
            Exit::Aborted => return Ok(Stopped::StillDraining { pid, supervised }),
            Exit::TimedOut => {
                // Only a restart sets a limit, so the advice is about getting the daemon
                // running again, not about stopping it.
                let next = if supervised {
                    " Its OS service, left running, relaunches it when it exits."
                } else {
                    ""
                };
                return Err(VaultError::Other(format!(
                    "sent SIGTERM to daemon (pid {pid}), but it did not exit within {} seconds; \
                     a daemon finishing a Sync exits once the Sync is done.{next}",
                    limit.unwrap_or_default().as_secs()
                )));
            }
        };
        if !supervised {
            return Ok(match relaunched {
                Some(by) => Stopped::Relaunched { pid, by },
                None => Stopped::Stopped(pid),
            });
        }
        // Ends a daemon `KeepAlive` already relaunched: it is idle, so nothing is cut off.
        self.stop_service(true);
        Ok(match self.wait_until_free()? {
            None => Stopped::Stopped(pid),
            Some(by) => Stopped::Relaunched { pid, by },
        })
    }

    /// Stop this vault's service; a failure is logged, and a relaunch it lets through is
    /// reported by the caller.
    fn stop_service(&self, supervised: bool) {
        if supervised {
            if let Err(err) = self.manager.stop() {
                tracing::warn!(error = %err, "could not stop the daemon's OS service");
            }
        }
    }

    /// The daemon status once a lock holder has written its pid (a daemon takes the lock,
    /// then writes its pid), or `Running(0)` if none does within the stop timeout.
    fn settled_status(&self) -> Result<DaemonStatus> {
        let deadline = Instant::now() + self.stop_timeout;
        loop {
            let status = daemon_status(&self.config)?;
            if status != DaemonStatus::Running(0) || Instant::now() >= deadline {
                return Ok(status);
            }
            std::thread::sleep(POLL);
        }
    }

    /// Wait for daemon `pid` to release the lock: for ever, or up to `limit`.
    fn wait_for_exit(
        &self,
        pid: u32,
        limit: Option<Duration>,
        on_wait: &mut dyn FnMut(&Draining) -> Wait,
    ) -> Result<Exit> {
        let started = Instant::now();
        loop {
            match daemon_status(&self.config)? {
                DaemonStatus::Stopped | DaemonStatus::Stale => return Ok(Exit::Exited),
                // 0: a daemon took the lock but has not written its pid yet.
                DaemonStatus::Running(by) if by != pid && by != 0 => {
                    return Ok(Exit::Relaunched(by))
                }
                DaemonStatus::Running(_) => {}
            }
            let waited = started.elapsed();
            if limit.is_some_and(|limit| waited >= limit) {
                return Ok(Exit::TimedOut);
            }
            if let Wait::Abort = on_wait(&Draining { pid, waited }) {
                return Ok(Exit::Aborted);
            }
            std::thread::sleep(POLL);
        }
    }

    /// Wait up to the stop timeout for nobody to hold the lock; `Some(pid)`: `pid` still does.
    fn wait_until_free(&self) -> Result<Option<u32>> {
        let deadline = Instant::now() + self.stop_timeout;
        loop {
            match daemon_status(&self.config)? {
                DaemonStatus::Stopped | DaemonStatus::Stale => return Ok(None),
                DaemonStatus::Running(by) if Instant::now() >= deadline => return Ok(Some(by)),
                DaemonStatus::Running(_) => std::thread::sleep(POLL),
            }
        }
    }

    /// The pid of a daemon that takes the lock within `grace`, e.g. one a service manager
    /// relaunches.
    fn relaunched_within(&self, grace: Duration) -> Result<Option<u32>> {
        let deadline = Instant::now() + grace;
        loop {
            match daemon_status(&self.config)? {
                DaemonStatus::Running(pid) if pid != 0 => return Ok(Some(pid)),
                _ if Instant::now() >= deadline => return Ok(None),
                _ => std::thread::sleep(POLL),
            }
        }
    }
}

/// How waiting for a daemon to leave the lock ended.
enum Exit {
    Exited,
    /// Another daemon took the lock straight away.
    Relaunched(u32),
    /// It still holds the lock after the time limit.
    TimedOut,
    /// The caller stopped waiting.
    Aborted,
}

/// Real processes: `kill -TERM`, and a detached `citadel daemon start --foreground`.
struct OsProcess {
    config: VaultConfig,
    /// Explicit path, so a daemon spawned after a self-update runs the new binary (on Linux
    /// `/proc/self/exe` then reads `(deleted)`).
    exe: PathBuf,
}

impl DaemonProcess for OsProcess {
    #[cfg(unix)]
    fn terminate(&self, pid: u32) -> Result<()> {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(VaultError::Other(format!(
                "failed to send SIGTERM to pid {pid} (exit status {status})"
            )));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn terminate(&self, pid: u32) -> Result<()> {
        Err(VaultError::Other(format!(
            "stopping daemon pid {pid} via signal is not supported on this platform"
        )))
    }

    fn spawn(&self) -> Result<u32> {
        self.config.ensure_data_dir()?;
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.config.log_path())?;
        let log_err = log_file.try_clone()?;
        let child = Command::new(&self.exe)
            .arg("--data-dir")
            .arg(&self.config.data_dir)
            .arg("daemon")
            .arg("start")
            .arg("--foreground")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err))
            .spawn()?;
        Ok(child.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::sync::{Arc, Mutex, MutexGuard};
    use tempfile::{tempdir, TempDir};

    /// A daemon process of the fake: holds the real `daemon.pid` lock under a fake pid.
    struct FakeDaemon {
        pid: u32,
        binary: u32,
        by_service: bool,
        /// Finishing a Sync: it does not exit on `SIGTERM` (yet).
        finishing_sync: bool,
        /// Got `SIGTERM` while finishing a Sync: it exits once the Sync is done.
        sigterm_pending: bool,
        _lock: File,
    }

    struct FakeService {
        /// `None`: the file names no vault (unreadable, or written by an old citadel).
        data_dir: Option<PathBuf>,
        loaded: bool,
        /// The binary launchd saw when it loaded the job.
        loaded_binary: u32,
    }

    struct World {
        pid_path: PathBuf,
        service: Option<FakeService>,
        daemon: Option<FakeDaemon>,
        /// Version of the binary on disk; `swap_binary` bumps it.
        binary: u32,
        next_pid: u32,
        /// How long `KeepAlive` takes to relaunch an exited daemon; zero: at once.
        relaunch_delay: Duration,
        /// The service manager SIGKILLed a daemon in the middle of a Sync.
        killed_mid_sync: bool,
        /// The job cannot be loaded (`bootstrap` keeps failing).
        job_broken: bool,
        /// `inspect` calls (each asks the manager, a subprocess in production).
        inspections: u32,
        /// Stopping the service fails (systemd: no user bus).
        stop_fails: bool,
        spawns: u32,
        service_starts: u32,
    }

    /// Models launchd/systemd and the daemon processes they run:
    /// - `KeepAlive`: whenever the daemon exits while the job is loaded, the manager
    ///   relaunches it at once;
    /// - after the binary is replaced, launchd refuses to spawn the loaded job (`EX_CONFIG`),
    ///   so a relaunch starts nothing; loading the job again (`start`) runs the new binary;
    /// - stopping the service unloads it, so nothing relaunches the daemon; like launchd's
    ///   `ExitTimeOut` and systemd's `TimeoutStopSec`, it SIGKILLs a daemon that is still
    ///   finishing a Sync.
    #[derive(Clone)]
    struct Fake(Arc<Mutex<World>>);

    impl World {
        fn launch(&mut self, by_service: bool) -> u32 {
            let pid = self.next_pid;
            self.next_pid += 1;
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&self.pid_path)
                .expect("open daemon.pid");
            if file.try_lock().is_err() {
                // A real daemon would exit on `AlreadyRunning`.
                return pid;
            }
            file.set_len(0).expect("truncate");
            writeln!(file, "{pid}").expect("write pid");
            self.daemon = Some(FakeDaemon {
                pid,
                binary: self.binary,
                by_service,
                finishing_sync: false,
                sigterm_pending: false,
                _lock: file,
            });
            pid
        }

        /// The daemon exits. Returns whether a loaded `KeepAlive` job relaunches it later:
        /// at once (`relaunch_delay` zero) it is relaunched here. launchd refuses a replaced
        /// binary.
        fn exit(&mut self) -> bool {
            if self.daemon.take().is_some() {
                let _ = fs::remove_file(&self.pid_path);
            }
            if !self.relaunches() {
                return false;
            }
            if self.relaunch_delay.is_zero() {
                self.launch(true);
                return false;
            }
            true
        }

        fn relaunches(&self) -> bool {
            self.service
                .as_ref()
                .is_some_and(|s| s.loaded && s.loaded_binary == self.binary)
        }
    }

    impl Fake {
        fn world(&self) -> MutexGuard<'_, World> {
            self.0.lock().expect("world lock")
        }

        /// The running daemon's Sync ends; after a `SIGTERM` it then exits (and a loaded
        /// `KeepAlive` job relaunches it).
        fn finish_sync(&self) {
            let mut w = self.world();
            let Some(daemon) = w.daemon.as_mut() else {
                return;
            };
            daemon.finishing_sync = false;
            if daemon.sigterm_pending {
                self.exit(w);
            }
        }

        /// The daemon exits; a delayed `KeepAlive` relaunch happens on another thread.
        fn exit(&self, mut w: MutexGuard<'_, World>) {
            if !w.exit() {
                return;
            }
            let delay = w.relaunch_delay;
            drop(w);
            let world = Arc::clone(&self.0);
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let mut w = world.lock().expect("world lock");
                if w.daemon.is_none() && w.relaunches() {
                    w.launch(true);
                }
            });
        }
    }

    impl ServiceManager for Fake {
        fn inspect(&self) -> ServiceState {
            let mut w = self.world();
            w.inspections += 1;
            match &w.service {
                None => ServiceState::NotInstalled,
                Some(s) => ServiceState::Installed {
                    file: ServiceFile {
                        path: PathBuf::from("/LaunchAgents/com.citadel.daemon.plist"),
                        binary: Some(PathBuf::from("/usr/local/bin/citadel")),
                        data_dir: s.data_dir.clone(),
                        unreadable: None,
                    },
                    loaded: s.loaded,
                },
            }
        }

        fn file_data_dir(&self) -> Option<PathBuf> {
            self.world().service.as_ref()?.data_dir.clone()
        }

        fn start(&self) -> Result<()> {
            // Like `Launchd::reload`, whose `bootout` failing is fine.
            let _ = self.stop();
            let mut w = self.world();
            if w.daemon.is_some() || w.job_broken {
                // launchd still tears the old instance down: `bootstrap` keeps failing.
                return Err(VaultError::Other(
                    "Bootstrap failed: 5: Input/output error".into(),
                ));
            }
            w.service_starts += 1;
            let binary = w.binary;
            let service = w
                .service
                .as_mut()
                .expect("start needs an installed service");
            service.loaded = true;
            service.loaded_binary = binary;
            w.launch(true);
            Ok(())
        }

        fn stop(&self) -> Result<()> {
            let mut w = self.world();
            if w.stop_fails {
                return Err(VaultError::Other("Failed to connect to bus".into()));
            }
            let Some(service) = w.service.as_mut() else {
                return Err(VaultError::Other("no service installed".into()));
            };
            service.loaded = false;
            let Some(daemon) = w.daemon.as_ref().filter(|d| d.by_service) else {
                return Ok(());
            };
            // SIGTERM, then SIGKILL once the stop timeout runs out: a Sync is cut off.
            if daemon.finishing_sync {
                w.killed_mid_sync = true;
            }
            w.exit();
            Ok(())
        }
    }

    impl DaemonProcess for Fake {
        fn terminate(&self, pid: u32) -> Result<()> {
            let w = self.world();
            let Some(daemon) = w.daemon.as_ref().filter(|d| d.pid == pid) else {
                return Err(VaultError::Other(format!("kill: {pid}: No such process")));
            };
            if daemon.finishing_sync {
                let mut w = w;
                if let Some(d) = w.daemon.as_mut() {
                    d.sigterm_pending = true;
                }
                return Ok(());
            }
            self.exit(w);
            Ok(())
        }

        fn spawn(&self) -> Result<u32> {
            let mut w = self.world();
            w.spawns += 1;
            Ok(w.launch(false))
        }
    }

    struct Rig {
        _dir: TempDir,
        config: VaultConfig,
        fake: Fake,
    }

    impl Rig {
        /// A vault with no service and no daemon.
        fn new() -> Self {
            let dir = tempdir().expect("tempdir");
            let config = VaultConfig::new(dir.path().join("vault"));
            config.ensure_data_dir().expect("data dir");
            let fake = Fake(Arc::new(Mutex::new(World {
                pid_path: config.pid_path(),
                service: None,
                daemon: None,
                binary: 1,
                next_pid: 100,
                relaunch_delay: Duration::ZERO,
                killed_mid_sync: false,
                job_broken: false,
                inspections: 0,
                stop_fails: false,
                spawns: 0,
                service_starts: 0,
            })));
            Self {
                _dir: dir,
                config,
                fake,
            }
        }

        /// Install and load the service of the vault at `data_dir`; it starts its daemon.
        fn with_service_for(self, data_dir: PathBuf) -> Self {
            let this_vault = crate::fsutil::same_path(&data_dir, &self.config.data_dir);
            self.with_service(Some(data_dir), this_vault)
        }

        /// Install and load a service whose file names `data_dir`; `runs_this_daemon`: it
        /// started this vault's daemon.
        fn with_service(self, data_dir: Option<PathBuf>, runs_this_daemon: bool) -> Self {
            {
                let mut w = self.world();
                let binary = w.binary;
                w.service = Some(FakeService {
                    data_dir,
                    loaded: true,
                    loaded_binary: binary,
                });
                if runs_this_daemon {
                    w.launch(true);
                }
            }
            self
        }

        fn with_this_vaults_service(self) -> Self {
            let data_dir = self.config.data_dir.clone();
            self.with_service_for(data_dir)
        }

        fn supervision(&self) -> Supervision {
            let mut supervision = Supervision::new(
                &self.config,
                Box::new(self.fake.clone()),
                Box::new(self.fake.clone()),
            );
            supervision.stop_timeout = Duration::from_millis(300);
            supervision.start_timeout = Duration::from_millis(300);
            supervision.relaunch_grace = Duration::from_millis(300);
            supervision
        }

        fn world(&self) -> MutexGuard<'_, World> {
            self.fake.world()
        }

        /// The running daemon starts a Sync, so it ignores `SIGTERM` until the Sync is done.
        fn daemon_syncing(&self) {
            self.world()
                .daemon
                .as_mut()
                .expect("a running daemon")
                .finishing_sync = true;
        }

        fn running(&self) -> Option<u32> {
            match daemon_status(&self.config).expect("status") {
                DaemonStatus::Running(pid) => Some(pid),
                _ => None,
            }
        }

        /// `citadel update` replaces the binary; the running daemon keeps the old one.
        fn swap_binary(&self) {
            self.world().binary += 1;
        }

        /// The daemon exits on its own (crash, or a stop before the swap took effect).
        fn daemon_exits(&self) {
            self.world().exit();
        }
    }

    #[test]
    fn only_this_vaults_service_supervises_its_daemon() {
        let rig = Rig::new();
        assert!(!rig.supervision().supervised(), "no service installed");

        let other = rig.config.data_dir.with_file_name("other");
        let rig = rig.with_service_for(other);
        assert!(
            !rig.supervision().supervised(),
            "a loaded job for another vault does not run this daemon"
        );

        let rig = Rig::new().with_this_vaults_service();
        rig.world().service.as_mut().expect("service").loaded = false;
        assert!(
            rig.supervision().supervised(),
            "a stopped (unloaded) service still owns this vault's daemon"
        );

        #[cfg(unix)]
        {
            let rig = Rig::new();
            let link = rig.config.data_dir.with_file_name("vault-link");
            std::os::unix::fs::symlink(&rig.config.data_dir, &link).expect("symlink");
            let rig = rig.with_service_for(link);
            assert!(
                rig.supervision().supervised(),
                "same vault through a symlink"
            );
        }
    }

    #[test]
    fn start_runs_a_supervised_daemon_through_its_service() {
        // launchd refused to relaunch the job after the binary swap: the job is loaded, the
        // daemon is gone. A detached spawn would run beside the job, outside it.
        let rig = Rig::new().with_this_vaults_service();
        rig.swap_binary();
        rig.daemon_exits();
        assert_eq!(rig.running(), None, "launchd refused the replaced binary");

        let started = rig.supervision().start().expect("start");
        let pid = rig.running().expect("the service runs the daemon");
        assert_eq!(started, Started::Service(pid));
        let w = rig.world();
        assert_eq!(w.spawns, 0, "no detached daemon beside the service");
        assert_eq!(w.daemon.as_ref().map(|d| d.binary), Some(2), "new binary");
    }

    #[test]
    fn start_spawns_a_daemon_no_service_of_this_vault_runs() {
        let other = Rig::new().config.data_dir.with_file_name("other");
        for rig in [Rig::new(), Rig::new().with_service_for(other)] {
            let started = rig.supervision().start().expect("start");
            let pid = rig.running().expect("spawned daemon");
            assert_eq!(started, Started::Spawned(pid));
            assert_eq!(rig.world().service_starts, 0);
        }
    }

    #[test]
    fn deciding_supervision_does_not_ask_the_manager() {
        // Only the service file decides; `launchctl list`/`systemctl is-active` are for doctor.
        let rig = Rig::new().with_this_vaults_service();
        assert!(rig.supervision().supervised());
        assert_eq!(rig.world().inspections, 0);
    }

    #[test]
    fn start_names_the_foreground_escape_when_the_service_cannot_start() {
        // No reachable manager (no user bus, no `gui/<uid>` domain over SSH): spawning beside
        // the service is not an option, running in the foreground is.
        let rig = Rig::new().with_this_vaults_service();
        rig.world().daemon = None;
        let _ = fs::remove_file(rig.config.pid_path());
        rig.world().job_broken = true;
        let err = rig.supervision().start().expect_err("service cannot start");
        assert!(
            err.to_string()
                .contains("citadel daemon start --foreground"),
            "{err}"
        );
        assert_eq!(rig.world().spawns, 0);
    }

    #[test]
    fn start_refuses_while_a_daemon_runs() {
        let rig = Rig::new().with_this_vaults_service();
        let err = rig.supervision().start().expect_err("already running");
        assert!(matches!(err, VaultError::AlreadyRunning(_)), "{err}");
    }

    #[test]
    fn stop_keeps_a_supervised_daemon_stopped() {
        // A bare SIGTERM would have `KeepAlive` relaunch it at once.
        let rig = Rig::new().with_this_vaults_service();
        let pid = rig.running().expect("running");
        assert_eq!(
            rig.supervision()
                .stop(&mut |_| Wait::Continue)
                .expect("stop"),
            Stopped::Stopped(pid)
        );
        assert_eq!(rig.running(), None, "the service did not relaunch it");
    }

    #[test]
    fn stop_reports_a_relaunch_instead_of_timing_out() {
        // Another vault's job is not stopped; a supervisor this vault does not know about
        // (here: a loaded job the fake relaunches) takes the lock straight away.
        let rig = Rig::new();
        rig.world().service = Some(FakeService {
            data_dir: Some(rig.config.data_dir.with_file_name("other")),
            loaded: true,
            loaded_binary: 1,
        });
        let pid = rig.world().launch(true);
        let stopped = rig
            .supervision()
            .stop(&mut |_| Wait::Continue)
            .expect("stop");
        let by = rig.running().expect("relaunched");
        assert_ne!(by, pid);
        assert_eq!(stopped, Stopped::Relaunched { pid, by });
    }

    #[test]
    fn stop_still_signals_the_daemon_when_the_service_stop_fails() {
        let rig = Rig::new().with_this_vaults_service();
        let pid = rig.running().expect("running");
        rig.world().stop_fails = true;
        let stopped = rig
            .supervision()
            .stop(&mut |_| Wait::Continue)
            .expect("stop");
        let by = rig.running().expect("the loaded service relaunched it");
        assert_eq!(stopped, Stopped::Relaunched { pid, by });
    }

    #[test]
    fn stop_can_stop_waiting_and_leaves_the_daemon_draining() {
        // Ctrl-C while a supervised daemon finishes a long Sync: nothing is killed, the
        // service is left running, and the stop says so.
        let rig = Rig::new().with_this_vaults_service();
        let pid = rig.running().expect("running");
        rig.daemon_syncing();
        let mut calls = 0;
        let stopped = rig
            .supervision()
            .stop(&mut |draining| {
                assert_eq!(draining.pid, pid);
                calls += 1;
                if calls == 3 {
                    Wait::Abort
                } else {
                    Wait::Continue
                }
            })
            .expect("stop");
        assert_eq!(
            stopped,
            Stopped::StillDraining {
                pid,
                supervised: true
            }
        );
        assert_eq!(rig.running(), Some(pid), "the daemon finishes its Sync");
        {
            let w = rig.world();
            assert!(!w.killed_mid_sync, "the Sync was cut off");
            assert!(
                w.service.as_ref().expect("service").loaded,
                "service left running"
            );
        }
        // Its Sync ends: it exits, and its service relaunches it, as the stop said.
        rig.fake.finish_sync();
        assert!(rig.running().is_some_and(|by| by != pid));
    }

    #[test]
    fn stop_reports_its_wait_while_the_daemon_drains() {
        let rig = Rig::new();
        rig.world().launch(false);
        rig.daemon_syncing();
        let fake = rig.fake.clone();
        let mut waits = Vec::new();
        let stopped = rig
            .supervision()
            .stop(&mut |draining| {
                waits.push(draining.waited);
                if waits.len() == 5 {
                    fake.finish_sync();
                }
                Wait::Continue
            })
            .expect("stop");
        assert!(matches!(stopped, Stopped::Stopped(_)), "{stopped:?}");
        assert_eq!(waits.len(), 5);
        assert!(waits.windows(2).all(|w| w[0] <= w[1]), "{waits:?}");
    }

    /// Stop a supervised daemon whose Sync ends after `sync_left`, well past the old 5 s cap
    /// (300 ms in the rig); the service relaunches it after `relaunch_delay`.
    fn stop_through_a_long_sync(relaunch_delay: Duration) {
        let rig = Rig::new().with_this_vaults_service();
        rig.world().relaunch_delay = relaunch_delay;
        let pid = rig.running().expect("running");
        rig.daemon_syncing();
        let fake = rig.fake.clone();
        let sync = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            fake.finish_sync();
        });
        let stopped = rig.supervision().stop(&mut |_| Wait::Continue);
        sync.join().expect("sync thread");
        assert_eq!(stopped.expect("stopped"), Stopped::Stopped(pid));
        // Past any delayed relaunch: nothing brings it back.
        std::thread::sleep(relaunch_delay + Duration::from_millis(200));
        assert_eq!(rig.running(), None, "stopped for good");
        let w = rig.world();
        assert!(
            !w.service.as_ref().expect("service").loaded,
            "service stopped"
        );
        assert!(!w.killed_mid_sync, "the Sync was cut off");
    }

    #[test]
    fn supervised_stop_waits_out_a_long_sync_and_stops_for_good() {
        // KeepAlive relaunches at once, between the exit and the service stop.
        stop_through_a_long_sync(Duration::ZERO);
    }

    #[test]
    fn supervised_stop_wins_a_delayed_relaunch() {
        stop_through_a_long_sync(Duration::from_millis(100));
    }

    #[test]
    fn supervised_stop_stops_the_service_once_the_daemon_has_drained() {
        // The daemon exits on SIGTERM, KeepAlive relaunches it, and the service stop then
        // ends the idle relaunched daemon and keeps it stopped.
        let rig = Rig::new().with_this_vaults_service();
        let pid = rig.running().expect("running");
        assert_eq!(
            rig.supervision()
                .stop(&mut |_| Wait::Continue)
                .expect("stop"),
            Stopped::Stopped(pid)
        );
        assert_eq!(rig.running(), None);
        let w = rig.world();
        assert!(
            !w.service.as_ref().expect("service").loaded,
            "service stopped"
        );
        assert!(!w.killed_mid_sync);
    }

    #[test]
    fn stop_waits_for_a_daemon_to_write_its_pid() {
        // A daemon takes the lock, then writes its pid: in between, daemon.pid is empty.
        let rig = Rig::new().with_this_vaults_service();
        let pid = rig.running().expect("running");
        fs::write(rig.config.pid_path(), "").expect("empty pid file, lock still held");
        let pid_path = rig.config.pid_path();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            fs::write(pid_path, format!("{pid}\n")).expect("write pid");
        });
        let stopped = rig.supervision().stop(&mut |_| Wait::Continue);
        writer.join().expect("writer");
        assert_eq!(stopped.expect("stop"), Stopped::Stopped(pid));
    }

    #[test]
    fn stop_clears_a_stale_pid_file() {
        let rig = Rig::new();
        fs::write(rig.config.pid_path(), "7\n").expect("stale pid");
        assert_eq!(
            rig.supervision()
                .stop(&mut |_| Wait::Continue)
                .expect("stop"),
            Stopped::NotRunning
        );
        assert!(!rig.config.pid_path().exists());
    }

    #[test]
    fn restart_runs_the_replaced_binary_through_the_service() {
        let rig = Rig::new().with_this_vaults_service();
        let old = rig.running().expect("running");
        rig.swap_binary();

        let restart = rig.supervision().restart(old);
        let pid = rig.running().expect("restarted");
        assert!(
            matches!(restart, Restart::Started(Started::Service(p)) if p == pid && p != old),
            "{restart:?}"
        );
        let w = rig.world();
        assert_eq!(w.daemon.as_ref().map(|d| d.binary), Some(2), "new binary");
        assert_eq!(w.spawns, 0, "no detached daemon beside the service");
    }

    #[test]
    fn restart_spawns_the_replaced_binary_without_a_service() {
        let rig = Rig::new();
        let old = rig.world().launch(false);
        rig.swap_binary();
        let restart = rig.supervision().restart(old);
        let pid = rig.running().expect("restarted");
        assert!(
            matches!(restart, Restart::Started(Started::Spawned(p)) if p == pid),
            "{restart:?}"
        );
        assert_eq!(rig.world().daemon.as_ref().map(|d| d.binary), Some(2));
    }

    #[test]
    fn restart_leaves_a_daemon_finishing_a_sync_alone() {
        for rig in [Rig::new().with_this_vaults_service(), Rig::new()] {
            if rig.running().is_none() {
                rig.world().launch(false);
            }
            let old = rig.running().expect("running");
            rig.daemon_syncing();
            let restart = rig.supervision().restart(old);
            assert!(
                matches!(restart, Restart::NotStopped { pid, stop_error: Some(_) } if pid == old),
                "{restart:?}"
            );
            assert_eq!(rig.running(), Some(old), "it finishes its Sync");
            let w = rig.world();
            assert!(!w.killed_mid_sync, "the Sync was cut off");
            assert_eq!((w.spawns, w.service_starts), (0, 0), "nothing fights it");
        }
    }

    #[test]
    fn restart_waits_for_a_relaunch_before_spawning() {
        // A loaded job whose file names no vault (unreadable plist): not this vault's
        // service, yet launchd relaunches the daemon shortly after it exits.
        let rig = Rig::new().with_service(None, true);
        rig.world().relaunch_delay = Duration::from_millis(100);
        let old = rig.running().expect("running");
        let restart = rig.supervision().restart(old);
        let by = rig.running().expect("relaunched");
        assert!(
            matches!(restart, Restart::Relaunched(p) if p == by),
            "{restart:?}"
        );
        assert_eq!(
            rig.world().spawns,
            0,
            "a second daemon would fight for the lock"
        );
    }

    #[test]
    fn restart_waits_for_a_relaunch_after_a_timed_out_stop() {
        // The stop timed out, then the daemon finished its Sync and exited before the
        // restart looked again; launchd relaunches it shortly after.
        let rig = Rig::new().with_service(None, true);
        rig.world().relaunch_delay = Duration::from_millis(100);
        let old = rig.running().expect("running");
        rig.fake.exit(rig.world());
        let restart = rig.supervision().restart_after_stop(
            old,
            false,
            Some(VaultError::Other("did not exit within 5 seconds".into())),
        );
        let by = rig.running().expect("relaunched");
        assert!(
            matches!(restart, Restart::Relaunched(p) if p == by),
            "{restart:?}"
        );
        assert_eq!(
            rig.world().spawns,
            0,
            "a second daemon would fight for the lock"
        );
    }

    #[test]
    fn restart_leaves_a_relaunched_daemon_alone() {
        let rig = Rig::new();
        rig.world().service = Some(FakeService {
            data_dir: Some(rig.config.data_dir.with_file_name("other")),
            loaded: true,
            loaded_binary: 1,
        });
        let old = rig.world().launch(true);
        let restart = rig.supervision().restart(old);
        let by = rig.running().expect("relaunched");
        assert!(
            matches!(restart, Restart::Relaunched(p) if p == by),
            "{restart:?}"
        );
        assert_eq!(
            rig.world().spawns,
            0,
            "a second daemon would fight for the lock"
        );
    }

    #[test]
    fn restart_never_spawns_when_the_service_fails_to_start() {
        let rig = Rig::new().with_this_vaults_service();
        let old = rig.running().expect("running");
        rig.world().job_broken = true;
        let restart = rig.supervision().restart(old);
        assert!(
            matches!(
                restart,
                Restart::StartFailed {
                    supervised: true,
                    ..
                }
            ),
            "{restart:?}"
        );
        assert_eq!(
            rig.world().spawns,
            0,
            "a detached daemon would run outside the service"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_reports_a_daemon_relaunched_by_its_service_manager_as_stopped() {
        // Real processes: launchd (`KeepAlive`) relaunches the job as soon as it exits, so
        // the lock is held again, by another pid, before the first poll.
        let rig = Rig::new();
        let _lock = super::super::PidLock::acquire(&rig.config.pid_path()).expect("lock");
        let mut child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let old_pid = child.id();
        fs::write(rig.config.pid_path(), format!("{old_pid}\n")).expect("record old pid");
        let pid_path = rig.config.pid_path();
        let relauncher = std::thread::spawn(move || {
            let _ = child.wait();
            fs::write(pid_path, "424242\n").expect("record relaunched pid");
        });

        let supervision = Supervision::new(
            &rig.config,
            Box::new(rig.fake.clone()),
            Box::new(OsProcess {
                config: rig.config.clone(),
                exe: PathBuf::from("citadel"),
            }),
        );
        let stopped = supervision.stop(&mut |_| Wait::Continue);
        relauncher.join().expect("relauncher");
        assert_eq!(
            stopped.expect("the old daemon stopped"),
            Stopped::Relaunched {
                pid: old_pid,
                by: 424242
            }
        );
    }

    #[test]
    fn spawn_runs_the_explicit_executable() {
        let rig = Rig::new();
        let process = OsProcess {
            config: rig.config.clone(),
            exe: rig.config.data_dir.join("nonexistent-citadel-bin"),
        };
        let err = process.spawn().expect_err("missing exe");
        assert!(
            matches!(err, VaultError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound),
            "{err}"
        );
    }
}
