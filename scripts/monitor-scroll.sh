#!/usr/bin/env bash
# Monitor combined CPU% for a set of processes (by name) over a
# fixed window. Use during an interactive scroll session for an
# honest "cost of reading this PDF" measurement.
#
# The companion script `bench-vs-browser.sh` measures steady-state
# idle automatically, but it can't inject scroll input into either
# the terminal or the browser. This script flips the model: you
# scroll manually, the script samples in the background.
#
# Why combined CPU%: when you scroll in termpdf-rs running inside
# Ghostty, the work is split — termpdf writes kitty escapes to the
# pty, Ghostty PNG-decodes the transmits and uploads textures to the
# GPU. Counting only termpdf's CPU undersells the cost; counting
# only Ghostty's misses the producer side. Sum them.
#
# Usage:
#   # Terminal A (your reading session):
#   pdf path/to/file.pdf
#
#   # Terminal B (or another tmux pane):
#   scripts/monitor-scroll.sh termpdf ghostty
#
#   # Now in Terminal A, scroll for ~25 seconds (hold j, mouse
#   # wheel, whatever). The script prints the median CPU% for
#   # the combined process tree.
#
# For a Firefox comparison:
#   scripts/monitor-scroll.sh firefox
#   # then open the PDF in Firefox and hold Page Down for 25 s.
#
# Args: one or more process *names* (matched via pgrep -x). All
# matching processes are sampled and their pcpu summed.
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <name1> [name2 ...]" >&2
    echo "  example: $0 termpdf ghostty" >&2
    echo "  example: $0 firefox" >&2
    exit 64
fi

WINDOW=25       # seconds — long enough to dominate startup blips
INTERVAL=1      # 1 Hz sampling, matches the in-app HUD cadence
COUNTDOWN=3     # warning seconds before sampling starts

# Resolve names → pids (process roots; we'll add their children at
# sample time so newly-spawned helpers / GPU children get counted).
ROOT_PIDS=()
for name in "$@"; do
    while IFS= read -r p; do
        ROOT_PIDS+=("$p")
    done < <(pgrep -x "$name" 2>/dev/null || true)
done

if [[ ${#ROOT_PIDS[@]} -eq 0 ]]; then
    echo "no processes match: $*" >&2
    echo "is the app running? pgrep -xa $*" >&2
    exit 69
fi

echo "monitoring: $* (roots: ${ROOT_PIDS[*]})"
for ((i=COUNTDOWN; i>0; i--)); do
    echo "  sampling starts in ${i}s — start scrolling now..."
    sleep 1
done
echo "  sampling for ${WINDOW}s..."

# Sample CPU% (sum across all matching roots + children) and peak RSS.
sum_cpu_for() {
    local total=0
    for root in "${ROOT_PIDS[@]}"; do
        if ! kill -0 "$root" 2>/dev/null; then
            continue
        fi
        local pids
        pids="$(pgrep -P "$root" 2>/dev/null || true)"
        pids="$root $pids"
        for p in $pids; do
            local pct
            pct="$(ps -p "$p" -o pcpu= 2>/dev/null | tr -d ' ' || true)"
            if [[ -n "$pct" && "$pct" != "0.0" ]]; then
                total="$(awk -v a="$total" -v b="$pct" 'BEGIN{printf "%.1f", a+b}')"
            fi
        done
    done
    echo "$total"
}
sum_rss_for() {
    local total=0
    for root in "${ROOT_PIDS[@]}"; do
        if ! kill -0 "$root" 2>/dev/null; then
            continue
        fi
        local pids
        pids="$(pgrep -P "$root" 2>/dev/null || true)"
        pids="$root $pids"
        for p in $pids; do
            local kb
            kb="$(ps -p "$p" -o rss= 2>/dev/null | tr -d ' ' || true)"
            if [[ -n "$kb" ]]; then
                total=$((total + kb))
            fi
        done
    done
    echo "$total"
}

cpu_samples=()
rss_peak=0
end=$(( SECONDS + WINDOW ))
while (( SECONDS < end )); do
    cpu="$(sum_cpu_for)"
    rss="$(sum_rss_for)"
    cpu_samples+=("$cpu")
    if (( rss > rss_peak )); then
        rss_peak="$rss"
    fi
    sleep "$INTERVAL"
done

# Stats: median + p95 + max so a held-key burst's tail is visible.
# pcpu is "of one CPU" so >100% means multiple cores.
sorted="$(printf '%s\n' "${cpu_samples[@]}" | sort -n)"
n="${#cpu_samples[@]}"
median="$(echo "$sorted" | sed -n "$((n/2 + 1))p")"
p95_idx=$(( (n * 95 + 99) / 100 ))
[[ $p95_idx -lt 1 ]] && p95_idx=1
p95="$(echo "$sorted" | sed -n "${p95_idx}p")"
max="$(echo "$sorted" | tail -n1)"

echo
echo "## scroll CPU sample"
echo
echo "Processes: $*"
echo "Window: ${WINDOW} s @ ${INTERVAL} Hz, ${n} samples."
echo
echo "| metric          | value     |"
echo "| --------------- | --------- |"
echo "| CPU% median     | ${median}%      |"
echo "| CPU% p95        | ${p95}%      |"
echo "| CPU% max        | ${max}%      |"
echo "| RSS peak        | $(awk -v kb="$rss_peak" 'BEGIN{printf "%.0f MB", kb/1024}')  |"
echo
echo "(pcpu is 'of one CPU' — values >100 % mean multiple cores)"
