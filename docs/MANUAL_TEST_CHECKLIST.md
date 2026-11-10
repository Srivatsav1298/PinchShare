# Manual test checklist: gestures and fallbacks

Run on every OS before a pilot build. Two paired devices on the same Wi-Fi (pair them first; see README).
Mark each line PASS / FAIL / N/A and note the OS version. A gesture failing is acceptable if its hotkey fallback passes; record it in KNOWN_LIMITATIONS.md.

## Setup (each device)
- [ ] App starts with no Dock icon; tray icon visible.
- [ ] Tray menu shows: Copy selection, Paste here, Cancel pending paste, Pinch gestures, Show pairing code, Quit.
- [ ] macOS: with no Accessibility permission, a toast explains it; "Open Accessibility settings" opens the right pane; after granting and restarting, the toast no longer appears.

## macOS (trackpad)
- [ ] Select text in TextEdit, pinch IN with two fingers: toast "Copied. Pinch out on another device to paste."
- [ ] On device B, focus a text field, pinch OUT: toast "Receiving text from A…" with a 3 s bar and Cancel; text appears at the cursor after the bar ends.
- [ ] Pinch OUT then click Cancel within 3 s: nothing is pasted; toast "Cancelled."
- [ ] One long pinch fires exactly one action (no repeat toasts).
- [ ] Pinch in Safari / Maps / Preview (apps that zoom on pinch): note whether the app zooms as well. Then set `gesture_modifier` to `alt` in settings.json and confirm the pinch only triggers PinchShare while Option is held.
- [ ] Tray "Pinch gestures" unchecked: pinching does nothing; hotkeys still work. Setting persists after restart.
- [ ] Copy an image in Preview, pinch in; paste on B into Notes/Preview: image appears.
- [ ] Select 2 files and a folder in Finder, pinch in; pinch out on B: files land in ~/Downloads/PinchShare and Finder reveals them.
- [ ] A 500 MB file shows progress in the toast and finishes.
- [ ] Wait 60 s after copying, then pinch out on B: "Nothing to paste".

## Windows 10/11 (precision touchpad)
Gesture detection on Windows is NOT implemented yet (spike pending); the hotkeys are the primary trigger.
- [ ] Ctrl+Alt+C copies the selection (toast), Ctrl+Alt+V on the other device pastes after the 3 s bar.
- [ ] Hotkey held down: only one action fires.
- [ ] If a hotkey is already taken by another app, a toast says so and the app keeps running.
- [ ] Spike (to enable gestures): with a precision touchpad, log WM_MOUSEWHEEL events with MK_CONTROL while pinching vs. while holding real Ctrl and scrolling; record whether the Ctrl key state differs. Report results in docs/spikes.

## Hotkeys (both OSes)
- [ ] Defaults: Cmd/Ctrl+Alt+C copy, Cmd/Ctrl+Alt+V paste. Edit settings.json and restart: new keys work.
- [ ] Hotkey works while the app has no focus and while another app is fullscreen.
- [ ] An invalid hotkey string shows a toast and does not crash.

## Safety
- [ ] 3-second cancel window always appears before any transfer starts (watch the network, or confirm the file is absent until it ends).
- [ ] Paired device removed on A ("unpair all"): B's paste says "Nothing to paste" and A shows no activity.
- [ ] Two devices with content ready: B pastes the most recently copied one.
- [ ] Metrics: `metrics.jsonl` in the app data folder has one line per paste attempt; cancelled gestures have `"misfire": true`; no file names or text appear in it.
