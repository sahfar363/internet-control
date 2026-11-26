/* Internet Control v0.4 UI.
 *
 * Polls GET /api/state every 500 ms and renders it. The app list is
 * keyed by exe path and updated IN PLACE: rows are never re-sorted
 * live. Sorting happens only when the user clicks a column header or
 * flips the Rate / Total toggle. Blocked apps are pinned to the top
 * (the server already returns them first, in stable order).
 */
(function () {
  "use strict";

  var DOWN = "#50b4ff";
  var UP = "#ffaa3c";
  var POLL_MS = 500;

  var state = null;          // latest /api/state payload
  var appOrder = [];         // exe paths in display order (stable)
  var appData = {};          // exe path -> latest app row
  var sortedBy = null;       // null | "rate" | "total" | "name"
  var currentTab = "overview";

  function $(id) { return document.getElementById(id); }
  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined && text !== null) n.textContent = text;
    return n;
  }
  function post(path, body) {
    return fetch(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body || {}),
    }).then(function (r) {
      if (!r.ok) return r.json().then(function (j) {
        throw new Error((j && j.error) || ("request failed: " + r.status));
      });
      return r.json();
    });
  }

  /* ---- tabs ---- */
  var tabs = document.querySelectorAll(".tab");
  tabs.forEach(function (t) {
    t.addEventListener("click", function () {
      currentTab = t.getAttribute("data-tab");
      tabs.forEach(function (x) { x.classList.toggle("active", x === t); });
      document.querySelectorAll(".tabpage").forEach(function (p) {
        p.classList.toggle("active", p.id === "tab-" + currentTab);
      });
    });
  });

  /* ---- overview ---- */
  function drawGraph() {
    var canvas = $("graph");
    if (!state) return;
    var dpr = window.devicePixelRatio || 1;
    var w = canvas.clientWidth, h = canvas.clientHeight;
    if (!w || !h) return;
    if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
      canvas.width = w * dpr;
      canvas.height = h * dpr;
    }
    var ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    var down = state.history_down || [];
    var up = state.history_up || [];
    var max = 1;
    for (var i = 0; i < down.length; i++) {
      if (down[i] > max) max = down[i];
      if (up[i] > max) max = up[i];
    }
    function line(values, color) {
      if (values.length < 2) return;
      ctx.strokeStyle = color;
      ctx.lineWidth = 1.5;
      ctx.beginPath();
      for (var i = 0; i < values.length; i++) {
        var x = (i / 599) * w;
        var y = h - (values[i] / max) * (h - 8) - 2;
        if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
      }
      ctx.stroke();
    }
    line(down, DOWN);
    line(up, UP);
    ctx.fillStyle = "#8b94a3";
    ctx.font = "11px sans-serif";
    ctx.fillText("peak in graph: " + fmtRate(max), 6, 14);
  }

  function fmtRate(v) {
    if (v < 1024) return Math.round(v) + " B/s";
    if (v < 1024 * 1024) return (v / 1024).toFixed(2) + " KB/s";
    if (v < 1024 * 1024 * 1024) return (v / 1048576).toFixed(2) + " MB/s";
    return (v / 1073741824).toFixed(2) + " GB/s";
  }

  /* ---- apps table: stable order, update in place ---- */
  var appRows = {};        // exe path -> { tr, nameEl, downEl, upEl, downTotEl, upTotEl, btn, badge }
  function sortOrder(key) {
    sortedBy = key;
    var rows = appOrder.map(function (exe) { return appData[exe]; }).filter(Boolean);
    if (key === "rate") {
      rows.sort(function (a, b) { return (b.down_bps + b.up_bps) - (a.down_bps + a.up_bps) || a.name.localeCompare(b.name); });
    } else if (key === "total") {
      rows.sort(function (a, b) { return (b.down_total + b.up_total) - (a.down_total + a.up_total) || a.name.localeCompare(b.name); });
    } else if (key === "name") {
      rows.sort(function (a, b) { return a.name.localeCompare(b.name); });
    }
    appOrder = rows.map(function (r) { return r.exe_path; });
  }

  function renderApps() {
    var body = $("appsBody");
    // Merge latest data into the stable order: new exes append at
    // the end, gone exes drop out. Values update in place — the
    // order array is only re-sorted by sortOrder() (header click /
    // view toggle), never here.
    var live = {};
    (state.apps || []).forEach(function (a) {
      live[a.exe_path] = true;
      appData[a.exe_path] = a;
      if (appOrder.indexOf(a.exe_path) === -1) appOrder.push(a.exe_path);
    });
    appOrder = appOrder.filter(function (exe) { return live[exe]; });
    Object.keys(appData).forEach(function (exe) { if (!live[exe]) delete appData[exe]; });

    // Blocked rows pinned to the top (stable partition, not a sort).
    var ordered = appOrder.slice();
    ordered.sort(function (a, b) {
      var ba = appData[a] && appData[a].blocked ? 0 : 1;
      var bb = appData[b] && appData[b].blocked ? 0 : 1;
      return ba - bb;
    });

    // Update rows IN PLACE: existing <tr> nodes are never rebuilt,
    // so a Block button under the mouse cannot vanish mid-click.
    // Only values, badges and disabled state change; new rows are
    // inserted at their position, gone rows removed.
    var seen = {};
    ordered.forEach(function (exe, idx) {
      var a = appData[exe];
      seen[exe] = true;
      var row = appRows[exe];
      if (!row) {
        var tr = el("tr");
        var nameTd = el("td");
        var name = el("span", "app-name", "");
        var sub = el("span", "app-sub", "");
        var badge = el("span", "badge blocked", "");
        badge.style.marginLeft = "8px";
        badge.style.display = "none";
        nameTd.appendChild(name);
        nameTd.appendChild(sub);
        nameTd.appendChild(badge);
        tr.appendChild(nameTd);
        var downEl = el("td", "num", "");
        var upEl = el("td", "num", "");
        var downTotEl = el("td", "num", "");
        var upTotEl = el("td", "num", "");
        tr.appendChild(downEl);
        tr.appendChild(upEl);
        tr.appendChild(downTotEl);
        tr.appendChild(upTotEl);
        var actTd = el("td");
        var btn = el("button", "btn block", "Block");
        btn.type = "button";
        btn.addEventListener("click", function () {
          var app = appData[exe];
          if (!app) return;
          btn.disabled = true;
          post("/api/block", { exe: exe, block: !app.blocked })
            .catch(function (e) { alert(e.message); })
            .finally(function () { btn.disabled = false; poll(); });
        });
        actTd.appendChild(btn);
        tr.appendChild(actTd);
        row = { tr: tr, nameEl: name, subEl: sub, badge: badge, downEl: downEl, upEl: upEl, downTotEl: downTotEl, upTotEl: upTotEl, btn: btn };
        appRows[exe] = row;
      }
      // Place the row at its position in the stable order.
      var next = null;
      for (var j = idx + 1; j < ordered.length; j++) {
        if (appRows[ordered[j]]) { next = appRows[ordered[j]].tr; break; }
      }
      if (row.tr.parentNode !== body || row.tr.nextSibling !== next) {
        body.insertBefore(row.tr, next);
      }
      row.tr.className = a.blocked ? "app-row-blocked" : "";
      row.nameEl.textContent = a.name;
      row.nameEl.className = "app-name" + (a.blocked ? " blocked" : "");
      row.subEl.textContent = a.exe_path;
      if (a.blocked) {
        row.badge.style.display = "";
        row.badge.textContent = a.block_state === "blocked_new_connections_only" ? "BLOCKED — NEW CONNECTIONS ONLY" : "BLOCKED";
        row.badge.className = "badge " + (a.block_state === "blocked_new_connections_only" ? "newonly" : "blocked");
      } else {
        row.badge.style.display = "none";
      }
      row.downEl.textContent = a.down;
      row.upEl.textContent = a.up;
      row.downTotEl.textContent = a.down_total_fmt;
      row.upTotEl.textContent = a.up_total_fmt;
      row.btn.textContent = a.blocked ? "Unblock" : "Block";
      row.btn.className = "btn " + (a.blocked ? "unblock" : "block");
      if (!state.can_block && !a.blocked) {
        row.btn.disabled = true;
        row.btn.title = "Blocking needs WinDivert intercept mode — restart the app so WinDivert can open in intercept mode.";
      } else {
        row.btn.disabled = false;
        row.btn.title = "";
      }
    });
    Object.keys(appRows).forEach(function (exe) {
      if (!seen[exe]) {
        appRows[exe].tr.remove();
        delete appRows[exe];
      }
    });
    $("appsEmpty").style.display = ordered.length ? "none" : "";
  }

  /* ---- adapters table ---- */
  function renderAdapters() {
    var body = $("adaptersBody");
    body.innerHTML = "";
    (state.adapters || []).forEach(function (a) {
      var tr = el("tr");
      var nameTd = el("td");
      nameTd.appendChild(el("span", null, (a.selected ? "▶ " : "") + a.name));
      if (a.unknown) {
        var b = el("span", "badge unknown", "UNKNOWN");
        b.style.marginLeft = "8px";
        nameTd.appendChild(b);
      }
      tr.appendChild(nameTd);
      tr.appendChild(el("td", null, a.type_name));
      tr.appendChild(el("td", null, a.status));
      tr.appendChild(el("td", "mono", a.ip || "—"));
      tr.appendChild(el("td", "num", a.down));
      tr.appendChild(el("td", "num", a.up));
      tr.appendChild(el("td", "num", a.total));
      var actTd = el("td");
      if (a.selected) {
        actTd.appendChild(el("span", "muted small", "selected"));
      } else {
        var btn = el("button", "btn", "Select");
        btn.type = "button";
        btn.addEventListener("click", function () {
          btn.disabled = true;
          post("/api/select", { luid: a.luid }).finally(function () { poll(); });
        });
        actTd.appendChild(btn);
      }
      tr.appendChild(actTd);
      body.appendChild(tr);
    });
  }

  /* ---- diagnostics ---- */
  function renderDiagnostics() {
    var d = state.diagnostics || {};
    $("diagCapture").textContent = d.capture_label || state.capture_label || "—";
    $("diagPackets").textContent = String(d.packets || 0);
    $("diagAdapters").textContent = String(d.adapters_enumerated || 0);
    $("diagSelected").textContent = state.selected_name +
      (state.selected_luid !== null && state.selected_luid !== undefined ? "  (LUID " + state.selected_luid + ")" : "");
    $("diagTopSource").textContent = d.top_source || "—";
    $("diagFallback").classList.toggle("hidden", !d.fallback_active);
    var tb = $("diagTotals");
    tb.innerHTML = "";
    var totals = d.capture_totals || [];
    if (!totals.length) {
      var tr = el("tr");
      tr.appendChild(el("td", "muted", "No capture data yet."));
      tr.firstChild.setAttribute("colspan", "3");
      tb.appendChild(tr);
    } else {
      totals.forEach(function (t) {
        var tr = el("tr");
        tr.appendChild(el("td", "mono", "Interface " + t.index));
        tr.appendChild(el("td", "num", fmtBytes(t.down)));
        tr.appendChild(el("td", "num", fmtBytes(t.up)));
        tb.appendChild(tr);
      });
    }
    $("diagNote").textContent = state.selection_note || "";
  }

  function fmtBytes(v) {
    if (v < 1024) return v + " B";
    if (v < 1024 * 1024) return (v / 1024).toFixed(2) + " KB";
    if (v < 1024 * 1024 * 1024) return (v / 1048576).toFixed(2) + " MB";
    return (v / 1073741824).toFixed(2) + " GB";
  }

  /* ---- render whole state ---- */
  function render() {
    if (!state) return;
    $("version").textContent = "v" + state.version;
    document.body.classList.toggle("minimal", !!state.minimal);
    $("minimalBtn").textContent = state.minimal ? "Full view" : "Minimal";
    $("startup").checked = !!state.run_on_startup;

    var src = $("sourceLabel");
    src.textContent = state.capture_label;
    src.parentElement.classList.toggle("fallback", !!(state.top && state.top.note));

    var note = $("topNote");
    if (state.top && state.top.note) {
      note.textContent = state.top.note + " (Source: " + state.top.source + ")";
      note.classList.remove("hidden");
    } else {
      note.classList.add("hidden");
    }

    $("downRate").textContent = state.top.down;
    $("upRate").textContent = state.top.up;
    $("sessionDown").textContent = fmtBytes(state.session.down);
    $("sessionUp").textContent = fmtBytes(state.session.up);
    $("todayDown").textContent = fmtBytes(state.today.down);
    $("todayUp").textContent = fmtBytes(state.today.up);
    $("peakDown").textContent = state.peak_down_fmt;
    $("peakUp").textContent = state.peak_up_fmt;
    $("selectedName").textContent = state.selected_name;
    $("selectedIp").textContent = state.selected_ip;
    $("minimalIpVal").textContent = state.selected_ip;
    $("minimalAdapter").textContent = state.selected_name;

    var bn = $("blockNote");
    if (!state.can_block) {
      bn.textContent = "Blocking is unavailable: WinDivert is not in intercept mode (sniff/counter mode). Restart the app as administrator so WinDivert can open in intercept mode.";
      bn.classList.remove("hidden");
    } else {
      bn.classList.add("hidden");
    }

    // View toggle state.
    $("viewRate").classList.toggle("active", state.app_view === "rate");
    $("viewTotal").classList.toggle("active", state.app_view === "total");

    var err = $("error");
    if (state.error) {
      err.textContent = state.error;
      err.classList.remove("hidden");
    } else {
      err.classList.add("hidden");
    }

    drawGraph();
    renderApps();
    renderAdapters();
    renderDiagnostics();
  }

  /* ---- controls ---- */
  $("minimalBtn").addEventListener("click", function () {
    if (!state) return;
    post("/api/minimal", { enabled: !state.minimal }).then(poll);
  });
  $("startup").addEventListener("change", function (e) {
    post("/api/startup", { enabled: e.target.checked }).then(poll);
  });
  $("viewRate").addEventListener("click", function () {
    if (!state || state.app_view === "rate") return;
    sortOrder("rate");
    post("/api/view", { view: "rate" }).then(poll);
    renderApps();
  });
  $("viewTotal").addEventListener("click", function () {
    if (!state || state.app_view === "total") return;
    sortOrder("total");
    post("/api/view", { view: "total" }).then(poll);
    renderApps();
  });
  document.querySelectorAll(".th-btn").forEach(function (b) {
    b.addEventListener("click", function () {
      var key = b.getAttribute("data-sort");
      sortOrder(key);
      post("/api/sort", { sort: key }).then(poll);
      renderApps();
    });
  });

  /* ---- poll loop ---- */
  function poll() {
    fetch("/api/state")
      .then(function (r) { return r.json(); })
      .then(function (s) { state = s; render(); })
      .catch(function () { /* server briefly busy; next tick retries */ });
  }
  poll();
  setInterval(poll, POLL_MS);
})();
