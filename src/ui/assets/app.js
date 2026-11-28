(() => {
  "use strict";

  const VALID_VIEWS = ["sources", "browser", "changes", "sql"];
  const THEME_STORAGE_KEY = "citadel-theme";

  const SQL_PRESETS = [
    {
      label: "v_sources overview",
      sql: "SELECT id, name, kind, status, active_pages, stale_pages, error_pages, total_words, last_synced_at\nFROM v_sources\nORDER BY name",
    },
    {
      label: "FTS5 search sections",
      sql: "SELECT s.name AS source, p.uri, sec.heading_path, snippet(sections_fts, 1, '[', ']', '...', 18) AS excerpt\nFROM sections_fts\nJOIN sections sec ON sec.id = sections_fts.rowid\nJOIN pages p ON p.id = sec.page_id\nJOIN sources s ON s.id = p.source_id\nWHERE sections_fts MATCH 'vault OR doc OR guide'\nLIMIT 25",
    },
    {
      label: "Recent diffs",
      sql: "SELECT page_id, source_name, uri, title, changed_at\nFROM v_recent_changes\nLIMIT 25",
    },
    {
      label: "Stale & error pages",
      sql: "SELECT s.name AS source, p.uri, p.status, p.http_status, p.error_message, p.last_checked_at\nFROM pages p\nJOIN sources s ON s.id = p.source_id\nWHERE p.status IN ('stale', 'error')\nORDER BY p.last_checked_at DESC\nLIMIT 50",
    },
    {
      label: "Sync history",
      sql: "SELECT s.name AS source, r.status, r.pages_added, r.pages_updated, r.pages_unchanged, r.pages_stale, r.pages_failed, r.started_at\nFROM sync_runs r\nJOIN sources s ON s.id = r.source_id\nORDER BY r.started_at DESC\nLIMIT 25",
    },
  ];

  const state = {
    view: "sources",
    sources: [],
    confirmDeleteId: null,
    browserPages: [],
    selectedPageId: null,
    detailMode: "sections",
    changesRows: [],
    selectedChangePageId: null,
    sqlHasRun: false,
    addSourceKind: "web",
    browserSearchSeq: 0,
  };

  function el(id) {
    return document.getElementById(id);
  }

  function escapeHtml(value) {
    return String(value ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function formatNumber(n) {
    const num = Number(n || 0);
    return Number.isFinite(num) ? num.toLocaleString() : "0";
  }

  function formatTimestamp(iso) {
    if (!iso) return "Never";
    const parsed = new Date(iso);
    if (Number.isNaN(parsed.getTime())) return String(iso);
    return parsed.toISOString().replace("T", " ").slice(0, 16) + "Z";
  }

  function renderStatusBadge(status) {
    const clean = String(status || "idle").toLowerCase();
    const icons = {
      ok: '<svg class="badge-icon" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3.5 8.5 6.5 11.5 12.5 4.5"></polyline></svg>',
      active: '<svg class="badge-icon" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3.5 8.5 6.5 11.5 12.5 4.5"></polyline></svg>',
      stale: '<svg class="badge-icon" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2"><circle cx="8" cy="8" r="6"></circle><line x1="8" y1="5" x2="8" y2="8.5"></line><circle cx="8" cy="11.2" r="0.6" fill="currentColor"></circle></svg>',
      error: '<svg class="badge-icon" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2"><line x1="4.5" y1="4.5" x2="11.5" y2="11.5"></line><line x1="11.5" y1="4.5" x2="4.5" y2="11.5"></line></svg>',
      syncing: '<svg class="badge-icon" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2"><path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9"></path><polyline points="13.5 2.5 13.5 5.5 10.5 5.5"></polyline></svg>',
      idle: '<svg class="badge-icon" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2"><circle cx="8" cy="8" r="4"></circle></svg>',
    };
    const svg = icons[clean] || icons.idle;
    return `<span class="badge badge-${escapeHtml(clean)}">${svg}<span>${escapeHtml(clean)}</span></span>`;
  }

  function showToast(message, isError = false) {
    const region = el("toast-region");
    if (!region) return;
    const node = document.createElement("div");
    node.className = isError ? "toast toast-error" : "toast";
    node.textContent = message;
    region.appendChild(node);
    window.setTimeout(() => {
      node.remove();
    }, 3600);
  }

  async function apiRequest(path, options = {}) {
    const init = {
      method: options.method || "GET",
      headers: { Accept: "application/json", ...(options.headers || {}) },
    };
    if (options.body !== undefined) {
      init.headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(options.body);
    }
    const resp = await fetch(path, init);
    const text = await resp.text();
    let payload = null;
    if (text) {
      try {
        payload = JSON.parse(text);
      } catch {
        payload = { error: text };
      }
    }
    if (!resp.ok) {
      const msg =
        (payload && payload.error) ||
        `Request failed (${resp.status} ${resp.statusText})`;
      throw new Error(msg);
    }
    return options.withHeaders ? { payload, headers: resp.headers } : payload;
  }

  // GET a Page list. The server cuts a list at its limit (default 1000) and says so
  // in a header, so the count can read "1000+" instead of looking complete.
  async function fetchPageList(path) {
    const { payload, headers } = await apiRequest(path, { withHeaders: true });
    return {
      pages: Array.isArray(payload) ? payload : [],
      truncated: headers.get("X-Citadel-Truncated") === "true",
    };
  }

  function formatPageCount(list) {
    return list.truncated ? `${list.pages.length}+` : String(list.pages.length);
  }

  // Theme management (supports automatic prefers-color-scheme + persisted override)
  function getEffectiveTheme() {
    const attr = document.documentElement.getAttribute("data-theme");
    if (attr === "light" || attr === "dark") return attr;
    const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (stored === "light" || stored === "dark") return stored;
    return window.matchMedia &&
      window.matchMedia("(prefers-color-scheme: dark)").matches
      ? "dark"
      : "light";
  }

  function applyTheme(theme, persist) {
    if (theme === "light" || theme === "dark") {
      document.documentElement.setAttribute("data-theme", theme);
      if (persist) {
        window.localStorage.setItem(THEME_STORAGE_KEY, theme);
      }
    } else {
      document.documentElement.removeAttribute("data-theme");
    }
    updateThemeButton();
  }

  function updateThemeButton() {
    const label = el("theme-label");
    if (!label) return;
    const active = getEffectiveTheme();
    label.textContent = active === "dark" ? "Light Mode" : "Dark Mode";
  }

  function initTheme() {
    const queryTheme = new URLSearchParams(window.location.search).get("theme");
    const stored =
      queryTheme === "light" || queryTheme === "dark"
        ? queryTheme
        : window.localStorage.getItem(THEME_STORAGE_KEY);
    if (stored === "light" || stored === "dark") {
      applyTheme(stored, false);
    } else {
      updateThemeButton();
    }
    const toggle = el("theme-toggle");
    if (toggle) {
      toggle.addEventListener("click", () => {
        const next = getEffectiveTheme() === "dark" ? "light" : "dark";
        applyTheme(next, true);
      });
    }
  }

  // Routing
  const VALID_PAGE_STATUSES = ["active", "stale", "error"];

  function parseHashView() {
    const raw = (window.location.hash || "").replace(/^#/, "").trim().toLowerCase();
    const view = raw.split("?")[0];
    return VALID_VIEWS.includes(view) ? view : "sources";
  }

  function parseHashParams() {
    const raw = (window.location.hash || "").replace(/^#/, "");
    const qIdx = raw.indexOf("?");
    return new URLSearchParams(qIdx >= 0 ? raw.slice(qIdx + 1) : "");
  }

  function navigateToBrowser(sourceId, status) {
    const cleanSource = sourceId !== null && sourceId !== undefined && sourceId !== "" ? String(sourceId) : "";
    const cleanStatus = VALID_PAGE_STATUSES.includes(status) ? status : "";

    const sourceSelect = el("browser-source-select");
    const statusSelect = el("browser-status-select");
    const searchInput = el("browser-search-input");
    if (sourceSelect) sourceSelect.value = cleanSource;
    if (statusSelect) statusSelect.value = cleanStatus;
    if (searchInput) searchInput.value = "";

    const params = new URLSearchParams();
    if (cleanSource) params.set("source", cleanSource);
    if (cleanStatus) params.set("status", cleanStatus);
    const qs = params.toString();
    const targetHash = qs ? `#browser?${qs}` : "#browser";

    if (window.location.hash === targetHash) {
      void loadBrowserView();
    } else {
      window.location.hash = targetHash;
    }
  }

  function setActiveView(view, shouldLoad = true) {
    state.view = VALID_VIEWS.includes(view) ? view : "sources";
    if (state.view !== "sources") {
      hidePagesTooltip();
    }

    document.querySelectorAll(".view").forEach((sec) => {
      sec.hidden = sec.getAttribute("data-view") !== state.view;
    });

    document.querySelectorAll(".nav-tab").forEach((tab) => {
      const isMatch = tab.getAttribute("data-view") === state.view;
      tab.classList.toggle("active", isMatch);
      if (isMatch) {
        tab.setAttribute("aria-current", "page");
      } else {
        tab.removeAttribute("aria-current");
      }
    });

    if (shouldLoad) {
      void loadActiveView();
    }
  }

  async function loadActiveView() {
    if (state.view === "sources") {
      await loadSourcesView();
    } else if (state.view === "browser") {
      await loadBrowserView();
    } else if (state.view === "changes") {
      await loadChangesView();
    } else if (state.view === "sql") {
      await loadSqlView();
    }
  }

  // Version
  async function loadVersion() {
    try {
      const data = await apiRequest("/api/version");
      const versionEl = el("app-version");
      if (versionEl && data && data.current_version) {
        versionEl.textContent = `v${data.current_version}`;
      }
      const wrap = el("update-badge-wrap");
      const label = el("update-badge-label");
      const link = el("update-release-link");
      if (!wrap || !label) return;

      if (data && data.update_available && data.latest_version) {
        label.textContent = `Update v${data.latest_version} available`;
        if (link && typeof data.release_url === "string" && /^https?:\/\//i.test(data.release_url)) {
          link.href = data.release_url;
          link.hidden = false;
        } else if (link) {
          link.hidden = true;
        }
        wrap.hidden = false;
      } else {
        wrap.hidden = true;
      }
    } catch {
      // Fail silently if /api/version is unavailable.
    }
  }

  function initUpdatePopover() {
    const btn = el("update-badge-btn");
    const popover = el("update-popover");
    const wrap = el("update-badge-wrap");
    if (!btn || !popover || !wrap) return;

    const close = () => {
      popover.hidden = true;
      btn.setAttribute("aria-expanded", "false");
    };

    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const willOpen = popover.hidden;
      popover.hidden = !willOpen;
      btn.setAttribute("aria-expanded", String(willOpen));
    });

    document.addEventListener("click", (e) => {
      if (!popover.hidden && !wrap.contains(e.target)) {
        close();
      }
    });

    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && !popover.hidden) {
        close();
      }
    });
  }

  // View 1: Sources
  async function fetchSources() {
    const sources = await apiRequest("/api/sources");
    state.sources = Array.isArray(sources) ? sources : [];
    populateBrowserSourceSelect();
    return state.sources;
  }

  async function loadSourcesView() {
    const tbody = el("sources-tbody");
    try {
      const sources = await fetchSources();
      renderSourcesKpis(sources);
      renderSourcesTable(sources);
    } catch (err) {
      if (tbody) {
        tbody.innerHTML = `<tr><td colspan="7" class="empty-cell">Failed to load sources: ${escapeHtml(err.message)}</td></tr>`;
      }
    }
  }

  function renderSourcesKpis(sources) {
    const activePages = sources.reduce((acc, s) => acc + Number(s.active_pages || 0), 0);
    const problemPages = sources.reduce(
      (acc, s) => acc + Number(s.stale_pages || 0) + Number(s.error_pages || 0),
      0
    );
    const totalWords = sources.reduce((acc, s) => acc + Number(s.total_words || 0), 0);

    el("kpi-sources").textContent = formatNumber(sources.length);
    el("kpi-active-pages").textContent = formatNumber(activePages);
    el("kpi-problem-pages").textContent = formatNumber(problemPages);
    el("kpi-total-words").textContent = formatNumber(totalWords);
  }

  function formatPagesBreakdown(source) {
    const active = Number(source.active_pages || 0);
    const stale = Number(source.stale_pages || 0);
    const errors = Number(source.error_pages || 0);
    const activeWord = active === 1 ? "page" : "pages";
    const errorWord = errors === 1 ? "error" : "errors";
    return `${formatNumber(active)} active ${activeWord} · ${formatNumber(stale)} stale (no longer fetched, last good copy kept) · ${formatNumber(errors)} ${errorWord} (last fetch failed)`;
  }

  function renderPagesCountButton(sourceId, status, label, descId) {
    return `<button type="button" class="pages-count-btn pages-count-${status}" data-action="browse-status" data-id="${sourceId}" data-status="${status}" aria-describedby="${descId}">${label}</button>`;
  }

  function renderPagesCell(s) {
    const active = Number(s.active_pages || 0);
    const stale = Number(s.stale_pages || 0);
    const errors = Number(s.error_pages || 0);
    const breakdown = formatPagesBreakdown(s);
    const descId = `pages-desc-${s.id}`;

    let activeHtml;
    if (active > 0) {
      activeHtml = renderPagesCountButton(s.id, "active", formatNumber(active), descId);
    } else if (stale + errors === 0) {
      activeHtml = `<span class="pages-zero-trigger" tabindex="0" aria-describedby="${descId}">0</span>`;
    } else {
      activeHtml = "<span>0</span>";
    }

    let staleOrErrHtml = "";
    if (stale + errors > 0) {
      const stalePart =
        stale > 0
          ? renderPagesCountButton(s.id, "stale", `${formatNumber(stale)}s`, descId)
          : "<span>0s</span>";
      const errorPart =
        errors > 0
          ? renderPagesCountButton(s.id, "error", `${formatNumber(errors)}e`, descId)
          : "<span>0e</span>";
      staleOrErrHtml = `<span class="muted-meta pages-meta">(${stalePart} / ${errorPart})</span>`;
    }

    return `<td class="num pages-cell" data-pages-tooltip="${escapeHtml(breakdown)}"><span id="${descId}" class="sr-only">${escapeHtml(breakdown)}</span>${activeHtml}${staleOrErrHtml}</td>`;
  }

  let hidePagesTooltip = () => {};

  function positionPagesTooltip(tooltip, anchorEl) {
    const pad = 8;
    const gap = 8;
    tooltip.style.left = "0px";
    tooltip.style.top = "0px";
    const anchorRect = anchorEl.getBoundingClientRect();
    const tipRect = tooltip.getBoundingClientRect();

    let top = anchorRect.top - tipRect.height - gap;
    if (top < pad) {
      top = anchorRect.bottom + gap;
    }
    const maxTop = Math.max(pad, window.innerHeight - tipRect.height - pad);
    top = Math.min(Math.max(pad, top), maxTop);

    const idealLeft = anchorRect.left + anchorRect.width / 2 - tipRect.width / 2;
    const maxLeft = Math.max(pad, window.innerWidth - tipRect.width - pad);
    const left = Math.min(Math.max(pad, idealLeft), maxLeft);

    tooltip.style.top = `${Math.round(top)}px`;
    tooltip.style.left = `${Math.round(left)}px`;
  }

  function initPagesTooltip() {
    const tooltip = el("pages-tooltip");
    const tbody = el("sources-tbody");
    if (!tooltip || !tbody) return;

    // Feature-detect popover="hint"; fall back to "manual" where unsupported (e.g. Safari)
    if ("popover" in HTMLElement.prototype) {
      const probe = document.createElement("div");
      probe.popover = "hint";
      if (probe.popover !== "hint") {
        tooltip.setAttribute("popover", "manual");
      }
    } else {
      tooltip.hidden = true;
    }

    let activeCell = null;
    let hideTimer = null;

    const cancelHide = () => {
      if (hideTimer !== null) {
        window.clearTimeout(hideTimer);
        hideTimer = null;
      }
    };

    const hideNow = () => {
      cancelHide();
      activeCell = null;
      if (typeof tooltip.hidePopover === "function") {
        try {
          if (tooltip.matches(":popover-open")) {
            tooltip.hidePopover();
          }
        } catch {
          // Ignore if already closed.
        }
      } else {
        tooltip.hidden = true;
      }
    };

    hidePagesTooltip = hideNow;

    const scheduleHide = () => {
      cancelHide();
      hideTimer = window.setTimeout(hideNow, 120);
    };

    const showForCell = (cell) => {
      const text = cell.getAttribute("data-pages-tooltip");
      if (!text) return;
      cancelHide();
      activeCell = cell;
      tooltip.textContent = text;
      if (typeof tooltip.showPopover === "function") {
        try {
          if (!tooltip.matches(":popover-open")) {
            tooltip.showPopover();
          }
        } catch {
          // Ignore if already open.
        }
      } else {
        tooltip.hidden = false;
      }
      positionPagesTooltip(tooltip, cell);
    };

    const handleTriggerEnter = (event) => {
      const cell = event.target.closest("td[data-pages-tooltip]");
      if (cell) showForCell(cell);
    };

    const handleTriggerLeave = (event) => {
      const cell = event.target.closest("td[data-pages-tooltip]");
      if (!cell) return;
      const next = event.relatedTarget;
      if (next && (cell.contains(next) || tooltip.contains(next))) return;
      scheduleHide();
    };

    const hideIfActive = () => {
      if (activeCell) hideNow();
    };

    tbody.addEventListener("mouseover", handleTriggerEnter);
    tbody.addEventListener("focusin", handleTriggerEnter);
    tbody.addEventListener("mouseout", handleTriggerLeave);
    tbody.addEventListener("focusout", handleTriggerLeave);

    tooltip.addEventListener("mouseenter", cancelHide);
    tooltip.addEventListener("mouseleave", (event) => {
      const next = event.relatedTarget;
      if (activeCell && next && activeCell.contains(next)) return;
      scheduleHide();
    });

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") hideIfActive();
    });
    window.addEventListener("scroll", hideIfActive, true);
    window.addEventListener("resize", hideIfActive);
  }

  function renderSourcesTable(sources) {
    hidePagesTooltip();
    const tbody = el("sources-tbody");
    if (!tbody) return;
    if (!sources.length) {
      tbody.innerHTML =
        '<tr><td colspan="7" class="empty-cell">No sources indexed yet. Click <strong>+ Add Source</strong> to index a website or local Markdown folder.</td></tr>';
      return;
    }

    tbody.innerHTML = sources
      .map((s) => {
        const tagsHtml =
          Array.isArray(s.tags) && s.tags.length
            ? `<div class="tag-list">${s.tags.map((t) => `<span class="tag">${escapeHtml(t)}</span>`).join("")}</div>`
            : "";

        const isConfirming = state.confirmDeleteId === s.id;
        const removeControls = isConfirming
          ? `<button type="button" class="btn btn-danger btn-sm" data-action="confirm-delete" data-id="${s.id}">Confirm</button>
             <button type="button" class="btn btn-ghost btn-sm" data-action="cancel-delete">Cancel</button>`
          : `<button type="button" class="btn btn-ghost btn-sm" data-action="ask-delete" data-id="${s.id}">Remove</button>`;

        return `<tr>
          <td>
            <div class="source-name">${escapeHtml(s.name)}</div>
            <div class="source-origin">${escapeHtml(s.origin)}</div>
            ${tagsHtml}
          </td>
          <td><span class="badge badge-kind">${escapeHtml(s.kind)}</span></td>
          <td>${renderStatusBadge(s.status)}</td>
          ${renderPagesCell(s)}
          <td class="num">${formatNumber(s.total_words)}</td>
          <td class="mono">${escapeHtml(formatTimestamp(s.last_synced_at))}</td>
          <td class="actions-col">
            <div class="action-group">
              <button type="button" class="btn btn-secondary btn-sm" data-action="sync" data-id="${s.id}" data-name="${escapeHtml(s.name)}">Sync</button>
              <button type="button" class="btn btn-secondary btn-sm" data-action="browse" data-id="${s.id}">Browse</button>
              ${removeControls}
            </div>
          </td>
        </tr>`;
      })
      .join("");
  }

  async function handleSourcesTableClick(event) {
    const btn = event.target.closest("button[data-action]");
    if (!btn) return;

    const action = btn.getAttribute("data-action");
    const id = Number(btn.getAttribute("data-id"));

    if (action === "browse") {
      navigateToBrowser(id, "");
      return;
    }

    if (action === "browse-status") {
      const status = btn.getAttribute("data-status") || "";
      navigateToBrowser(id, status);
      return;
    }

    if (action === "sync") {
      const name = btn.getAttribute("data-name") || String(id);
      btn.disabled = true;
      try {
        await apiRequest(`/api/sources/${id}/sync`, { method: "POST" });
        showToast(`Sync started for ${name}`);
        window.setTimeout(() => void loadSourcesView(), 600);
      } catch (err) {
        showToast(err.message, true);
      } finally {
        btn.disabled = false;
      }
      return;
    }

    if (action === "ask-delete") {
      state.confirmDeleteId = id;
      renderSourcesTable(state.sources);
      return;
    }

    if (action === "cancel-delete") {
      state.confirmDeleteId = null;
      renderSourcesTable(state.sources);
      return;
    }

    if (action === "confirm-delete") {
      btn.disabled = true;
      try {
        const removed = await apiRequest(`/api/sources/${id}`, { method: "DELETE" });
        state.confirmDeleteId = null;
        showToast(`Removed ${removed.name} (${removed.pages_deleted} pages)`);
        await loadSourcesView();
      } catch (err) {
        showToast(err.message, true);
      }
    }
  }

  // Add Source Modal
  function setAddSourceKind(kind) {
    state.addSourceKind = kind === "local" ? "local" : "web";
    document.querySelectorAll("#add-source-dialog .segmented-btn").forEach((b) => {
      const active = b.getAttribute("data-kind") === state.addSourceKind;
      b.classList.toggle("active", active);
      b.setAttribute("aria-pressed", String(active));
    });
    const label = el("add-origin-label");
    const input = el("add-origin");
    if (state.addSourceKind === "local") {
      label.textContent = "Local Markdown Folder Path";
      input.placeholder = "/path/to/markdown/folder";
    } else {
      label.textContent = "Documentation URL";
      input.placeholder = "docs.example.com/guide (include scheme)";
    }
    document.querySelectorAll("#add-source-dialog .web-only-field").forEach((node) => {
      const hide = state.addSourceKind === "local";
      node.hidden = hide;
      // A hidden control must not take part in form validation: the browser cannot show
      // an error on a field it does not render, so the submit would silently do nothing.
      node.querySelectorAll("input, select, textarea").forEach((input) => {
        input.disabled = hide;
      });
    });
  }

  function openAddSourceModal() {
    const dialog = el("add-source-dialog");
    const errorBox = el("add-source-error");
    el("add-source-form").reset();
    if (errorBox) {
      errorBox.hidden = true;
      errorBox.textContent = "";
    }
    setAddSourceKind("web");
    if (dialog && typeof dialog.showModal === "function") {
      dialog.showModal();
    }
  }

  function closeAddSourceModal() {
    const dialog = el("add-source-dialog");
    if (dialog && dialog.open) {
      dialog.close();
    }
  }

  async function handleAddSourceSubmit(event) {
    event.preventDefault();
    const errorBox = el("add-source-error");
    const submitBtn = el("btn-submit-add-source");
    errorBox.hidden = true;

    const origin = el("add-origin").value.trim();
    const name = el("add-name").value.trim();
    const tagsRaw = el("add-tags").value.trim();
    const intervalVal = Number(el("add-interval").value);
    const maxPagesVal = Number(el("add-max-pages").value);
    const maxDepthVal = Number(el("add-max-depth").value);

    const payload = {
      origin,
      sync_now: true,
    };
    if (name) payload.name = name;
    if (tagsRaw) {
      payload.tags = tagsRaw
        .split(",")
        .map((t) => t.trim())
        .filter(Boolean);
    }
    if (Number.isFinite(intervalVal) && intervalVal >= 0) {
      payload.refresh_interval_secs = intervalVal;
    }
    if (state.addSourceKind === "web") {
      if (Number.isFinite(maxPagesVal) && maxPagesVal > 0) {
        payload.max_pages = maxPagesVal;
      }
      if (Number.isFinite(maxDepthVal) && maxDepthVal > 0) {
        payload.max_depth = maxDepthVal;
      }
    }

    submitBtn.disabled = true;
    submitBtn.textContent = "Adding…";
    try {
      const created = await apiRequest("/api/sources", {
        method: "POST",
        body: payload,
      });
      closeAddSourceModal();
      showToast(`Added source ${created.source.name}`);
      await loadSourcesView();
    } catch (err) {
      errorBox.textContent = err.message;
      errorBox.hidden = false;
    } finally {
      submitBtn.disabled = false;
      submitBtn.textContent = "Add & Sync";
    }
  }

  // View 2: Knowledge Browser
  function populateBrowserSourceSelect() {
    const select = el("browser-source-select");
    if (!select) return;
    const previous = select.value;
    const options = ['<option value="">All sources</option>'].concat(
      state.sources.map(
        (s) =>
          `<option value="${s.id}">${escapeHtml(s.name)} (${escapeHtml(s.kind)})</option>`
      )
    );
    select.innerHTML = options.join("");
    if (previous && state.sources.some((s) => String(s.id) === previous)) {
      select.value = previous;
    }
  }

  async function loadBrowserView() {
    if (!state.sources.length) {
      try {
        await fetchSources();
      } catch {
        // Handled below
      }
    }
    const params = parseHashParams();
    const sourceParam = params.get("source");
    const statusParam = params.get("status");
    const sourceSelect = el("browser-source-select");
    const statusSelect = el("browser-status-select");

    if (sourceParam && sourceSelect && state.sources.some((s) => String(s.id) === sourceParam)) {
      sourceSelect.value = sourceParam;
    }
    if (statusSelect) {
      if (statusParam && VALID_PAGE_STATUSES.includes(statusParam)) {
        statusSelect.value = statusParam;
      } else if (sourceParam && !params.has("status")) {
        statusSelect.value = "";
      }
    }
    await searchBrowserPages();
  }

  async function searchBrowserPages() {
    const sourceId = el("browser-source-select").value;
    const status = el("browser-status-select").value;
    const q = el("browser-search-input").value.trim();
    const listEl = el("browser-page-list");
    const countEl = el("browser-page-count");
    // Only the latest search may render: a slower, superseded response is dropped.
    const seq = ++state.browserSearchSeq;

    try {
      const params = new URLSearchParams();
      if (sourceId) params.set("source", sourceId);
      if (status) params.set("status", status);
      if (q) params.set("q", q);
      const qs = params.toString();
      const list = await fetchPageList(`/api/pages${qs ? `?${qs}` : ""}`);
      if (seq !== state.browserSearchSeq) return;

      state.browserPages = list.pages;
      countEl.textContent = formatPageCount(list);
      renderBrowserPageList();

      if (state.browserPages.length) {
        const keepCurrent = state.browserPages.some((p) => p.id === state.selectedPageId);
        const nextId = keepCurrent ? state.selectedPageId : state.browserPages[0].id;
        await loadPageDetail(nextId);
      } else {
        state.selectedPageId = null;
        el("browser-detail").innerHTML =
          '<div class="empty-state">No pages matched the current filter.</div>';
      }
    } catch (err) {
      if (seq !== state.browserSearchSeq) return;
      listEl.innerHTML = `<div class="empty-state">Failed to load pages: ${escapeHtml(err.message)}</div>`;
    }
  }

  function renderBrowserPageList() {
    const listEl = el("browser-page-list");
    if (!state.browserPages.length) {
      listEl.innerHTML = '<div class="empty-state">No pages found.</div>';
      return;
    }

    listEl.innerHTML = state.browserPages
      .map((p) => {
        const selectedClass = p.id === state.selectedPageId ? " selected" : "";
        const httpMeta =
          p.status === "error" && p.http_status !== null && p.http_status !== undefined
            ? `<span class="mono">HTTP ${escapeHtml(p.http_status)}</span>`
            : "";
        return `<button type="button" class="page-item${selectedClass}" data-page-id="${p.id}" role="option" aria-selected="${p.id === state.selectedPageId}">
          <span class="page-item-title">${escapeHtml(p.title || p.uri)}</span>
          <span class="page-item-uri">${escapeHtml(p.uri)}</span>
          <span class="page-item-meta">
            ${renderStatusBadge(p.status)}
            ${httpMeta}
            <span>${formatNumber(p.word_count)} words</span>
          </span>
        </button>`;
      })
      .join("");
  }

  async function loadPageDetail(pageId) {
    state.selectedPageId = pageId;
    renderBrowserPageList();
    const detailEl = el("browser-detail");

    try {
      const page = await apiRequest(`/api/pages/${pageId}`);
      renderPageDetail(page);
    } catch (err) {
      detailEl.innerHTML = `<div class="empty-state">Failed to load page: ${escapeHtml(err.message)}</div>`;
    }
  }

  // An Error page is one whose `pages.status` is "error", the same rule as the
  // server's counts and status filter.
  function renderPageStatusBanner(page) {
    const status = String(page.status || "").toLowerCase();
    const errMsg = typeof page.error_message === "string" ? page.error_message.trim() : "";
    const hasHttp = page.http_status !== null && page.http_status !== undefined;

    if (status === "error") {
      const parts = [];
      const httpPrefix = hasHttp ? `HTTP ${page.http_status}` : "";
      if (httpPrefix && !errMsg.startsWith(httpPrefix)) parts.push(httpPrefix);
      if (errMsg) parts.push(errMsg);
      const detail = parts.length ? parts.join(" — ") : "Last fetch failed.";
      return `<div class="alert alert-error detail-status-banner" role="alert"><strong>Fetch error:</strong> ${escapeHtml(detail)}</div>`;
    }

    if (status === "stale") {
      return '<div class="alert alert-warn detail-status-banner" role="status"><strong>Stale page:</strong> No longer fetched on the last sync; last good copy is kept.</div>';
    }

    return "";
  }

  function renderPageDetail(page) {
    const detailEl = el("browser-detail");
    const sections = Array.isArray(page.sections) ? page.sections : [];
    const mode = state.detailMode;

    const sectionsHtml = sections.length
      ? sections
          .map(
            (sec) => `<div class="section-card">
              <div class="section-card-header">
                <span class="section-breadcrumb">${escapeHtml(sec.heading_path || sec.heading || "Intro")}</span>
                <span class="muted-meta">${formatNumber(sec.word_count)} words</span>
              </div>
              <pre class="md-block">${escapeHtml(sec.content_md)}</pre>
            </div>`
          )
          .join("")
      : '<div class="empty-state">No heading sections extracted for this page.</div>';

    const contentHtml =
      mode === "full"
        ? `<pre class="md-block">${escapeHtml(page.content_md)}</pre>`
        : sectionsHtml;

    detailEl.innerHTML = `
      <div class="detail-header">
        <div class="detail-title-row">
          <h2 class="detail-title">${escapeHtml(page.title || page.uri)}</h2>
          <div class="action-group">
            <button type="button" class="btn btn-sm ${mode === "sections" ? "btn-primary" : "btn-secondary"}" data-detail-mode="sections" aria-pressed="${mode === "sections"}">Sections (${sections.length})</button>
            <button type="button" class="btn btn-sm ${mode === "full" ? "btn-primary" : "btn-secondary"}" data-detail-mode="full" aria-pressed="${mode === "full"}">Full Markdown</button>
          </div>
        </div>
        <div class="source-origin">${escapeHtml(page.uri)}</div>
        <div class="detail-meta-row">
          <span class="badge badge-kind">${escapeHtml(page.source_name)}</span>
          ${renderStatusBadge(page.status)}
          <span>${formatNumber(page.word_count)} words</span>
          <span class="mono">Changed: ${escapeHtml(formatTimestamp(page.last_changed_at))}</span>
        </div>
        ${renderPageStatusBanner(page)}
      </div>
      <div class="detail-body">${contentHtml}</div>
    `;

    detailEl.querySelectorAll("button[data-detail-mode]").forEach((btn) => {
      btn.addEventListener("click", () => {
        state.detailMode = btn.getAttribute("data-detail-mode") || "sections";
        renderPageDetail(page);
      });
    });
  }

  // View 3: Changes & Health
  async function loadChangesView() {
    await Promise.all([loadRecentChanges(), loadHealthPages()]);
  }

  async function loadRecentChanges() {
    const tbody = el("changes-tbody");
    const countEl = el("changes-count");
    const diffViewer = el("diff-viewer");
    const diffMeta = el("diff-meta");

    try {
      const res = await apiRequest("/api/query", {
        method: "POST",
        body: {
          sql: "SELECT page_id, source_name, uri, title, changed_at, unified_diff FROM v_recent_changes LIMIT 50",
          limit: 50,
        },
      });
      const rows = (res && Array.isArray(res.rows) ? res.rows : []).map((r) => ({
        page_id: r[0],
        source_name: r[1],
        uri: r[2],
        title: r[3],
        changed_at: r[4],
        unified_diff: r[5],
      }));

      state.changesRows = rows;
      countEl.textContent = String(rows.length);

      if (!rows.length) {
        tbody.innerHTML =
          '<tr><td colspan="3" class="empty-cell">No page revisions recorded yet. Content diffs appear here after a page changes on sync.</td></tr>';
        diffMeta.textContent = "No revisions";
        diffViewer.innerHTML =
          '<div class="empty-state">No diffs available yet.</div>';
        return;
      }

      const keepSelected = rows.some((r) => r.page_id === state.selectedChangePageId);
      state.selectedChangePageId = keepSelected
        ? state.selectedChangePageId
        : rows[0].page_id;

      renderChangesTable();
      const selected = rows.find((r) => r.page_id === state.selectedChangePageId);
      if (selected) {
        renderUnifiedDiff(selected);
      }
    } catch (err) {
      tbody.innerHTML = `<tr><td colspan="3" class="empty-cell">Failed to query changes: ${escapeHtml(err.message)}</td></tr>`;
    }
  }

  function renderChangesTable() {
    const tbody = el("changes-tbody");
    tbody.innerHTML = state.changesRows
      .map((row) => {
        const selected = row.page_id === state.selectedChangePageId;
        const cls = selected ? "clickable row-selected" : "clickable";
        return `<tr class="${cls}" data-change-page-id="${row.page_id}" tabindex="0" aria-selected="${selected}">
          <td>
            <div class="source-name">${escapeHtml(row.title || row.uri)}</div>
            <div class="source-origin">${escapeHtml(row.uri)}</div>
          </td>
          <td><span class="badge badge-kind">${escapeHtml(row.source_name)}</span></td>
          <td class="mono">${escapeHtml(formatTimestamp(row.changed_at))}</td>
        </tr>`;
      })
      .join("");
  }

  function renderUnifiedDiff(change) {
    const diffViewer = el("diff-viewer");
    const diffMeta = el("diff-meta");
    diffMeta.textContent = `${change.source_name} · ${change.uri}`;

    const raw = String(change.unified_diff || "");
    if (!raw.trim()) {
      diffViewer.innerHTML = '<div class="empty-state">Empty diff payload.</div>';
      return;
    }

    const lines = raw.split("\n");
    diffViewer.innerHTML = lines
      .map((line) => {
        let cls = "diff-line";
        if (line.startsWith("+++") || line.startsWith("---")) {
          cls += " diff-meta-line";
        } else if (line.startsWith("@@")) {
          cls += " diff-hunk";
        } else if (line.startsWith("+")) {
          cls += " diff-add";
        } else if (line.startsWith("-")) {
          cls += " diff-del";
        }
        return `<div class="${cls}">${escapeHtml(line || " ")}</div>`;
      })
      .join("");
  }

  async function loadHealthPages() {
    const tbody = el("health-tbody");
    const countEl = el("health-count");
    try {
      // The server orders before it cuts, so a cut list keeps the newest checks.
      const list = await fetchPageList("/api/pages?status=stale,error&order=checked");
      const rows = list.pages;
      countEl.textContent = formatPageCount(list);

      if (!rows.length) {
        tbody.innerHTML =
          '<tr><td colspan="6" class="empty-cell">All indexed pages are healthy and active.</td></tr>';
        return;
      }

      tbody.innerHTML = rows
        .map((p) => {
          const status = p.status || "stale";
          const httpStatus =
            p.http_status === null || p.http_status === undefined ? "—" : String(p.http_status);
          const errMsg = p.error_message
            ? escapeHtml(p.error_message)
            : '<span class="null-val">none</span>';
          return `<tr>
            <td>${renderStatusBadge(status)}</td>
            <td><span class="badge badge-kind">${escapeHtml(p.source_name)}</span></td>
            <td class="mono">${escapeHtml(p.uri)}</td>
            <td class="mono">${escapeHtml(httpStatus)}</td>
            <td>${errMsg}</td>
            <td class="mono">${escapeHtml(formatTimestamp(p.last_checked_at))}</td>
          </tr>`;
        })
        .join("");
    } catch (err) {
      tbody.innerHTML = `<tr><td colspan="6" class="empty-cell">Failed to query health: ${escapeHtml(err.message)}</td></tr>`;
    }
  }

  // View 4: SQL Explorer
  function initSqlPresets() {
    const bar = el("sql-presets");
    const input = el("sql-input");
    if (!bar || !input) return;

    bar.innerHTML = SQL_PRESETS.map(
      (preset, idx) =>
        `<button type="button" class="preset-chip" data-preset-idx="${idx}">${escapeHtml(preset.label)}</button>`
    ).join("");

    bar.addEventListener("click", (event) => {
      const btn = event.target.closest("button[data-preset-idx]");
      if (!btn) return;
      const idx = Number(btn.getAttribute("data-preset-idx"));
      const preset = SQL_PRESETS[idx];
      if (!preset) return;
      input.value = preset.sql;
      void runSqlQuery();
    });

    if (!input.value.trim()) {
      input.value = SQL_PRESETS[0].sql;
    }
  }

  async function loadSqlView() {
    if (!state.sqlHasRun) {
      state.sqlHasRun = true;
      await runSqlQuery();
    }
  }

  async function runSqlQuery() {
    const sql = el("sql-input").value.trim();
    const limit = Number(el("sql-limit").value) || 50;
    const errorEl = el("sql-error");
    const truncEl = el("sql-truncation");
    const metaEl = el("sql-meta");
    const wrapEl = el("sql-results-wrap");

    errorEl.hidden = true;
    truncEl.hidden = true;

    if (!sql) {
      errorEl.textContent = "Enter a read-only SELECT query.";
      errorEl.hidden = false;
      return;
    }

    metaEl.textContent = "Running…";
    try {
      const res = await apiRequest("/api/query", {
        method: "POST",
        body: { sql, limit },
      });

      metaEl.textContent = `${formatNumber(res.row_count)} rows · ${res.elapsed_ms} ms`;

      if (res.truncated && res.truncation_hint) {
        truncEl.textContent = res.truncation_hint;
        truncEl.hidden = false;
      }

      renderSqlResultsTable(res.columns || [], res.rows || [], wrapEl);
    } catch (err) {
      metaEl.textContent = "Error";
      errorEl.textContent = err.message;
      errorEl.hidden = false;
      wrapEl.innerHTML = '<div class="empty-state">Query returned an error.</div>';
    }
  }

  function renderSqlResultsTable(columns, rows, container) {
    if (!columns.length) {
      container.innerHTML = '<div class="empty-state">Query returned no columns.</div>';
      return;
    }
    if (!rows.length) {
      container.innerHTML = '<div class="empty-state">Query returned 0 rows.</div>';
      return;
    }

    const headHtml = columns.map((c) => `<th>${escapeHtml(c)}</th>`).join("");
    const bodyHtml = rows
      .map((row) => {
        const cells = row
          .map((cell) => {
            if (cell === null || cell === undefined) {
              return '<td><span class="null-val">NULL</span></td>';
            }
            const text =
              typeof cell === "object" ? JSON.stringify(cell) : String(cell);
            return `<td class="mono">${escapeHtml(text)}</td>`;
          })
          .join("");
        return `<tr>${cells}</tr>`;
      })
      .join("");

    container.innerHTML = `<table class="data-table"><thead><tr>${headHtml}</tr></thead><tbody>${bodyHtml}</tbody></table>`;
  }

  // Event Wiring
  function initEvents() {
    el("btn-refresh-sources").addEventListener("click", () => void loadSourcesView());
    el("btn-open-add-source").addEventListener("click", openAddSourceModal);
    el("btn-close-add-source").addEventListener("click", closeAddSourceModal);
    el("btn-cancel-add-source").addEventListener("click", closeAddSourceModal);
    el("add-source-form").addEventListener("submit", (e) => void handleAddSourceSubmit(e));

    document.querySelectorAll("#add-source-dialog .segmented-btn").forEach((btn) => {
      btn.addEventListener("click", () => {
        setAddSourceKind(btn.getAttribute("data-kind"));
      });
    });

    el("sources-tbody").addEventListener("click", (e) => void handleSourcesTableClick(e));

    el("browser-filter-form").addEventListener("submit", (e) => {
      e.preventDefault();
      void searchBrowserPages();
    });
    el("browser-source-select").addEventListener("change", () => void searchBrowserPages());
    el("browser-status-select").addEventListener("change", () => void searchBrowserPages());

    el("browser-page-list").addEventListener("click", (event) => {
      const item = event.target.closest("button[data-page-id]");
      if (!item) return;
      const pageId = Number(item.getAttribute("data-page-id"));
      if (Number.isFinite(pageId)) {
        void loadPageDetail(pageId);
      }
    });

    function selectChangeRow(row) {
      if (!row) return;
      const pageId = Number(row.getAttribute("data-change-page-id"));
      const found = state.changesRows.find((r) => r.page_id === pageId);
      if (found) {
        state.selectedChangePageId = found.page_id;
        renderChangesTable();
        renderUnifiedDiff(found);
      }
    }

    el("btn-refresh-changes").addEventListener("click", () => void loadChangesView());
    el("changes-tbody").addEventListener("click", (event) => {
      selectChangeRow(event.target.closest("tr[data-change-page-id]"));
    });
    el("changes-tbody").addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        const row = event.target.closest("tr[data-change-page-id]");
        if (row) {
          event.preventDefault();
          selectChangeRow(row);
        }
      }
    });

    el("sql-form").addEventListener("submit", (e) => {
      e.preventDefault();
      void runSqlQuery();
    });
    el("sql-input").addEventListener("keydown", (e) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
        e.preventDefault();
        void runSqlQuery();
      }
    });

    window.addEventListener("hashchange", () => {
      setActiveView(parseHashView(), true);
    });
  }

  function boot() {
    initTheme();
    initUpdatePopover();
    initPagesTooltip();
    initSqlPresets();
    initEvents();
    void loadVersion();

    if (!window.location.hash) {
      window.history.replaceState(null, "", "#sources");
    }
    setActiveView(parseHashView(), true);
  }

  document.addEventListener("DOMContentLoaded", boot);
})();
