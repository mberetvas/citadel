<p align="center">
  <img src="Docs/assets/logo/citadel-intro.gif" alt="Citadel logo: local knowledge vault &amp; MCP server for docs" width="720" />
</p>

<p align="center">
  <a href="https://github.com/MaximeBeretvas/citadel/actions/workflows/ci.yml?query=branch%3Amain"><img src="https://img.shields.io/github/actions/workflow/status/MaximeBeretvas/citadel/ci.yml?branch=main&label=CI" alt="CI status" /></a>
  <a href="https://github.com/MaximeBeretvas/citadel/releases/latest"><img src="https://img.shields.io/github/v/release/MaximeBeretvas/citadel" alt="Latest release" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/MaximeBeretvas/citadel" alt="License: Apache-2.0" /></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey" alt="Platforms: macOS and Linux" />
</p>

Local knowledge vault and MCP server for web documentation sites and local Markdown folders. `citadel` crawls documentation sites, watches local Markdown directories, indexes content into SQLite + FTS5 with section splitting and unified diff history, and serves an offline embedded Web UI alongside a 2-tool MCP interface.

## Key Features

- **Unified ingestion:** Crawl `http(s)://` docs sites (`spider`, `sitemap.xml`, `llms.txt`, optional headless Chromium) and watch local Markdown folders with incremental SHA-256 diffing and `stale`/`error` tracking.
- **SQLite + FTS5 search:** Single-file WAL database with heading-level sections (`H1`–`H3`), breadcrumbs, FTS5 tables (`pages_fts`, `sections_fts`), and query views (`v_sources`, `v_recent_changes`).
- **Hardened 2-tool MCP server:** Exposes `query` (strictly read-only SQLite authorizer, 5s timeout, row/byte caps) and `manage` (Private-origin-guarded URL and canonicalized path validation) over Streamable HTTP (`/mcp`) or `stdio`.
- **Self-contained runtime:** Single Rust binary with bundled SQLite and `rust-embed` offline Web UI (Sources, Knowledge Browser, Changes & Health, SQL Explorer).

## Resource Usage

Measured on an M1 Pro Mac with a real docs site (500 pages of dlthub docs):

| | Memory | CPU |
|---|---|---|
| Daemon, idle | ~5 MB (empty vault), ~25–35 MB after Syncs | ~0.1 % |
| Daemon, syncing 500 pages | ~45–60 MB median, up to ~120 MB peak, for about a minute | ~6–7 CPU-seconds per Sync |
| `citadel mcp`, agent queries | under 8 MB | ~3 ms per query |
| `citadel ui` | under 10 MB | negligible |

Method, every scenario, and caveats (measured on `v0.1.0`): [Docs/resource-usage.md](Docs/resource-usage.md).

## Install (Recommended)

Install the `citadel` binary straight from GitHub with Cargo. Requires a Rust toolchain ([`rustup`](https://rustup.rs/)); SQLite is bundled, so no system `sqlite3` library is needed.

```bash
cargo install --git https://github.com/MaximeBeretvas/citadel --locked

# Lite build: no headless-browser support (smaller binary, no Chrome needed)
cargo install --git https://github.com/MaximeBeretvas/citadel --locked --no-default-features
```

The default build includes the `chrome` feature, so Sources added with `--render-js` can crawl sites whose navigation is built by JavaScript. Those crawls need Google Chrome or Chromium installed (or `CHROME_URL` pointing at a running instance); Citadel launches a private headless browser only while such a crawl runs, so other Sources and the idle daemon never start one. On Linux, see [Troubleshooting `render_js` on Linux](#troubleshooting-render_js-on-linux) if those Syncs fail with `No usable sandbox!`.

This builds the `main` branch; add `--tag <tag>` or `--branch <name>` to pin another ref, and re-run with `--force` to upgrade. Ensure `~/.cargo/bin` is on your `$PATH`, then verify and optionally register the background service:

```bash
citadel --version

# Register and start the background scheduler & local folder watcher
# (launchd on macOS, systemd --user on Linux)
citadel daemon install
```

### Prebuilt Binaries (macOS & Linux)

Every [GitHub Release](https://github.com/MaximeBeretvas/citadel/releases) ships a `citadel` binary built with the default features (including `chrome`) for macOS (Apple Silicon `aarch64-apple-darwin`, Intel `x86_64-apple-darwin`) and Linux (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`; glibc 2.35+), plus a `checksums.sha256` manifest. No Rust toolchain needed:

```bash
V=v0.4.1                  # release tag
T=aarch64-apple-darwin    # your target, from the list above
curl -LO https://github.com/MaximeBeretvas/citadel/releases/download/$V/citadel-$V-$T.tar.gz
curl -LO https://github.com/MaximeBeretvas/citadel/releases/download/$V/checksums.sha256
shasum -a 256 -c --ignore-missing checksums.sha256   # Linux: sha256sum -c --ignore-missing checksums.sha256
mkdir -p ~/.local/bin
tar xzf citadel-$V-$T.tar.gz && install -m 755 citadel ~/.local/bin/
```

Make sure `~/.local/bin` is on your `$PATH` (on macOS it is not by default: add `export PATH="$HOME/.local/bin:$PATH"` to `~/.zshrc`). After that, `citadel update` keeps it current. On macOS, a binary downloaded through a **browser** is quarantined by Gatekeeper (the release is not notarized); clear it once with `xattr -d com.apple.quarantine citadel`. Downloads via `curl`, `cargo install`, and `citadel update` are not affected.

### Windows

Windows is **not yet tested** and has no prebuilt binary. Install with the `cargo install --git` command above, or build from a checkout (see below). `citadel update` and `citadel daemon install` are not supported on Windows.

## Alternative: Container (Docker)

The `Makefile` drives Docker by default. Apple's `container` CLI ([`apple/container`](https://github.com/apple/container)) also works: add `RUNTIME=container` to any target (for example `make build RUNTIME=container`). It is still beta, and its build VM cache once grew to 98 GB and filled the disk (clear it with `container builder delete`), so Docker is the default.

```bash
# Build the multi-stage non-root image (includes Chromium for --render-js Sources)
make build

# Lite image without Chromium; --render-js Sources fall back to static HTTP
make build-lite

# Start the container (Web UI + REST API + MCP HTTP on loopback)
make run

# Mount a local Markdown folder read-only at /sources/books
BOOKS_DIR=/path/to/your/markdown make run
```

Open the embedded Web UI at **http://127.0.0.1:4317**.

Useful `Makefile` targets:

```bash
make logs                      # Inspect container logs
make exec CMD="list"           # Run CLI commands inside the container
make exec CMD="sync --all"     # Trigger a manual sync inside the container
make stop                      # Stop the running container
make clean                     # Stop and remove the container (preserves data volume)
```

## Alternative: Install from a Local Checkout

For development, install from a cloned repository instead:

```bash
make install          # cargo install --path . --locked --force (default features, includes chrome)

make install-lite     # same, with --no-default-features (no headless-browser support)
```

## Quick Start

```bash
# Index a docs site (or a local Markdown folder) and run its first Sync
citadel add https://docs.example.com --name example-docs

# Browse and search it in the Web UI
citadel ui

# Connect your coding agents (skill + MCP entry)
citadel setup
```

In the container, run the same commands through `make exec`, for example `make exec CMD="add https://docs.example.com"`; the Web UI is already running at http://127.0.0.1:4317.

## MCP Client Configuration

`citadel` exposes two MCP tools:

- **`query`**: Execute a single read-only SQLite/FTS5 `SELECT` statement against the vault (`v_sources`, `pages`, `sections`, `pages_fts`, `sections_fts`, `v_recent_changes`) with `SQLITE_OPEN_READ_ONLY`, `PRAGMA query_only = ON`, authorizer allowlist, 5-second timeout, and row/byte caps.
- **`manage`**: Register, update, sync, or remove sources (`add`, `update`, `sync`, `remove`) with directory canonicalization on local paths. Every entry point (CLI, UI, MCP, daemon) refuses Private origins (loopback, private network, link-local, CGNAT, `localhost`); set `CITADEL_ALLOW_PRIVATE_ORIGINS=1` to index intranet or `localhost` docs.

`citadel setup` (see [Agent Skill Installation](#agent-skill-installation)) writes these entries for Claude, Antigravity, and Codex. Use the snippets below for other clients or to configure by hand.

### Streamable HTTP (Recommended with `citadel serve` or the container)

Point Antigravity or any Streamable HTTP MCP client to the endpoint below. Claude Code also needs `"type": "http"` in the `citadel` entry (`citadel setup` adds it for you):

```json
{
  "mcpServers": {
    "citadel": {
      "url": "http://127.0.0.1:4317/mcp"
    }
  }
}
```

### Native `stdio` (`citadel mcp`)

```json
{
  "mcpServers": {
    "citadel": {
      "command": "citadel",
      "args": ["mcp"]
    }
  }
}
```

### Container `stdio` (`docker exec -i citadel citadel mcp`)

```json
{
  "mcpServers": {
    "citadel": {
      "command": "docker",
      "args": ["exec", "-i", "citadel", "citadel", "mcp"]
    }
  }
}
```

With `RUNTIME=container`, use `"command": "container"` instead.

## Agent Skill Installation

The quickest route is the setup wizard, which installs the skill and registers the MCP server for Claude, Antigravity, and Codex in one step:

```bash
# Interactive wizard
citadel setup

# Non-interactive: global skill + stdio MCP entry for Claude and Codex
citadel setup --agent claude --agent codex --scope global --transport stdio -y
```

`--scope local` writes into the current project (for example `.mcp.json`, `.codex/config.toml`); `--scope global` writes into your home directory. Use `--only skill` or `--only mcp` to configure one part, `--transport http` (with `--url`) to point agents at a running `citadel serve`, and `--force` to overwrite existing entries.

To install only the skill, install the embedded `SKILL.md` router so coding agents know the vault schema, FTS5 recipes, and `query`/`manage` workflows:

```bash
# Install into the current repository (.agents/skills/citadel/SKILL.md)
citadel skill install

# Install globally into ~/.agents/skills, symlinked into each agent's skill directory
citadel skill install --global

# Limit a global install to one agent (agents, claude, copilot, opencode; default: all)
citadel skill install --global --target claude

# Install into a custom skills parent directory
citadel skill install --path /path/to/skills
```

Add `--force` to overwrite an existing `SKILL.md` or symlink.

## Core CLI Commands

| Command | Description |
|---|---|
| `citadel add <URL_OR_PATH> [--name SLUG] [OPTIONS]` | Register and sync a documentation URL or local Markdown directory. Options: `--tags`, `--interval` (e.g. `24h`, `30m`; `0` turns off scheduled Syncs), `--max-pages`, `--max-depth`, `--include`/`--exclude` (glob or prefix), `--render-js` (headless Chrome), `--no-sync`. |
| `citadel list [--json]` | List configured sources, page counts, and health status. |
| `citadel show <NAME>` | Inspect source details, recent sync runs, and stale/error pages. |
| `citadel sync [NAME \| --all] [--prune]` | Manually sync one or all sources (optionally pruning stale pages). |
| `citadel rm <NAME> [--yes]` | Remove a source and cascade-delete its indexed pages. |
| `citadel query "<SQL>" [--limit N] [--format table\|json\|csv]` | Run a read-only SQLite/FTS5 query against the vault (default 50 rows, max 200). |
| `citadel ui [--port PORT] [--no-open]` | Launch the embedded local Web UI server on `127.0.0.1` (default port 4317; falls back to a free port if 4317 is taken). |
| `citadel serve [--bind ADDR]` | Run the all-in-one server (daemon + Web UI + MCP Streamable HTTP on `/mcp`); default bind `0.0.0.0:4317` (also respects `CITADEL_BIND`). |
| `citadel mcp` | Run the MCP server over `stdio` (newline-delimited JSON-RPC 2.0). |
| `citadel daemon <start [--foreground]\|stop\|status\|install\|uninstall>` | Manage the background scheduler/watcher and OS service registration. |
| `citadel doctor [--json]` | Read-only health report: versions, what is running, and where the skill and MCP entry are set up. Exits 1 if a check fails. |
| `citadel skill install [--global [--target T] \| --path DIR] [--force]` | Install the embedded agent router `SKILL.md`. |
| `citadel setup [--agent A] [--scope S] [--transport T] [--only O] [--force] [-y]` | Wizard that installs the skill and MCP entry for Claude, Antigravity (`agy`), and Codex. |
| `citadel update [--check] [-y]` | Check for or install the latest GitHub Release binary (macOS and Linux). |

All commands also accept `--data-dir <DIR>` (or `CITADEL_HOME`) to override the vault data directory.

## Troubleshooting `render_js` on Linux

Natively, Citadel keeps Chrome's sandbox on. On Ubuntu 23.10+ (and other distros that restrict unprivileged user namespaces through AppArmor), a Chrome or Chromium without its own AppArmor profile cannot start that sandbox, so every `render_js` Sync fails with `No usable sandbox!`. Fixes, safest first:

1. Use a browser that ships an AppArmor profile (Google Chrome's `.deb`, the Chromium snap), or give yours one. See [Chromium's AppArmor guide](https://chromium.googlesource.com/chromium/src/+/main/docs/security/apparmor-userns-restrictions.md), which also covers the system-wide `sysctl` alternative.
2. Point `CHROME_URL` at a browser that runs elsewhere (e.g. `http://127.0.0.1:9222`).
3. Set `CITADEL_CHROME_NO_SANDBOX=1` to launch Chrome without its sandbox. This weakens isolation from the pages Citadel crawls; use it only for sites you trust.

Chrome also refuses to run as root with its sandbox on, whatever AppArmor allows: run Citadel as a regular user, point `CHROME_URL` at a browser that runs elsewhere, or set `CITADEL_CHROME_NO_SANDBOX=1`. The container image is not affected: there, the container is the isolation boundary.

The background daemon does not inherit your shell's environment. To set the variable for it on Linux, run `systemctl --user edit citadel` and add:

```ini
[Service]
Environment=CITADEL_CHROME_NO_SANDBOX=1
```

Then restart it with `systemctl --user restart citadel`.

## Documentation

- [Docs/PRD.md](Docs/PRD.md) — Product scope, user journeys, and requirements.
- [Docs/SPEC.md](Docs/SPEC.md) — Architecture, SQLite + FTS5 schema, MCP contracts, and deployment details.
- [Docs/resource-usage.md](Docs/resource-usage.md) — Memory and CPU measurements and how they were taken.
- [Docs/adr/](Docs/adr/) — Architecture decision records.
- [CONTEXT.md](CONTEXT.md) — Domain glossary (Source, Origin, Sync, Sync claim).

## Development

Contributors and coding agents follow [AGENTS.md](AGENTS.md). Every change must pass:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
