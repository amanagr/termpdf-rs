#!/usr/bin/env bash
# Compare termpdf-rs against a browser opening the same PDF. Measures
# steady-state idle CPU% and RSS — i.e. what the resource cost is
# while you're reading a page (not actively scrolling). Output is a
# small markdown table you can paste into the README.
#
# Why this script exists: browsers showing a PDF carry the full
# render pipeline (V8 + Blink + GPU compositor + IPC layers). The
# claim termpdf-rs makes is that you can have native pixel-perfect
# rendering at ~order-of-magnitude lower resource cost. This script
# turns that claim into numbers from your machine.
#
# Why ONLY idle is measured: the script can't reliably inject scroll
# input into either app — they're attached to a TTY (termpdf) or a
# windowing system (browser). Past versions sampled a "scroll window"
# that was really an extra idle window, which produced misleading
# numbers (the second window was always lower because warmup had
# completed). For a real scroll comparison, use the companion
# `monitor-scroll.sh` script, which delta-samples /proc/<pid>/stat
# during a manual scroll burst — same machinery `top` uses.
#
# Usage:
#   scripts/bench-vs-browser.sh path/to/some.pdf [browser]
#
# `browser` is optional; defaults to whichever of chromium / google-
# chrome / firefox is on PATH. Pass an explicit name to override.
#
# The script does NOT close your existing browser windows or interact
# with your current session — it spawns a fresh process with a
# scratch profile dir so the numbers reflect a "just opened a PDF"
# steady state, not a multi-tab daily-driver.
#
# Caveats:
#   - chromium shows the PDF in a tab; the per-process CPU is summed
#     across the multi-process tree (renderer + GPU + main process
#     all count).
#   - firefox is single-process for this purpose, easier to attribute.
#   - termpdf-rs is one process and reports its own CPU directly.
#
# This is not a published benchmark — it's a sanity-check tool for
# the README claim. If it shows termpdf-rs LOSING to a browser on
# any axis, that's a bug we want to see.
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <file.pdf> [browser]" >&2
    echo "browser defaults to first found of: chromium google-chrome firefox" >&2
    exit 64
fi

PDF="$1"
if [[ ! -f "$PDF" ]]; then
    echo "PDF not found: $PDF" >&2
    exit 66
fi

BROWSER="${2:-}"
if [[ -z "$BROWSER" ]]; then
    for cand in chromium google-chrome firefox; do
        if command -v "$cand" >/dev/null 2>&1; then
            BROWSER="$cand"
            break
        fi
    done
fi
if [[ -z "$BROWSER" ]] || ! command -v "$BROWSER" >/dev/null 2>&1; then
    echo "no browser found; install chromium/google-chrome/firefox or pass one explicitly" >&2
    exit 69
fi

TERMPDF="$(dirname "$0")/../target/release/termpdf"
if [[ ! -x "$TERMPDF" ]]; then
    echo "build termpdf first: cargo build --release" >&2
    exit 70
fi

WORK="$(mktemp -d -t termpdf-bench-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

# Sample interval (s) and total sample window (s).
INTERVAL=1
WINDOW=20
# Settle time before sampling. Long enough that pdfium init + first
# paint + warm prefetch + Sharp upgrade re-transmits all complete on
# a 600-page book. Without this, the median CPU% is dragged up by
# warmup activity and looks worse than steady-state actually is.
SETTLE_TERMPDF=10
SETTLE_BROWSER=10

# `ps` outputs in HHHHHpercent (1 decimal); we sum the process tree
# so multi-process browsers (chrome) get fair attribution.
sum_cpu() {
    local pid="$1"
    local total=0
    local pids
    pids="$(pgrep -P "$pid" 2>/dev/null || true)"
    pids="$pid $pids"
    for p in $pids; do
        local pct
        pct="$(ps -p "$p" -o pcpu= 2>/dev/null | tr -d ' ' || true)"
        if [[ -n "$pct" && "$pct" != "0.0" ]]; then
            total="$(awk -v a="$total" -v b="$pct" 'BEGIN{printf "%.1f", a+b}')"
        fi
    done
    echo "$total"
}
sum_rss_kb() {
    local pid="$1"
    local total=0
    local pids
    pids="$(pgrep -P "$pid" 2>/dev/null || true)"
    pids="$pid $pids"
    for p in $pids; do
        local kb
        kb="$(ps -p "$p" -o rss= 2>/dev/null | tr -d ' ' || true)"
        if [[ -n "$kb" ]]; then
            total=$((total + kb))
        fi
    done
    echo "$total"
}

# Sample CPU% and RSS_KB every $INTERVAL seconds for $window seconds,
# return median CPU% and peak RSS_KB.
sample_loop() {
    local pid="$1" window="$2" label="$3"
    local cpu_samples=()
    local rss_peak=0
    local end=$(( SECONDS + window ))
    while (( SECONDS < end )); do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "  $label: process gone, aborting sample" >&2
            break
        fi
        local cpu rss
        cpu="$(sum_cpu "$pid")"
        rss="$(sum_rss_kb "$pid")"
        cpu_samples+=("$cpu")
        if (( rss > rss_peak )); then
            rss_peak="$rss"
        fi
        sleep "$INTERVAL"
    done
    # Median CPU%.
    local sorted
    sorted="$(printf '%s\n' "${cpu_samples[@]}" | sort -n)"
    local n="${#cpu_samples[@]}"
    local mid=$(( n / 2 ))
    local median
    median="$(echo "$sorted" | sed -n "$((mid + 1))p")"
    echo "$median $rss_peak"
}

# ===========================================================================
# Run termpdf-rs
# ===========================================================================
echo "=== termpdf-rs ==="
TERMPDF_LOG="$WORK/termpdf.log"
"$TERMPDF" "$PDF" --protocol kitty </dev/null >"$TERMPDF_LOG" 2>&1 &
TERMPDF_PID=$!
echo "  warmup settle ($SETTLE_TERMPDF s)..."
sleep "$SETTLE_TERMPDF"
echo "  steady-state idle window ($WINDOW s)..."
TERMPDF_IDLE="$(sample_loop "$TERMPDF_PID" "$WINDOW" termpdf)"
TERMPDF_CPU="${TERMPDF_IDLE% *}"
TERMPDF_RSS="${TERMPDF_IDLE#* }"

kill "$TERMPDF_PID" 2>/dev/null || true
wait "$TERMPDF_PID" 2>/dev/null || true

# ===========================================================================
# Run browser
# ===========================================================================
echo "=== $BROWSER ==="
PROFILE="$WORK/browser-profile"
mkdir -p "$PROFILE"

# Browser-specific spawn. Use --no-first-run to skip welcome dialogs;
# a scratch profile keeps this run isolated from your daily browsing.
case "$BROWSER" in
    chromium|google-chrome)
        "$BROWSER" \
            --user-data-dir="$PROFILE" \
            --no-first-run \
            --no-default-browser-check \
            "file://$PDF" >/dev/null 2>&1 &
        ;;
    firefox)
        "$BROWSER" \
            --profile "$PROFILE" \
            --no-remote \
            "file://$PDF" >/dev/null 2>&1 &
        ;;
    *)
        echo "unknown browser '$BROWSER'; this script only knows chromium/google-chrome/firefox" >&2
        exit 78
        ;;
esac
BROWSER_PID=$!
echo "  warmup settle ($SETTLE_BROWSER s)..."
sleep "$SETTLE_BROWSER"

echo "  steady-state idle window ($WINDOW s)..."
BROWSER_IDLE="$(sample_loop "$BROWSER_PID" "$WINDOW" "$BROWSER")"
BROWSER_CPU="${BROWSER_IDLE% *}"
BROWSER_RSS="${BROWSER_IDLE#* }"

# Tear down browser process tree.
pkill -P "$BROWSER_PID" 2>/dev/null || true
kill "$BROWSER_PID" 2>/dev/null || true
wait "$BROWSER_PID" 2>/dev/null || true

# ===========================================================================
# Output
# ===========================================================================
mb() { awk -v kb="$1" 'BEGIN{printf "%.0f MB", kb/1024}'; }
ratio() { awk -v a="$1" -v b="$2" 'BEGIN{ if (b==0) printf "n/a"; else printf "%.1f×", b/a }'; }

echo
echo "## Steady-state resource cost: termpdf-rs vs $BROWSER"
echo
echo "PDF: $(basename "$PDF")"
echo "Sample window: $WINDOW s after a $SETTLE_TERMPDF s warmup settle."
echo
echo "| Metric        | termpdf-rs   | $BROWSER     | ratio |"
echo "| ------------- | ------------ | ------------ | ----- |"
echo "| Idle CPU%     | ${TERMPDF_CPU}%       | ${BROWSER_CPU}%      | $(ratio "$TERMPDF_CPU" "$BROWSER_CPU") |"
echo "| Idle RSS      | $(mb "$TERMPDF_RSS")     | $(mb "$BROWSER_RSS")    | $(ratio "$TERMPDF_RSS" "$BROWSER_RSS") |"
echo
echo "Notes:"
echo "  - termpdf-rs is a single process; its numbers are direct."
echo "  - $BROWSER's numbers sum the process tree (renderer + GPU"
echo "    + main); a single-process browser like firefox is more"
echo "    apples-to-apples but chromium/chrome reflects the full cost."
echo "  - This benchmark only measures IDLE. For scroll comparison,"
echo "    use scripts/monitor-scroll.sh: in one pane open the PDF"
echo "    (in termpdf-rs or the browser) and hold j / Page Down;"
echo "    in another pane run 'monitor-scroll.sh termpdf ghostty'"
echo "    or 'monitor-scroll.sh firefox'."
