# Internet Control — Usage Meter (v0.3)

A Windows 10/11 (x64) internet usage meter in Rust + egui that shows
**real, precise upload and download** — including over PDAnet phone
tethering + VPN, where many other apps show 0 or double-count — plus
a per-app list with block/unblock, like NetLimiter.

This is step 2 of the Internet Control project. The meter is proven;
v0.3 adds per-app traffic and blocking on the same measurement
engine. Speed limiting per app is the next step.

## Why v0.2 — the v0.1 counters were blind

v0.1 read the standard adapter counters (`GetIfTable2`
`InOctets` / `OutOctets`). On Sahand's PC those never move while
downloading: `Get-NetAdapterStatistics` shows only "Ethernet" 0/0,
while the internet comes through "PdaNet Broadband Connection"
(PDAnet Broadband Adapter, Up, IPv4 10.1.19.2, gateway 10.1.19.1).
The PDAnet PPP driver simply does not update the standard counters —
Windows itself is blind to that traffic, so every counter-based
meter shows 0. Counting the actual packets is the only way.

## What v0.3 adds

- **Per-app list (NetLimiter-style)** — every app using the
  internet, with its own download/upload rate and cumulative totals.
  Click a column header to sort: rate columns sort by Rate
  (current down+up), total columns sort by Total (cumulative
  down+up); App sorts by name. Default sort is Rate, busiest first.
- **Block / Unblock per app** — blocking uses WinDivert intercept
  mode, not the firewall: the counting handle holds every packet
  and reinjects it, except packets belonging to a blocked app,
  which are dropped. **Blocking only works while this app is
  running.** Close the app (or restart the PC without it) and
  every blocked app gets its internet back automatically — no
  firewall rules are created, ever. The blocked *list* is saved in
  config, so the same apps are blocked again next time you open
  the app — but nothing is applied while the app is closed.
- **Smoother numbers** — totals stay exact, but the displayed
  rates (top numbers, graph, per-app rates) are smoothed with an
  exponential moving average over about 1 second, so they no
  longer flicker every 100 ms. Same colours: download blue,
  upload orange.
- **Minimal view** — a "Minimal" button at the top switches to a
  view showing only the big download/upload numbers, the session
  totals, and the IP of the selected adapter. "Full view" switches
  back.
- **Run on startup** — a checkbox in the full view. It creates a
  Run key under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
  pointing at the exe. Note: the app needs administrator rights
  for WinDivert, so Windows will still show a UAC prompt at login
  — that is expected; nothing here bypasses UAC.
- **Normal connections too** — WinDivert sees every interface, so
  WiFi, Ethernet, PDAnet and VPN traffic are all counted and all
  appear in the per-app list. The adapter selector still controls
  which adapter the top totals follow.

## How this meter is different

- **WinDivert packet capture (v0.2+)** — a WinDivert handle on the
  network layer, filter `true` (all IP traffic), priority 0. In
  v0.3 it runs in **intercept mode**: every packet is reinjected
  promptly, except blocked apps' packets, which are dropped. If
  intercept mode cannot be opened, it falls back to sniff mode
  (copies only — counting works, blocking does not) and the window
  says so. Each packet is counted exactly once, by its IP length,
  attributed to its interface index. Outbound = upload,
  inbound = download. Loopback packets are skipped (but still
  reinjected in intercept mode, so local apps never stall).
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
- **Totals are for the selected adapter only**, never the sum of all
  adapters. By default the adapter on the default IPv4 route
  (0.0.0.0/0, lowest metric) is selected; you can pick another one
  in the list. Your selection is saved.
- Session totals, today totals (reset at local midnight, saved to
  `%APPDATA%\internet-control\config.json`), peak rates, and a
  60-second live graph.

## Install

1. Download `internet-control-v0.3.0-windows-x64.zip` from Releases.
2. Unzip it anywhere. Keep all three files together in the same
   folder: `internet-control.exe`, `WinDivert.dll`,
   `WinDivert64.sys` (the WinDivert 2.2.2 driver files — without
   them the meter falls back to counter mode).
3. Run `internet-control.exe`. It asks for administrator rights
   (UAC prompt) — WinDivert needs them. If you decline, the app
   still opens but shows the counter fallback and the reason.

No installer, no internet connection needed to run it.

## How to use

- The big numbers at the top are the live download / upload of the
  **selected adapter**. The line under them tells you the source:
  "WinDivert intercept" (good — blocking works), "WinDivert
  packet capture (sniff)" (counting works, blocking does not) or
  "adapter counters (fallback)" with the reason (bad on PDAnet).
- If the auto-picked adapter is wrong (e.g. it picked WiFi while you
  are tethered), click **Select** next to the right adapter in the
  list — PDAnet is usually the one whose traffic moves when you
  download something.
- In the Apps list, click **Block** next to an app to cut its
  internet, **Unblock** to give it back. Blocked apps stay in the
  list (marked BLOCKED) even when idle. Blocking ends the moment
  you close Internet Control.
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
   start — check the source line under the heading.

If the number is wrong, tell us which adapter you selected and what
the per-adapter numbers showed — that is exactly what this version
is for.

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
aggregation, rate math, counter-reset handling, formatting) on any
OS. `cargo check --target x86_64-pc-windows-msvc` needs
`WINDIVERT_PATH` set as above.

## License

MIT for the meter itself. The WinDivert driver files
(`WinDivert.dll`, `WinDivert64.sys`) are from the official WinDivert
2.2.2 release by Basil00 (https://reqrypt.org/windivert.html) and
are covered by their own license — see the LICENSE file in the
WinDivert zip.
