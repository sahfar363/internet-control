# Internet Control — Usage Meter (v0.9.0)

A Windows 10/11 (x64) internet usage meter in Rust with a native
egui UI (complete Rust, no WebView2/Edge) that shows **real, precise
upload and download** — including over PDAnet phone tethering + VPN,
where many other apps show 0 or double-count — plus a per-app list
with block/unblock, like NetLimiter.

This is step 2 of the Internet Control project. The meter is proven;
v0.4 rebuilds the UI on WebView2 and fixes the v0.3 top-numbers bug.
Speed limiting per app is the next step.

## What v0.9.0 changes (borderless window, public IP + country, Minimal graph, UI scaling)

Sahand tested v0.7.0 and asked for four things:

- **Borderless window with in-app controls** — the Windows title
  bar ("the windows container on top of the app") is hidden, and
  the app draws its own slim title bar instead: title on the left,
  minimize / maximize / close buttons on the right, in both normal
  and Minimal views. Drag the title bar to move the window,
  double-click it to maximize/restore, and drag any edge or corner
  to resize (the window has no OS resize border any more, so the
  app provides invisible resize strips along all edges and
  corners). Closing the window still quits the app — blocking stops
  and traffic flows again, exactly as before. The buttons follow
  the app's dark/light theme.
- **Public IP + country instead of the local adapter IP** — the IP
  line used to show the local adapter IP (e.g. PDaNet's internal
  10.1.19.2). It now shows the public IP as the internet sees it —
  the VPN exit IP — with the country: "Public IP: 1.2.3.4
  (Country)", in both normal and Minimal views. A background
  thread looks it up roughly every 5 minutes from a what-is-my-ip
  service (ipapi.co, with ip-api.com as a fallback). While offline
  or when the lookup fails it shows "Public IP: unavailable" and
  falls back to the local adapter IP — the meter itself never needs
  the internet and never waits on this. The per-adapter local IPs
  on the Adapters tab are unchanged. Privacy note: the lookup
  service necessarily sees the public IP — that is what the
  service is for. Nothing else is sent: a plain GET, no body, no
  identifier, no account.
- **Graph lines back in Minimal** — v0.7.0 dropped the live
  60-second download/upload graph from the really-small Minimal
  window, and Sahand noticed. It is back: the same blue download /
  orange upload lines as the Overview graph, from the same
  history. The Minimal window grows a little to fit it (~360x260)
  and keeps the order: download/upload lines, graph, opacity
  slider, IP underneath.
- **Content scales when resizing, and the window can be made
  smaller** — resizing used to only make the frame bigger while
  the content stayed the same size, and the window could not go
  below 560x420. Now the whole UI scales with the window width
  (0.6x–1.5x against the design width), so text, rates, buttons
  and the graph all grow/shrink with the window, and the normal
  minimum is now 420x320. Scrolling still works for tall content.

## What v0.8.0 changes (exact file location per app, identify the Unknown traffic)

Sahand asked: "I want to be able to see the exact location of each
file that is using the internet. Also what is unauthorized? it should
be possible to find the app." v0.8.0 answers both:

- **Exact location of every app** — the Apps list still shows only
  the exe name (compact), but hovering a row now shows a tooltip
  with the full on-disk path, and clicking a row shows a details
  line under the list with the full path and a **Copy** button. The
  path is the exact location the engine captured (PID ->
  `QueryFullProcessImageName`), never a guess. The block note under
  the list now names the full path too ("Terminated 3 open TCP
  connections for C:\Program Files\...\chrome.exe").
- **Identify the "Unknown (unattributed)" row** — clicking that row
  opens a panel listing the flows the meter could not match to a
  program (at most the last 50, biggest first): protocol, local
  addr:port, remote addr:port, bytes, packets, last seen. Only
  unattributed traffic is recorded — nothing for attributed apps.
  An **Identify** button looks each sample up in the *current*
  connection tables (`GetExtendedTcpTable` /
  `GetExtendedUdpTable` -> PID -> exe path) and shows the owner
  next to it when found.
- **Honest labels for what cannot be owned** — ICMP samples are
  labelled "ICMP (ping/traceroute — no app owns this, the remote
  IP is shown)". Traffic from PID 4 (the Windows kernel) is no
  longer lumped under Unknown at all: it gets its own **System**
  row with path `C:\Windows\System32\ntoskrnl.exe`. That row
  cannot be blocked — dropping the kernel's packets would break
  networking, so it has no Block button (same for the Unknown row
  itself: there is no app to block). A sample whose connection has
  since closed, or that started after the sample was recorded,
  shows "no owner found right now" — the tables only know about
  right now, so the panel says so instead of guessing.
- The v0.5 fallback attribution (connection-table map rebuilt
  ~1/s) is unchanged — Identify is on top of it, for what still
  misses.

## What v0.7.0 changes (really-small Minimal, opacity slider, multi-adapter)

Sahand tested v0.6.0: it works, but Minimal did not really make
the window small — it only hid panels. v0.7.0 fixes that and adds
two more things he asked for:

- **Really-small Minimal** — clicking Minimal now shrinks the
  window itself to ~360x190: just the big download/upload rates,
  the opacity slider, and the IP underneath. No session totals, no
  graph, no tabs. Clicking **Full view** restores the window to
  the size it had before. The Minimal window is still resizable
  (down to 300x150).
- **Opacity slider** — a slider (20%–100%, saved in `config.json`)
  in the top bar in normal view and under the rates in Minimal
  view, that makes the whole window more or less transparent.
  On Windows it uses a layered window (`WS_EX_LAYERED` +
  `SetLayeredWindowAttributes`); at 100% the layered style is
  removed again so the window behaves exactly like a normal one.
- **Multi-adapter selection on the Adapters tab** — the top
  numbers no longer have to follow one adapter. Each adapter row
  has a checkbox, plus **Auto (smart switch)** (the default — the
  v0.4–v0.6 behaviour: one adapter, fallback to all interfaces
  when it goes dry, auto-switch to the busiest carrying interface)
  and **All** (sum of everything). Ticking boxes sums just those
  adapters; unticking all goes back to Auto. The selection is
  saved by adapter *name*, not LUID, so it survives PdaNet
  re-tethers. Yes, two adapters can be used at once — but traffic
  that crosses two of them (e.g. VPN over PdaNet) is counted
  twice, so the UI warns about it and Auto stays the default. The
  per-app list is unchanged (it is interface-agnostic), and the
  top numbers still never read 0 while traffic flows: a Named
  selection whose adapters are all dry falls back to all
  interfaces with a note.

## What v0.6.0 changes (back to native egui — no WebView2/Edge)

v0.4–v0.5.5 tried an Edge/WebView2 window, but on Sahand's PC the
embedded Edge process would not stay alive: every attempt failed
at `Navigate` with `0x8007139F`, even with a fresh profile,
`--disable-gpu` and `--no-sandbox` — while Edge itself opens
Google fine. The old egui window (v0.3.0) always opened fine, so
v0.6.0 goes back to a **native egui UI in complete Rust**
(`src/app.rs`, `eframe` 0.32). No Edge, no browser, no local HTTP
server, no `WebView2Loader.dll` in the zip. The WebView2 stack is
gone: `src/webview.rs`, `src/server.rs` and `assets/` are deleted,
and the `webview2-com` dependency is removed.

Nothing about the meter changes: same WinDivert engine, same
per-app attribution, same blocking — including terminating open
connections at once (v0.5) — same top-numbers fallback and
auto-switch, same Diagnostics. What the egui rebuild adds or
carries over from the WebView UI:

- **Dark / Light theme toggle at the top** (new) — switches the
  whole window between egui dark and light visuals; the choice is
  saved in `config.json` and restored next launch (dark by
  default, also for configs written before v0.6.0).
- **Tabs Overview / Apps / Adapters**, as in the WebView UI:
  Overview has the big numbers, the 60-second graph and the
  capture status; Apps has the per-app list with Block/Unblock,
  the Rate/Total toggle and clickable sort headers; Adapters has
  the adapter table (Select, IP, live rates, totals) and the
  Diagnostics (raw per-interface capture totals, packets seen,
  top-numbers source).
- **Fixed columns** — numeric columns have fixed widths and use a
  monospace font, so values never shift left/right as numbers
  change (the v0.3 complaint). The app list keeps the bridge's
  stable order: re-sorted only on header click or view toggle,
  blocked apps pinned to the top with a BLOCKED badge.
- **Scrolling** — the whole panel scrolls vertically, so nothing
  is cut off when the window is short; the tables scroll
  horizontally when the window is narrow.
- **Resizable Minimal window** — the window is resizable in both
  views (minimum 560×420 in normal view). Minimal shows only the
  big numbers, the IP and the live graph. (v0.7.0: Minimal now
  shrinks the window itself to ~360x190 with just rates, the
  opacity slider and the IP — no graph, no session totals.)
- **IP in both views** — the carrying adapter's IP is shown under
  the big numbers in normal and in Minimal view.
- The termination note ("Terminated 3 open TCP connections for
  C:\Program Files\...\chrome.exe") is shown under the Apps list, as before.
- `internet-control.log` next to the exe is kept (the exe still
  has no console window), with capture-status changes and Block
  actions.

## What v0.5.5 changes (escalating window attempts + why the Edge process dies)

v0.5.4's WM_QUIT drain worked — the retry now actually runs, no
more `TaskCanceled` — but on Sahand's PC BOTH attempts still fail
at `Navigate` with the same `0x8007139F`, with the runtime present
and current (158) and a fresh profile. So Edge's browser process
itself will not stay alive: most likely it crashes at startup (the
GPU process) or is blocked (sandbox / antivirus).

The window now escalates across up to three attempts, each with
its own fresh profile: plain, then `--disable-gpu`, then
`--disable-gpu --no-sandbox`. The last one is a last resort — the
page is local-only, so turning the sandbox off for a window that
otherwise cannot open at all is a diagnostic fallback, not a
general setting. Any failure other than `0x8007139F` still falls
back to the browser at once, with no further attempts, and the
banner history now shows which arguments each attempt used, e.g.
`… [args: --disable-gpu]`.

v0.5.5 also registers a `ProcessFailed` handler right after
getting the WebView2 control, so the log finally says why the
process died instead of guessing: `WebView2 process failed:
browser process exited (crashed, exit code -1073741819)` — kind
(`browser` / `GPU` / `render` / `sandbox helper` / …), reason
(`crashed` / `launch failed` / `out of memory` / …) and exit code.
If the event fires before `Navigate` returns, the note is also
appended to the failure reason shown in the banner. Registering
the handler is best-effort: if it fails, the window carries on
without it and the log says so.

## What v0.5.4 changes (the retry no longer dies at once — stale WM_QUIT drained)

v0.5.3's fresh-profile retry never got a chance to work. The
banner on Sahand's PC said `WebView2 controller creation failed:
TaskCanceled [WebView2 runtime 158.0.4370.33]` — and the runtime
version proves Edge itself is present and current, so this is
not a missing or broken Edge install.

`TaskCanceled` is what the `webview2-com` wait helper returns
when `GetMessage` retrieves WM_QUIT. The only place that posts
WM_QUIT is our own window procedure: attempt 1's window is
destroyed on a retryable failure, WM_DESTROY posts WM_QUIT, and
nobody pumps messages between the destroy and the retry — so the
stale quit message is still sitting in the thread's queue when
attempt 2 starts waiting for the controller, and the wait dies
at once. (Each attempt creates a fresh window — the retry never
reuses a destroyed HWND; the poison is the queued WM_QUIT. This
also explains the window flashing open for a split second and
then closing: that is attempt 1 being shown and then destroyed.)

So v0.5.4 drains stale WM_QUIT messages (`PeekMessageW` with a
WM_QUIT filter) before every window attempt, so each attempt
starts with a clean message queue. The retry also pauses ~500 ms
first, so a crashed Edge process can exit and release its locks,
and every attempt is logged (`WebView2 window attempt N …`) and
listed in the banner reason when the window finally falls back,
e.g. `… TaskCanceled (earlier attempts: attempt 1: Navigate
failed: … 0x8007139F) [WebView2 runtime 158.0.4370.33]`.

## What v0.5.3 changes (fresh WebView2 profile retry + runtime version on screen)

v0.5.2's fix worked as far as it could — the handler failure went
away — but on Sahand's PC `Navigate` itself then failed with the
same ERROR_INVALID_STATE (`0x8007139F`): the WebView2 browser
process never starts at all, not just late. The prime suspect is
the user-data folder: the crate default left the profile wherever
Edge put it, and a stale or locked profile from a crashed first
run can keep the browser process from starting.

So v0.5.3 uses an EXPLICIT user-data folder,
`%LOCALAPPDATA%\InternetControl\WebView2`, and when the window
fails with `0x8007139F` the app moves that folder aside (to
`WebView2.bak-<timestamp>`) and retries exactly once with a fresh
profile. If the old folder cannot be moved (a crashed Edge process
still holds a lock on it), the retry uses a fresh folder with a
suffix instead (`WebView2-<timestamp>`). Only our own explicit
folder is ever touched — never any shared Edge folder — and the
webview only ever shows our own local page, so its user data is
disposable: nothing of the user's is stored there.

v0.5.3 also logs the available WebView2 runtime version at
startup (`GetAvailableCoreWebView2BrowserVersionString`) and
appends it to the failure reason, so the banner now shows the
Edge version too, e.g. `… [WebView2 runtime 139.0.7258.66]`. If
no runtime is found at all, the banner says so explicitly —
then the fix is installing the Evergreen WebView2 Runtime.

## What v0.5.2 changes (Minimal graph + window keeps trying when the page-loaded check fails)

Minimal mode used to hide the 60-second graph — it showed only the
big download/upload numbers, the session totals and the IP. v0.5.2
keeps the live graph in Minimal mode too: same download (blue
`#50B4FF`) and upload (orange `#FFAA3C`) lines, same 60-second
window, updating every poll exactly like in full mode. Everything
else about Minimal stays minimal — still no tabs, no per-app list,
no adapters tab, no diagnostics — and the v0.5.1 window-failure
banner still shows at the top in Minimal mode as well.

The app window also no longer gives up when registering the
`NavigationCompleted` handler fails. On some PCs that call fails
with ERROR_INVALID_STATE (`0x8007139F`, "The group or resource is
not in the correct state to perform the requested operation") —
the WebView2 browser process is not up yet at that moment — and
v0.5.1 treated it as a total window failure and fell back to the
browser. v0.5.2 navigates anyway (`Navigate` is the call that
(re)starts the browser process), retries the handler once after
navigating, and only falls back to the browser if `Navigate`
itself fails. If the handler never registers, the window stays
open without the page-loaded check — the page renders when it
renders — instead of dropping to the browser.

## What v0.5.1 changes (browser page shows why the app window failed)

On some PCs the app's own WebView2 window never opens and the UI
only appears in the browser (the v0.4.1 fallback). The exact reason
was only in `internet-control.log`, which is easy to miss. v0.5.1
surfaces it: when the window fails, the same reason string —
including the WebErrorStatus / HRESULT where available, e.g.
"WebView2 navigation failed (WebErrorStatus 12, HTTP 0)" — is shown
as a banner at the top of the Overview tab in the browser page:

> The app window could not open: … You are seeing the UI in your
> browser instead.

The banner only appears when the fallback was actually used; when
the page is inside the working app window it is hidden. Nothing
else changes — same v0.5.0 blocking, same log file.

## What v0.5.0 changes (Block terminates open connections)

v0.4's Block only stopped **new** connections: flows that started
before the app launched were unattributed (the WinDivert FLOW layer
only sees flows established after launch), so their packets could
not be dropped and the app showed "BLOCKED — NEW CONNECTIONS ONLY"
while the old connection kept moving. v0.5.0 fixes that two ways:

- **Block terminates the app's open TCP connections at once**
  (`SetTcpEntry` with `DELETE_TCB` — the documented Windows way,
  what TCPView does). The connection dies immediately, and if the
  app reconnects afterwards, the new connection is caught by the
  existing WinDivert drop. Needs admin — the app already requires
  it. (IPv6 TCP has no `SetTcpEntry` in the Windows API, so those
  connections are covered by the second mechanism instead.)
- **Fallback attribution from the connection tables**: a background
  thread rebuilds a 5-tuple → PID map from `GetExtendedTcpTable` +
  `GetExtendedUdpTable` about once a second, and the capture thread
  consults it when the FLOW layer has no entry — so pre-existing
  flows (TCP and UDP, which has no terminate API at all) are
  attributed and their packets dropped like any other blocked
  packet. UDP sockets are keyed by local address + port.
- The Apps tab shows what the Block click did ("Terminated 3 open
  TCP connections for chrome.exe; 2 UDP flows will be dropped") and
  the row flips to plain **BLOCKED** once its traffic stops.
  "BLOCKED — NEW CONNECTIONS ONLY" now only appears for the rare
  flow that genuinely cannot be attributed or terminated.

Nothing about the blocking promise changes: still WinDivert
intercept only, no firewall rules, blocking only while the app is
running — close it and everything is back. Terminations are
one-time actions on live sockets, not persistent rules.

## What v0.4.1 changes (white-page fix)

v0.4.0 opened to a **white page** on Sahand's PC. The page is
embedded in the exe, and static HTML renders visible text even if
the data polling fails — so a fully white page meant the HTML never
rendered: either navigation failed (and v0.4.0 never checked
whether it had succeeded) or navigation stalled before the window
was even shown. v0.4.1:

- the window is **shown first, then navigation starts** — a stuck
  navigation can no longer leave a hidden window;
- the first navigation is checked for real success (`IsSuccess` +
  `WebErrorStatus`) with a **15-second watchdog**. A failed or
  timed-out navigation **closes the window and opens the same page
  in the default browser** instead of leaving a blank page;
- the control bounds are re-applied after showing the window, in
  case the rect at creation time was empty (a zero-size control
  renders white even when navigation succeeds);
- the app now writes **`internet-control.log` next to the exe**
  (or in the temp folder if the exe folder is not writable), with
  timestamps: server URL/port at startup, WebView2 navigation
  success/failure + WebErrorStatus, and capture-status changes
  (intercept / sniff / counter fallback + reason). The exe has no
  console window, so this file is the only record of what happened;
- if no data has arrived from the local server **3 seconds after
  the page loads**, the page itself says so ("Still waiting for
  data from the local server — see internet-control.log next to the
  exe") instead of silently showing zeros.

## If something looks wrong

1. Close the app and look at **`internet-control.log` next to
   `internet-control.exe`** — send that file (or its last lines)
   with the report. The lines to look for:
   - `capture status: ...` — which mode the meter is in
     (intercept / sniff / counter fallback) and why.
   - `Block <exe>: ...` — what the last Block click terminated.
2. If the top numbers read 0 while the Apps list moves, check the
   Diagnostics on the Adapters tab (capture totals by interface
   index, selected adapter, top-numbers source) — that is exactly
   what this version is for.
3. If the window itself does not open, the log file is the only
   record — the exe has no console window.

## What v0.4 changes

- **New UI: WebView2 (Edge, HTML/CSS)** — a single window, dark and
  minimal, same colours (download blue `#50B4FF`, upload orange
  `#FFAA3C`). Three tabs: **Overview** (big DOWNLOAD/UPLOAD,
  session/today/peak, 60-second graph, source line, Run-on-startup
  checkbox, Minimal toggle, Diagnostics), **Apps** (per-app list),
  **Adapters** (adapter table). No internet needed for the UI itself:
  the app serves the page from `http://127.0.0.1:<port>/` and shows
  it in an Edge WebView2 control.
- **WebView2 requirement** — Windows 11 has the WebView2 runtime
  built in. On Windows 10 it comes with Edge (if you have Edge
  installed, you have WebView2). If the runtime is missing, the app
  opens the same page in your default browser instead — the meter
  keeps working either way. The zip ships `WebView2Loader.dll`
  next to the exe so the runtime can be found.
- **Top DOWNLOAD/UPLOAD fixed (the v0.3 bug)** — in v0.3 the top
  numbers followed ONE adapter picked once and never re-evaluated,
  so when the picked adapter went idle (stale PdaNet LUID after a
  re-tether, or the default route pointing at a zero-traffic
  adapter) the top numbers read 0 forever while the per-app list
  kept moving. v0.4:
  - the top numbers use the selected adapter when it carries
    traffic, and **fall back to the sum over all capture
    interfaces** (with a visible amber note) when the selected
    adapter shows nothing while packets are flowing — the top
    numbers can never read 0 while the per-app list shows traffic;
  - if the selected adapter stays idle for ~1 second while other
    interfaces carry traffic, the app **auto-switches to the
    busiest carrying interface** and says so;
  - adapter enumeration now unions `GetIfTable2` with
    `GetAdaptersAddresses`, so a PPP/virtual adapter (PdaNet) that
    is missing from one of them still appears;
  - a **Diagnostics** section (Overview tab) shows the capture
    status and packets seen, the raw per-interface capture totals
    by interface index, how many adapters were enumerated, the
    selected adapter, and which source the top numbers use — so
    the next test proves the cause on your PC instead of guessing.
- **Apps tab: stable, clickable list** — fixed table columns
  (`table-layout: fixed`, right-aligned tabular numerals), so
  columns never shift as numbers change. Rows are keyed by exe
  path and updated in place — the list is **never re-sorted live**,
  so Block is always clickable. Sorting happens only when you click
  a column header or the **View: Rate | Total** toggle. Blocked
  apps are pinned to the top with a BLOCKED badge and their rates
  read 0. Since v0.5, Block also **terminates the app's open TCP
  connections at once** and drops its pre-existing flows (via a
  5-tuple map rebuilt from the connection tables), so a blocked app
  normally flips straight to plain BLOCKED. A blocked app whose
  traffic is still moving shows **"BLOCKED — NEW CONNECTIONS ONLY"**
  only for the rare flow that cannot be attributed or terminated
  (e.g. ICMP) — the UI says so instead of pretending it is fully
  blocked.
- **Rate / Total view toggle** — the Apps toolbar switches the list
  between Rate view (current down/up rates) and Total view
  (cumulative down/up totals). Both columns exist in the data; the
  toggle chooses which the list is primarily sorted/shown by.
- **Minimal toggle** — same app, shows ONLY the big download/upload
  numbers, the session totals, the IP, and the live 60-second
  download/upload graph (since v0.5.2). Since v0.7.0 Minimal shrinks
  the window itself to ~360x190 and shows only the rates, the
  opacity slider and the IP — no session totals, no graph.

Everything else is unchanged from v0.3: per-app attribution via the
WinDivert FLOW layer plus the v0.5 table-based fallback map, ~1 s
smoothed rates, exact per-app totals, run-on-startup (HKCU Run key),
admin manifest, WinDivert 2.2.2 driver in the zip. Blocking is
still WinDivert intercept only — no firewall rules, blocking only
while the app is running; close the app and blocked apps get their
internet back. Block also terminates open TCP connections at once
(`SetTcpEntry`); that is a one-time action, not a persistent rule.

## Why v0.2 — the v0.1 counters were blind

v0.1 read the standard adapter counters (`GetIfTable2`
`InOctets` / `OutOctets`). On Sahand's PC those never move while
downloading: `Get-NetAdapterStatistics` shows only "Ethernet" 0/0,
while the internet comes through "PdaNet Broadband Connection"
(PDAnet Broadband Adapter, Up, IPv4 10.1.19.2, gateway 10.1.19.1).
The PDAnet PPP driver simply does not update the standard counters —
Windows itself is blind to that traffic, so every counter-based
meter shows 0. Counting the actual packets is the only way.

## How this meter is different

- **WinDivert packet capture (v0.2+)** — a WinDivert handle on the
  network layer, filter `true` (all IP traffic), priority 0. It runs
  in **intercept mode**: every packet is reinjected promptly, except
  blocked apps' packets, which are dropped. If intercept mode cannot
  be opened, it falls back to sniff mode (copies only — counting
  works, blocking does not) and the window says so. Each packet is
  counted exactly once, by its IP length, attributed to its interface
  index. Outbound = upload, inbound = download. Loopback packets are
  skipped (but still reinjected in intercept mode, so local apps
  never stall).
- **Per-app attribution** — a second WinDivert handle on the flow
  layer reports flow established/deleted events with ProcessId +
  5-tuple; each packet's 5-tuple (parsed with etherparse) is
  matched to a PID, then to an exe path. Flows that started before
  the app launched appear after they reconnect; unattributed bytes
  (ICMP, pre-existing flows) are still counted in the totals under
  "Unknown (unattributed)" — never lost, never dropped.
- **All adapters are listed, no filtering** — PDAnet, VPN/TAP,
  WiFi, Ethernet, everything — with live down/up per adapter. If
  WinDivert reports an interface index that is not in the adapter
  list, it is shown as an "Unknown interface N" row instead of
  dropping the bytes, so the top numbers never read 0 while packets
  are flowing.
- **Counter fallback (v0.1 mode)** — if WinDivert cannot be opened
  at all (not run as administrator, or driver files missing), the
  meter falls back to the 64-bit adapter counters polled every
  100 ms, and the window says so plainly, with the reason. On
  normal adapters the fallback still works; on PDAnet it will read
  0, because the counters there are frozen.
- **Counter reset handling:** if a counter goes backwards (adapter
  restarted), the meter re-baselines and reports 0 for that sample —
  never a huge spike.
- **Totals are for the selected adapter only** (or the all-interfaces
  fallback, clearly labelled, when the selected adapter is dry),
  never a silent sum of all adapters — unless you choose it: since
  v0.7.0 the Adapters tab offers **Auto** (default, one adapter with
  smart switching), **All**, or ticked adapters summed. By default
  the adapter on the default IPv4 route (0.0.0.0/0, lowest metric)
  is selected; you can pick another one on the Adapters tab. Your
  selection is saved.
- Session totals, today totals (reset at local midnight, saved to
  `%APPDATA%\internet-control\config.json`), peak rates, and a
  60-second live graph.

## Install

1. Download `internet-control-v0.9.0-windows-x64.zip` from Releases.
2. Unzip it anywhere. **Keep all files together in the same folder**
   — the exe will not work properly on its own: `internet-control.exe`,
   `WinDivert.dll`, `WinDivert64.sys` (the WinDivert 2.2.2 driver
   files — without them the meter falls back to counter mode).
3. Run `internet-control.exe`. It asks for administrator rights
   (UAC prompt) — WinDivert needs them. If you decline, the app
   still opens but shows the counter fallback and the reason. **Run
   it as administrator** for the real meter: without admin rights
   WinDivert cannot open, packet counting does not start, and
   blocking does not work.

No installer, no internet connection needed to run it. No Edge or
WebView2 runtime needed (v0.6.0 is native egui — nothing opens in
a browser).

## How to use

- The big numbers at the top are the live download / upload. The
  line under them tells you the source: "WinDivert intercept" (good
  — blocking works), "WinDivert packet capture (sniff)" (counting
  works, blocking does not) or "adapter counters (fallback)" with
  the reason (bad on PDAnet). If the top numbers say "showing all
  interfaces instead", the selected adapter is dry — check the
  Diagnostics section to see which interface is actually carrying
  traffic. On the Adapters tab, **Auto (smart switch)** is the
  default: the app follows the carrying adapter by itself. Tick
  the boxes next to adapters to sum just those instead, or pick
  **All** to sum everything (traffic crossing two adapters, e.g.
  VPN over PdaNet, is counted twice).
- If the auto-picked adapter is wrong (e.g. it picked WiFi while you
  are tethered), tick the right adapter's box on the Adapters tab —
  PDAnet is usually the one whose traffic moves when you download
  something.
- On the Apps tab, click **Block** next to an app to cut its
  internet at once — its open TCP connections are terminated
  immediately and its pre-existing flows are dropped, so the row
  flips to BLOCKED instead of only stopping new connections.
  **Unblock** gives it back. Blocked apps stay pinned at the top
  (marked BLOCKED) even when idle. Blocking ends the moment
  you close Internet Control.
- Click a column header to sort the Apps list, or the **Rate /
  Total** toggle to switch views. The list never re-sorts by itself.
- Click **Minimal** for the really small view: the window shrinks
  to ~360x190 with just the rates, the opacity slider and the IP.
  The Minimal window is resizable too — drag its edges like the
  normal one (down to 300x150). Click **Full view** to go back.
- Drag the **Opacity** slider at the top (or under the rates in
  Minimal view) to make the window see-through — 20% is the most
  transparent it goes, 100% is fully solid. The setting is saved.
- Click **Theme: Dark** / **Theme: Light** at the top to switch between
  the dark and light theme. The choice is saved.
- If the window is short, scroll down — the whole panel scrolls, so
  nothing is cut off. The tables scroll sideways if the window is narrow.

## Verification test — please do this

This is the test that proves the meter is real:

1. Note the session total, or restart the app (session starts at 0).
2. Download a file of a known size, e.g. a **100 MB** file.
3. The session total should read **~100 MB** — a little over, for
   protocol overhead (typically 1–3%). Counts are IP-layer bytes,
   so expect roughly the file size plus TCP/IP headers, not the
   Ethernet framing on top.
4. It should **not** read 0, and it should **not** read ~200 MB.
   ~200 MB means it is double-counting (VPN + PDAnet summed);
   0 means it is watching the wrong adapter, or WinDivert did not
   start — check the source line and the Diagnostics section.

If the number is wrong, tell us what the Diagnostics section shows
(capture totals by interface index, selected adapter, top-numbers
source) — that is exactly what this version is for.

## Build from source

The `windivert` crate needs the WinDivert 2.2.2 library files at
build time. Download
`WinDivert-2.2.2-A.zip` from
https://github.com/basil00/WinDivert/releases/download/v2.2.2/WinDivert-2.2.2-A.zip
and set `WINDIVERT_PATH` to its `x64` folder:

```powershell
$env:WINDIVERT_PATH = "$PWD\WinDivert-2.2.2-A\x64"
cargo build --release --target x86_64-pc-windows-msvc
```

The binary is at `target\x86_64-pc-windows-msvc\release\internet-control.exe`.
Copy `WinDivert.dll` and `WinDivert64.sys` from the same `x64` folder
next to it before running.

`cargo test` runs the platform-independent unit tests (packet
aggregation, rate math, counter-reset handling, formatting, the
top-totals fallback, Unknown-interface rows, stable app-list
ordering, Rate/Total view sorting, the egui UI helpers) on any OS.
`cargo check --target x86_64-pc-windows-msvc` needs
`WINDIVERT_PATH` set as above. `cargo run` on Linux tries to open
the egui window and fails without a display — that is expected;
the engine tests are the Linux gate.

## License

MIT for the meter itself. The WinDivert driver files
(`WinDivert.dll`, `WinDivert64.sys`) are from the official WinDivert
2.2.2 release by Basil00 (https://reqrypt.org/windivert.html) and
are covered by their own license — see the LICENSE file in the
WinDivert zip.
