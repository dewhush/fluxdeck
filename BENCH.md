# FluxDeck RSS benchmark

Live measurement (2026-10, X11/XFCE):

| Process | RSS |
|---|---|
| Rust fluxdeck (x11rb) | ~2.6 MB |
| Python brightness_tray.py (PyQt6, previous) | ~79 MB |

~30x smaller resident footprint. Regenerate: `scripts/bench.sh`.
