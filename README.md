# Internet Control — Usage Meter (v0.2)

A Windows 10/11 (x64) internet usage meter in Rust + egui that shows
**real, precise upload and download** — including over PDAnet phone
tethering + VPN, where many other apps show 0 or double-count.

This is step 1 of the Internet Control project. Once the meter is proven
accurate, the per-app limiter will be built on top of it (same
measurement engine + WinDivert shaping).

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

- **WinDivert packet capture (v0.2)** — a WinDivert handle on the
  network layer, filter `true` (all IP traffic), priority 0, flags
  SNIFF + RECV_ONLY. In sniff mode WinDivert delivers a *copy* of each
  packet and the original keeps flowing untouched: nothing is blocked,
  dropped, or re-injected. Each packet is counted exactly once, by its
  IP length, attributed to its interface index. Outbound = upload,
  inbound = download. Loopback packets are skipped.
- **All adapters are listed, no filtering** — PDAnet, VPN/TAP,
  WiFi, Ethernet, everything — with live down/up per adapter. If
  WinDivert reports an interface index that is not in the adapter
  list, it is shown as an "Unknown interface N" row instead of
  dropping the bytes, so the top numbers never read 0 while packets
  are flowing.
- **Counter fallback (v0.1 mode)** — if WinDivert cannot be opened
  (not run as administrator, or driver files missing), the meter falls
  back to the 64-bit adapter counters polled every 100 ms, and the
  window says so plainly, with the reason. On normal adapters the
  fallback still works; on PDAnet it will read 0, because the
  counters there are frozen.
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

1. Download `internet-control-v0.2.0-windows-x64.zip` from Releases.
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
  "WinDivert packet capture" (good) or "adapter counters
  (fallback)" with the reason (bad on PDAnet).
- If the auto-picked adapter is wrong (e.g. it picked WiFi while you
  are tethered), click **Select** next to the right adapter in the
  list — PDAnet is usually the one whose traffic moves when you
  download something.
- Compare adapters side by side: if two adapters move together with
  almost the same numbers, they are carrying the same traffic
  (VPN + PDAnet). Pick the outer one (PDAnet) for your real
  carrier usage.

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
