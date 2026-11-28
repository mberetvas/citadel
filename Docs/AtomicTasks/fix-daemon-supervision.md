# Daemon supervision: one owner

Source: architecture review 2026-11-28, candidate 01 (Strong). Region: `src/daemon/service.rs`,
the pid/process code of `src/daemon/mod.rs`, `src/doctor/`, `src/update/mod.rs`, the daemon arm
of `src/cli/mod.rs`.

## Findings

### 1. `daemon_version_check` rows that get `kickstart -k`: confirmed, all four
`restart_hint(service_installed)` (`src/doctor/version.rs:108-117`) is used by every warn row
of `daemon_version_check` (`version.rs:121-150`):
- "daemon runs X, cli is Y";
- "binary no longer exists";
- "binary replaced since the daemon started";
- "daemon started by an older citadel (version unknown)".

All four mean the binary on disk changed under the running daemon. That is exactly when launchd
can refuse to spawn the job (`EX_CONFIG`, "needs LWCR update") while `kickstart -k` exits 0. That
lesson is in AGENTS.md and at `src/daemon/service.rs:248-250`.

Red test: `doctor::version::tests::restart_hint_never_kickstarts_a_swapped_binary` fails on
macOS with `daemon runs 0.2.0, cli is 0.3.0: launchctl kickstart -k gui/$(id -u)/com.citadel.daemon`.

On Linux the hint is `systemctl --user restart citadel.service`, which does run the new binary. The
wrong hint is macOS-only.

### 2. Doctor's loaded-only rule names another vault's job: confirmed
`src/doctor/mod.rs:69-74` treats any `Installed { loaded: true, .. }` as the service that runs
this daemon, whatever its `CITADEL_HOME`. `update` already requires the data dir to match
(`service_supervises`, `src/update/mod.rs:474-483`). So the two "supervised" rules disagree.

For a vault whose daemon was spawned detached while the one launchd job serves another vault,
doctor's hint therefore restarts the other vault's daemon.

Red test: the rule was extracted unchanged into `service_runs_daemon`. Then
`doctor::tests::another_vaults_service_does_not_run_this_daemon` fails: a loaded job for
`/vaults/other` counts as running `/vaults/this`.

### 3. launchctl/systemctl call sites and dialects: changed
| Call site | Command | Dialect |
|---|---|---|
| `service.rs:128` install | `launchctl unload <plist>` (status ignored) | legacy |
| `service.rs:133` install | `launchctl load -w <plist>` | legacy |
| `service.rs:155` uninstall | `launchctl unload <plist>` | legacy |
| `service.rs:257` start | `launchctl bootout gui/<uid>/com.citadel.daemon` | modern |
| `service.rs:263` start | `launchctl bootstrap gui/<uid> <plist>`, retried for 10 s | modern |
| `doctor/running.rs:173` loaded? | `launchctl list com.citadel.daemon` | legacy |
| `doctor/version.rs:113` hint | `launchctl kickstart -k gui/$(id -u)/…` (printed, not run) | modern |
| `service.rs:185,194` install | `systemctl --user daemon-reload`, `enable --now citadel.service` | — |
| `service.rs:213,217` uninstall | `systemctl --user disable --now`, `daemon-reload` | — |
| `service.rs:269` start | `systemctl --user start citadel.service` | — |
| `doctor/running.rs:184` loaded? | `systemctl --user is-active citadel.service` | — |
| `doctor/version.rs:115` hint | `systemctl --user restart citadel.service` (printed) | — |
| `update/mod.rs:988` test | the string "launchctl kickstart failed" | — |
| `Docs/SPEC.md:504,575-576`, `Docs/resource-usage.md:36`, README:246-253 | prose | — |

Does install's legacy `unload`/`load -w` hit the teardown race that `start_service` retries
around? **Refuted.** It has a different defect instead.

Evidence: a throwaway LaunchAgent (label `dev.citadel-investigation.teardown`, never the citadel
job) ran a `/bin/sh` loop that takes 3, 6 or 12 s to exit on SIGTERM. Observed on this Mac
(Darwin 27.2), one run each, not documented launchd behaviour:
- `launchctl unload` blocked about 5 s (5.02 s and 5.16 s), and the process was gone afterwards.
  With a 12 s handler it was gone too, so unload ends the job after about 5 s. The `load -w`
  that followed succeeded every time. So legacy `unload` does not race.
- `launchctl bootout` returned in 0.05 s with the job still listed. An immediate `bootstrap`
  failed: `Bootstrap failed: 5: Input/output error`, exit 5. This is the race `start_service`
  retries around.
- `launchctl load -w` right after a `bootout` printed `Load failed: 5: Input/output error` but
  **exited 0**. Install's `status.success()` check (`service.rs:139`) can therefore never see a
  failed load. `citadel daemon install` can then print "Installed background service" with
  nothing loaded.

Linux has a related gap. `service_is_loaded` uses `systemctl --user is-active`, which means
"running now", not "registered". After a clean stop, `Restart=on-failure` does not relaunch the
unit, so an enabled, stopped unit reads as "not loaded". Update's `service_supervises` then says
unsupervised. Also, `install` runs `enable --now`, which does not restart a running unit, so
`citadel daemon install` after a binary swap leaves the old binary running. That contradicts
update's advice "run `citadel daemon install`, which restarts it … on the new version".

### 4a. CLI `daemon start` takes the unsupervised spawn path beside a service: confirmed
Confirmed by reading the code. `src/cli/mod.rs:497` calls `daemon::start_daemon(config, false)`,
which goes to `start_daemon_with_exe` (`src/daemon/mod.rs:187-213`). That function checks only
the pid lock and then spawns a detached `citadel daemon start --foreground`. It never consults
the OS service.

So the PR #22 symptom happens without `update`: the launchd job (after launchd refused a swapped
binary) or the stopped systemd unit stays registered, and an unsupervised daemon runs beside it.
The manager keeps failing on the lock. A red test needs a service seam, which does not exist yet,
so this one is proven through the new interface (see Acceptance criteria).

### 4b. `stop_daemon` reports a timeout when KeepAlive relaunches immediately: confirmed
`stop_daemon` (`src/daemon/mod.rs:217-247`) polls until the lock is not held by *any* pid. launchd
relaunches a `KeepAlive` job at once when it has run longer than its throttle interval, which is
the normal case for a daemon. The relaunched daemon holds the lock under a new pid. The poll
mistakes it for the old one and reports "did not exit within 5 seconds" after the old daemon
has exited.

Even when stop is reported correctly, a SIGTERM to a supervised daemon stops nothing: the manager
starts it again.

Red test: `daemon::tests::stop_reports_a_daemon_relaunched_by_its_service_manager_as_stopped`. A
real `sleep` child stands in for the old daemon, and the lock is re-recorded under another pid
once the child exits. The test fails with `sent SIGTERM to daemon (pid …), but it did not exit
within 5 seconds`.

## Task
Give daemon supervision one owner: a new module `src/daemon/supervision.rs`.
- It answers whether this vault's launchd job or systemd unit runs the daemon.
- It starts, stops and restarts the daemon in a way that survives a binary swap.
- It words the restart hint.

Doctor, update and the CLI's `daemon start`/`stop` call it. Install, uninstall, start and stop
use one launchctl dialect (`bootout`/`enable`/`bootstrap`).

## Acceptance criteria
- `update` no longer imports `doctor::running`. `ServiceState` and the service-file parsing
  live in the supervision module, and doctor only renders them.
- Doctor's daemon-version hint never names `launchctl`/`systemctl`. It is
  `citadel daemon stop && citadel daemon start`, which is right in every case because both
  commands route through the module (claims 1 and 2).
- A daemon is supervised when this vault's service file exists and names this vault's data dir,
  whether or not the manager has it loaded or active right now. Another vault's job never
  counts (claim 2).
- `citadel daemon start` with this vault's service installed starts the service (launchd: reload
  with retry; systemd: `start`). It never spawns a detached daemon (claim 4a).
- `citadel daemon stop` with this vault's service installed stops the service first (launchd
  `bootout`, systemd `stop`), so `KeepAlive` does not undo it. It then sends SIGTERM to whatever
  still holds the lock.
- A stop that sees the lock re-taken by another pid reports that the old daemon stopped and that
  it was relaunched, without a timeout (claim 4b).
- `citadel daemon install` reloads through the same routine as start, so a failed bootstrap is
  an error (claim 3). On Linux it restarts the unit so a swapped binary takes effect.
- `citadel update` restarts through the module (stop, then start). Its decision-table tests drive
  the module through a fake service manager that models:
  - `KeepAlive` relaunch;
  - launchd refusing to spawn the job after a binary swap;
  - a daemon still finishing a Sync.

  The closure-driven tests are deleted.
- Both production adapters (launchd, systemd) compile on every platform. Only the default
  adapter is chosen by `cfg(target_os)`.
- SPEC.md's doctor, update and daemon sections describe the new behaviour. CONTEXT.md gains
  **Supervised daemon**.
- Gate: `cargo fmt --check`, both clippy runs, and `cargo test` all pass.

## Design decisions
**Interface.** A `Supervision` value is built for one vault, with its dependencies passed in:
- `service()` returns the `ServiceState`, which doctor renders.
- `supervised()` says whether this vault's service runs the daemon.
- `status()`.
- `start()` returns `Started::{Service, Spawned}(pid)`.
- `stop()` returns `Stopped::{NotRunning, Stopped(pid), Relaunched { pid, by }}`.
- `restart(old_pid)` returns a `Restart` outcome that update turns into text.
- `RESTART_HINT`.

All of the decision logic lives behind those few calls: which path to take, the lock polling,
relaunch detection, and the binary-swap-safe reload.

**Seams.** There are two small ports, each with production adapters and one shared fake:
- `ServiceManager` (`inspect`, `start`, `stop`). Its production adapters are `Launchd` and
  `Systemd`; `cfg(target_os)` picks the default, and on other platforms there is none.
- `DaemonProcess` (`terminate(pid)`, `spawn()`). Its production adapter sends SIGTERM and spawns
  a detached child.

The process port exists because KeepAlive relaunch happens when the *process* exits. A fake
that only owned the service could not model it. Tests that went through real SIGTERM would also
signal the test runner, because `PidLock` records the test's own pid. The fake holds real flock
locks in a tempdir vault under fake pids, so `daemon_status` stays the production pid-file code.

**Supervised = this vault's service is installed, not "loaded".** `is-active` is false for a
stopped unit, and a job that `daemon stop` booted out is not loaded. A "loaded" rule would send
the next `start` down the detached path, which is the bug in claim 4a. Doctor's "installed and
loaded" row keeps its meaning, but systemd's check becomes `is-enabled`, so it matches launchd's
"registered" semantics.

Because `stop` now boots out the job, a not-loaded launchd job for this vault is normal. That
row's hint becomes `citadel daemon start`, which loads it.

**`stop` stops the service.** This is a visible change. Under KeepAlive, today's
`citadel daemon stop` cannot stop a supervised daemon, so it was wrong. The job comes back at the
next login (launchd loads the plist again) or with `citadel daemon start`, the same as an
enabled systemd unit.

**Rejected alternatives.**
- **Keep a platform-specific doctor hint** (a `launchctl bootout && bootstrap` one-liner). It is
  long, platform-forked and easy to get wrong. The citadel commands are right everywhere once
  they route through the module.
- **`citadel daemon install` as the hint.** It rewrites the plist to the current executable, so
  running doctor from a dev build would point the service at that build.
- **Keep closures for update's restart.** The review flagged canned closures as the reason the
  restart path regressed three times. A fake that models launchd lets the tests state the
  launchd facts.
- **Only compare against the old pid in `stop` (no service stop).** That fixes the false
  timeout, but the daemon still comes straight back.

## Outcome (HumanAudit)

### Red → green
| Claim | Red on the old code | Green now |
|---|---|---|
| 1 | `doctor::version::tests::restart_hint_never_kickstarts_a_swapped_binary`: hint was `launchctl kickstart -k …` | Same test; every warn row's hint is `RESTART_HINT` (`citadel daemon stop && citadel daemon start`). |
| 2 | `doctor::tests::another_vaults_service_does_not_run_this_daemon`, on the rule extracted unchanged (`service_runs_daemon`) | Test and rule deleted: doctor no longer needs a "supervised" rule, because its hint is constant. Coverage of the one remaining rule: `daemon::supervision::tests::only_this_vaults_service_supervises_its_daemon`. |
| 4a | None: there was no seam, so the evidence is from code reading. | `daemon::supervision::tests::start_runs_a_supervised_daemon_through_its_service`; this test was written before the module existed, through the fake. |
| 4b | `daemon::tests::stop_reports_a_daemon_relaunched_by_its_service_manager_as_stopped` (real `sleep` child): "did not exit within 5 seconds" | Moved to `daemon::supervision::tests::` (same name, real `OsProcess`). It now expects `Stopped::Relaunched { pid, by }` and no timeout. It also caught a real race: `PidLock` truncates, then writes the pid, so a poll can read pid 0. Stop and start now wait for a non-zero pid. Also covered by `stop_keeps_a_supervised_daemon_stopped` (fake). |
| 3 | No unit test, because launchctl is only run live (throwaway label above). | `daemon::service::tests::launchd_start_reloads_the_existing_plist`: install now goes through the same `bootout`/`enable`/`bootstrap` reload. |

`launchctl enable` was checked on the throwaway label `dev.citadel-investigation.enable`:
- It exits 0 when the label is unknown, when it is loaded, and when it is disabled.
- `bootstrap` of a disabled job fails with `5: Input/output error` until `enable` runs.

So `enable` stays on the reload path and its error is propagated.

### Behaviour changes
- **`citadel daemon stop` with this vault's service** stops the service first: launchd `bootout`, systemd `stop`. It then sends SIGTERM to whatever holds the lock. The launchd job stays unloaded until `citadel daemon start` or the next login.
  - If stopping the service fails (for example, systemd with no user bus), the failure is logged and the daemon is still signalled.
  - A relaunch is then reported, not a timeout.
- **"Supervised"** no longer requires the job to be loaded. So `daemon start` and `update` load a job the user unloaded by hand. They also clear a `launchctl disable` (`enable` before `bootstrap`); before this change only `install` did that, through `load -w`.
- **An update restart that finds the daemon still finishing a Sync** (`Restart::NotStopped`) leaves a supervised job booted out. The message tells the user to run `citadel daemon start`, which reloads it.
- **Doctor:**
  - The daemon-version hint is always `citadel daemon stop && citadel daemon start`.
  - A service file that is present but not loaded is `·` info with hint `citadel daemon start`. It used to be a warn with hint `citadel daemon install`.
  - ~~systemd "loaded" means `is-enabled`, not `is-active`.~~ Reverted after the review: it is `is-active` again (see Review fixes, finding 5).
- **`citadel daemon start` through the service** prints "Started citadel daemon through its OS service (pid …, log: …)."
- **`citadel daemon install` on Linux** runs `enable` and then `restart`, instead of `enable --now`.

### Verification limits
- Supervised paths were exercised through the fake adapter only. No live launchd or systemd run was made against the citadel job, as the brief forbids.
- The CLI smoke test used a temp `HOME`/`CITADEL_HOME` with no service: start, status, a second start (refused), stop, a second stop, and `doctor`.
- The Linux adapters and install code now compile on macOS too: only the choice of adapter is behind `cfg`. Linux behaviour itself was not run.

## Review fixes

Review of `116f42c..75098fa` (claude-opus-5-5, `/code-review` medium). Every bug test was red
before its fix. The fake was changed first so that it models reality: a service stop
SIGKILLs a daemon that is still finishing a Sync, as launchd (`ExitTimeOut`) and systemd
(`TimeoutStopSec`) do; KeepAlive can relaunch after a delay; Sync state is per daemon; and a
service file can name no vault.

| # | Finding | Status | Commit | Red → green test |
|---|---|---|---|---|
| 1 | A supervised stop SIGKILLs a daemon finishing a Sync (blocker) | Fixed: drain first | 947cace | `daemon::supervision::tests::supervised_stop_lets_the_daemon_finish_its_sync`, `restart_leaves_a_daemon_finishing_a_sync_alone` (now asserts `!killed_mid_sync`); new green: `supervised_stop_stops_the_service_once_the_daemon_has_drained` |
| 2 | `stop` fails on `Running(0)` after the service is already stopped | Fixed: waits for the pid, before touching the service | 947cace | `stop_waits_for_a_daemon_to_write_its_pid` |
| 3 | The relaunch grace was dropped from the unsupervised restart | Fixed: 1 s `RELAUNCH_GRACE` before spawning | 947cace | `restart_waits_for_a_relaunch_before_spawning` (fake: plist naming no vault, relaunch after 100 ms) |
| 4 | Unloaded-service hint ignores a data-dir mismatch | Fixed: `daemon start` only for this vault, else `daemon install` | 8ada4d8 | `doctor::running::tests::service_not_loaded_for_another_vault_hints_install` |
| 5 | systemd "loaded = enabled" makes doctor show OK while the daemon is down | Fixed: `is-active` again (supervision no longer depends on it); comment reworded | 8ada4d8 | `daemon::service::tests::systemd_loaded_means_active` |
| 6 | No version bump | Skipped, as instructed: bump and tag at merge time | — | — |
| C1 | `Fake::start` propagates its own stop error; `Launchd::reload` ignores a failed `bootout` | Fixed | 947cace (the c58c298 commit message wrongly claims it) | covered by the existing restart tests |
| C2 | `supervised()` runs a subprocess it does not need | Fixed: `ServiceManager::file_data_dir` reads only the file | c58c298 | `deciding_supervision_does_not_ask_the_manager` |
| B1 | `daemon start` fails with no reachable manager | Kept: falling back would reintroduce claim 4a. The error now names `citadel daemon start --foreground` | c58c298 | `start_names_the_foreground_escape_when_the_service_cannot_start` |
| B2 | Supervised means installed, not loaded or enabled | Kept, as documented in Design decisions | — | `only_this_vaults_service_supervises_its_daemon` |
| B3 | Linux `daemon install` now `restart`s a running daemon | Deferred, see follow-ups | — | — |

### Finding 1: drain first, not stop timeouts
I chose suggestion (b), drain first, over (a), a long `ExitTimeOut` / `TimeoutStopSec` with
`--no-block`, for three reasons:
- **It works for already-installed jobs.** Their plists and units are not rewritten until
  `citadel daemon install` runs again; (a) would protect only new installs, or would need
  users to reinstall.
- **On timeout it falls back to the pre-branch behaviour.** A daemon still syncing after 5 s
  is left running, service and all. The stop or update reports it, and the service relaunches
  the daemon once its Sync is done (on the new binary, or launchd refuses it after a swap,
  and `citadel daemon start` then reloads it).
- **No CLI call blocks for 90 s** on `systemctl stop`: the service is stopped only after the
  daemon has exited.

How the stop now runs:
1. Send SIGTERM to the daemon.
2. Wait up to 5 s for the lock to be released, or to be taken by a KeepAlive relaunch.
3. Only then stop the service. That ends a relaunched daemon while it is still idle.

The update warning for a daemon that has not stopped now says that its service may relaunch
it on the new version.

### Follow-ups (deferred, not widened here)
- **`citadel daemon install` while a Sync runs.**
  - On macOS, `Launchd::reload` boots out a running job, and launchd SIGKILLs it after
    `ExitTimeOut`. On `main`, `unload` ended it after about 5 s, so this was already there
    before this branch.
  - On Linux, `systemctl restart` sends SIGTERM and waits for `TimeoutStopSec` (90 s by
    default) before SIGKILL, so a Sync shorter than that finishes.
  - A fix would drain through `Supervision::stop` before reloading. It is out of this
    review's scope.
- **Version bump at merge** (finding 6).

### Remaining risks
- Supervised paths are tested through the fake only. No live launchd or systemd run touched
  the citadel job.
- The drain-first stop relies on KeepAlive leaving a gap in which the service stop ends the
  relaunched daemon. The service stop and `wait_until_free` cover it, but a real launchd job
  was not exercised.
