# PinchShare

Move text, images, files and folders between your own devices with a trackpad pinch: pinch in to copy on one device, pinch out on another to paste. Direct over your local network, no cloud, no accounts, no telemetry. MIT licensed.

> **Status: early MVP, pre-pilot.** Phases 0–3 of 6 are built (transport, files, gesture layer). The phone PWA, onboarding/pairing UI and installers are not done. See [Status](#status) for what is and isn't verified.

## How it works

1. **Pair** two devices once with a 6-digit code. The code is run through SPAKE2, so it can't be brute-forced offline, and each code allows 3 guesses.
2. **Copy** on device A: pinch in (macOS) or press the hotkey. The selection goes into a "ready" slot for 60 seconds.
3. **Paste** on device B: pinch out or press the hotkey. B finds the freshest ready slot among paired devices and shows a 3-second "Cancel" toast. After that, A sends the content over TLS.
4. Text and images are pasted at the cursor. Files and folders are saved to `~/Downloads/PinchShare/` and revealed.

Security: TLS 1.3 with self-signed certificates pinned per device, every request HMAC-signed (replay and tamper protected), unpaired devices rejected, clipboard content never logged.

## Layout

```
apps/desktop/core        Rust library: pairing, crypto, discovery (mDNS), transfer, controller
apps/desktop/src-tauri   Tauri 2 tray app: gestures, hotkeys, toast, clipboard glue
apps/desktop/ui          Toast page
scripts/spikes           Phase 0 feasibility spikes (macOS pinch, clipboard, mDNS, PWA pinch)
docs/                    Manual test checklist
```

## Build and test

Requires Rust (stable). macOS needs Xcode Command Line Tools.

```bash
cargo test --workspace                                    # unit + integration tests
cargo test --release --test phase2 -- --ignored --nocapture   # 500 MB file and folder tests
cargo run -p pinchshare-desktop                           # run the tray app
```

Default hotkeys: `Cmd/Ctrl+Alt+C` copy, `Cmd/Ctrl+Alt+V` paste. Settings live in `settings.json` in the app data folder (`~/Library/Application Support/PinchShare` on macOS).

On macOS, grant **Accessibility** permission to the app (or the terminal you launch it from) so it can send Cmd+C / Cmd+V.

## Status

| Phase | State |
|---|---|
| 0 Feasibility spikes | macOS clipboard files and desktop mDNS verified. macOS pinch, Windows pinch, phone pinch: **not yet tested** |
| 1 Pairing, TLS, text transfer, discovery | Done, tested on loopback |
| 2 Files and folders, progress, retry, checksum | Done. 500 MB file ~2 s and 500 MB nested folder ~6 s on loopback |
| 3 Gestures, hotkeys, cancel toast | Core logic tested. **The tray app has only been smoke-launched**; gestures, hotkeys and simulated paste are unverified on real hardware |
| 4 Phone PWA | Not started |
| 5 Onboarding, settings UI, metrics export | Not started (metrics are already written locally as `metrics.jsonl`) |
| 6 Installers | Not started |

Known gaps right now:

- **No pairing UI yet.** Pairing works through the core library and the `psdemo` CLI only.
- **Windows has no pinch detection.** Hotkeys only until the Windows spike is run.
- **No auto-accept setting.** Any paired device can send to you.
- Paired-device keys are stored in a plaintext JSON file (0600), not the OS keychain.
- A paste overwrites the receiving device's clipboard.
- Not yet tested across two real machines or on real Wi-Fi, so the 99% success target is unmeasured.

Manual test steps: [docs/MANUAL_TEST_CHECKLIST.md](docs/MANUAL_TEST_CHECKLIST.md).

## License

MIT, see [LICENSE](LICENSE).
