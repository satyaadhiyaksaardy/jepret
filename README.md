# Jepret

A lightweight screenshot and annotation tool for macOS that lives in the menu bar.
*Jepret* is the Javanese and Indonesian word for the click of a camera shutter, or "to snap a photo".

## Capture

| Shortcut | Action |
|---|---|
| ⌘⇧2 | Capture an area; releasing the selection opens the editor |
| ⌥⇧⌘2 | Repeat the last area |
| ⌘⇧1 | Recognize text or a QR code and copy it |

The menu bar icon also offers window capture, full screen, a 3-second delayed capture, opening an image file and launch at login.

In the capture overlay: Enter edits the full screen, ⌘C copies, ⌘S saves to the Desktop, the arrow keys measure gaps with a pixel ruler (scroll to adjust its tolerance), and Tab copies the color under the cursor.

## Editor

Tools: crop (V), arrow (A), line (L), rectangle (R), oval (O), freehand (D), highlighter (H), pixelate (B), spotlight (S), text (T), numbered counter (C) and magnifier (M).

- ⌘V pastes an image to place, move, resize and fade.
- K adds a gradient backdrop with padding, rounded corners and a shadow.
- ⌘C copies and closes, ⌘S saves to the Desktop, and ⌘P pins the image on screen as a floating window.
- ⌘Z undoes and ⇧⌘Z redoes.
- Pinch or ⌘-scroll zooms, ⌘0 shows actual size, and ⌘1 fits the image to the window.

## Build

Requires macOS 12 or later and a Rust toolchain.

```bash
cargo run --release
```

`scripts/bundle.sh` builds a universal `Jepret.app` and a drag-to-install `Jepret.dmg` in `dist/`.
It signs with the first Developer ID or Apple Development identity in your keychain (or `SIGN_ID`), so macOS keeps the Screen Recording permission across rebuilds.

On first launch, grant Jepret Screen Recording access in System Settings ▸ Privacy & Security, then relaunch it.
