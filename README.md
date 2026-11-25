# Internet Control — Usage Meter (v0.1)

A Windows 10/11 (x64) internet usage meter in Rust + egui that shows
**real, precise upload and download** — including over PDAnet phone
tethering + VPN, where many other apps show 0 or double-count.

This is step 1 of the Internet Control project. Once the meter is proven
accurate, the per-app limiter will be built on top of it (same
measurement engine + WinDivert shaping).

## Why other apps get it wrong

- Most meters only watch normal WiFi / Ethernet adapters and ignore
  virtual adapters. On a PDAnet setup the internet comes through a
  virtual adapter, so they show 0.
- Some rely on Windows' own data-usage driver; if that driver is
  stopped, they show 0 too.
- Some sum *all* adapters. If your PC has both a VPN adapter and a
  PDAnet adapter, the same bytes cross both — summing them counts
  every byte twice.

## How this meter is different

- **All adapters are listed, no filtering** — PDAnet, VPN/TAP,
  WiFi, Ethernet, everything — with live down/up per adapter.
- **64-bit counters** (`ifHCInOctets` / `ifHCOutOctets` via IP Helper
  `GetIfTable2`), polled **every 100 ms** from a background thread.
  Rate = counter delta / elapsed time (monotonic clock).
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

1. Download `internet-control-v0.1.0-windows-x64.zip` from Releases.
2. Unzip it anywhere.
3. Run `internet-control.exe`.

No admin rights needed for the meter. No installer, no internet
connection needed to run it.

## How to use

- The big numbers at the top are the live download / upload of the
  **selected adapter**.
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
   protocol overhead (typically 1–3%).
4. It should **not** read 0, and it should **not** read ~200 MB.
   ~200 MB means it is double-counting (VPN + PDAnet summed);
   0 means it is watching the wrong adapter.

If the number is wrong, tell us which adapter you selected and what
the per-adapter numbers showed — that is exactly what v0.1 is for.

## Build from source

```powershell
cargo build --release --target x86_64-pc-windows-msvc
```

The binary is at `target\x86_64-pc-windows-msvc\release\internet-control.exe`.

`cargo test` runs the platform-independent unit tests (rate math,
counter-reset handling, formatting) on any OS.

## License

MIT. WinDivert is not used in v0.1 (meter only); it will be used
for the limiter in a later version.
