#!/bin/sh
# FluxDeck RSS benchmark. Safe to run anytime; missing processes are skipped.
set -u

row() { # $1 label  $2 pid (empty if absent)
    if [ -n "$2" ] && kill -0 "$2" 2>/dev/null; then
        rss=$(ps -o rss= -p "$2")
        echo "| $1 | $((rss / 1024)) MB |"
    else
        echo "| $1 | not running |"
    fi
}

RUST_PID=$(pidof fluxdeck 2>/dev/null || true)
PY_PID=$(pgrep -f brightness_tray.py 2>/dev/null | head -n1 || true)

echo "## FluxDeck RSS benchmark ($(date -u '+%Y-%m-%d %H:%M UTC'))"
echo
echo "| Process | RSS |"
echo "|---|---|"
row "Rust fluxdeck" "$RUST_PID"
row "Python brightness_tray.py" "$PY_PID"
