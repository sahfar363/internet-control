# Internet Control — Usage Meter (v0.4)

A Windows 10/11 (x64) internet usage meter in Rust with a modern
WebView2 (Edge, HTML/CSS) UI that shows **real, precise upload and
download** — including over PDAnet phone tethering + VPN, where many
other apps show 0 or double-count — plus a per-app list with
block/unblock, like NetLimiter.

This is step 2 of the Internet Control project. The meter is proven;
v0.4 rebuilds the UI on WebView2 and fixes the v0.3 top-numbers bug.
Speed limiting per app is the next step.

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
  read 0. A blocked app whose traffic is still moving shows
  **"BLOCKED — NEW CONNECTIONS ONLY"**: its flows started before
  this app launched, so their packets cannot be dropped until the
  app reconnects — the UI says so instead of pretending it is
  fully blocked.
- **Rate / Total view toggle** — the Apps toolbar switches the list
  between Rate view (current down/up rates) and Total view
  (cumulative down/up totals). Both columns exist in the data; the
  toggle chooses which the list is primarily sorted/shown by.
- **Minimal toggle** — same app, shows ONLY the big download/upload
  numbers, the session totals, and the IP.

Everything else is unchanged from v0.3: per-app attribution via the
WinDivert FLOW layer, ~1 s smoothed rates, exact per-app totals,
run-on-startup (HKCU Run key), admin manifest, WinDivert 2.2.2
driver in the zip. Blocking is still WinDivert intercept only — no
firewall rules, blocking only while the app is running; close the
app and blocked apps get their internet back.

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
  never a silent sum of all adapters. By default the adapter on the
  default IPv4 route (0.0.0.0/0, lowest metric) is selected; you can
  pick another one on the Adapters tab. Your selection is saved.
- Session totals, today totals (reset at local midnight, saved to
  `%APPDATA%\internet-control\config.json`), peak rates, and a
  60-second live graph.

## Install

1. Download `internet-control-v0.4.0-windows-x64.zip` from Releases.
2. Unzip it anywhere. Keep all files together in the same folder:
   `internet-control.exe`, `WinDivert.dll`, `WinDivert64.sys` (the
   WinDivert 2.2.2 driver files — without them the meter falls back
   to counter mode) and `WebView2Loader.dll` (lets the app find the
   Edge WebView2 runtime).
3. Run `internet-control.exe`. It asks for administrator rights
   (UAC prompt) — WinDivert needs them. If you decline, the app
   still opens but shows the counter fallback and the reason.

No installer, no internet connection needed to run it. Windows 11
has the WebView2 runtime built in; on Windows 10 it comes with
Edge. Without the runtime the UI opens in your default browser
instead.

## How to use

- The big numbers at the top are the live download / upload. The
  line under them tells you the source: "WinDivert intercept" (good
  — blocking works), "WinDivert packet capture (sniff)" (counting
  works, blocking does not) or "adapter counters (fallback)" with
  the reason (bad on PDAnet). If the top numbers say "showing all
  interfaces instead", the selected adapter is dry — check the
  Diagnostics section to see which interface is actually carrying
  traffic, then Select it on the Adapters tab.
- If the auto-picked adapter is wrong (e.g. it picked WiFi while you
  are tethered), click **Select** next to the right adapter on the
  Adapters tab — PDAnet is usually the one whose traffic moves when
  you download something.
- On the Apps tab, click **Block** next to an app to cut its
  internet, **Unblock** to give it back. Blocked apps stay pinned at
  the top (marked BLOCKED) even when idle. Blocking ends the moment
  you close Internet Control.
- Click a column header to sort the Apps list, or the **Rate /
  Total** toggle to switch views. The list never re-sorts by itself.
- Click **Minimal** for the small view (usage + speed + IP only).

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
next to it before running. `WebView2Loader.dll` comes from the
Microsoft.Web.WebView2 NuGet package (see `.github/workflows/release.yml`)
— or from any Edge/WebView2 install — and goes next to the exe too.

The WebView2 UI itself needs no SDK at build time: the
`webview2-com` crate binds the COM APIs and loads
`WebView2Loader.dll` at runtime only.

`cargo test` runs the platform-independent unit tests (packet
aggregation, rate math, counter-reset handling, formatting, the
top-totals fallback, Unknown-interface rows, stable app-list
ordering, Rate/Total view sorting, the local HTTP server) on any
OS. `cargo check --target x86_64-pc-windows-msvc` needs
`WINDIVERT_PATH` set as above. `cargo run` on Linux serves the UI
on `http://127.0.0.1:<port>/` for a curl check (no live data —
WinDivert is Windows-only).

## License

MIT for the meter itself. The WinDivert driver files
(`WinDivert.dll`, `WinDivert64.sys`) are from the official WinDivert
2.2.2 release by Basil00 (https://reqrypt.org/windivert.html) and
are covered by their own license — see the LICENSE file in the
WinDivert zip. `WebView2Loader.dll` is from Microsoft's WebView2
SDK (https://www.nuget.org/packages/Microsoft.Web.WebView2) and is
covered by its own license.
