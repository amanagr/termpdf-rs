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
# Args: one or more process *names* (matched via pgrep -x). Each
# matching root pid + its direct children get utime/stime delta-
# sampled per second; the deltas are summed across the tree, so a
# multi-process browser (chrome/firefox) is fairly attributed.
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <name1> [name2 ...]" >&2
    echo "  example: $0 termpdf ghostty" >&2
    echo "  example: $0 firefox" >&2
    exit 64
fi

WINDOW=25       # seconds — long enough to dominate startup blips
INTERVAL=1      # 1 Hz delta-sampling; each window is a "current CPU%"
COUNTDOWN=3     # warning seconds before sampling starts
CLK_TCK="$(getconf CLK_TCK)"  # jiffies/sec, for /proc/<pid>/stat math

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

# Enumerate root pids + their direct children. Direct children only —
# matches what `ps -o pcpu` would have summed, and is enough for the
# browsers we care about (Chrome / Firefox flatten content + GPU
# processes as direct children of the main process).
all_pids() {
    for root in "${ROOT_PIDS[@]}"; do
        if ! kill -0 "$root" 2>/dev/null; then
            continue
        fi
        echo "$root"
        pgrep -P "$root" 2>/dev/null || true
    done
}

# Snapshot {pid, utime+stime} for the process tree. Lines look like
# "<pid> <ticks>". Reading /proc/<pid>/stat is fast (kernel-resident);
# we tolerate transient pids vanishing mid-snapshot.
#
# Field layout (man 5 proc): pid (comm) state ppid ... utime stime ...
# comm can contain spaces and parens, so we anchor on ") " to skip past
# it before splitting the rest by whitespace.
snapshot_ticks() {
    while IFS= read -r pid; do
        [[ -z "$pid" ]] && continue
        awk -v pid="$pid" '
        {
            i = match($0, /\) /)
            if (i == 0) next
            rest = substr($0, i + 2)
            split(rest, a, " ")
            # a[12] = utime (orig field 14), a[13] = stime (orig field 15).
            printf "%s %d\n", pid, a[12] + a[13]
        }' "/proc/$pid/stat" 2>/dev/null || true
    done < <(all_pids)
}

# Sample CPU% over a fixed interval using /proc/<pid>/stat deltas.
#
# Why not `ps -o pcpu`: that field is the *lifetime* average since the
# process started — for a Firefox that's been up for hours, a 25 s
# scroll burst at 50 % CPU averages out to ~1 % over its lifetime, so
# ps reports 1 % and you think the browser is idle. We want what `top`
# shows: utime+stime delta over a short window, divided by wall time.
#
# Output: one float, "of one CPU" units (>100 means multi-core).
cpu_window() {
    local interval="$1"
    local snap0 snap1
    snap0="$(snapshot_ticks)"
    sleep "$interval"
    snap1="$(snapshot_ticks)"
    awk -v interval="$interval" -v clk="$CLK_TCK" '
    NR==FNR { t0[$1] = $2; next }
    {
        if ($1 in t0) {
            d = $2 - t0[$1]
            if (d > 0) sum += d
        }
    }
    END {
        printf "%.1f\n", (sum * 100.0) / (interval * clk)
    }' <(echo "$snap0") <(echo "$snap1")
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
    # cpu_window blocks for INTERVAL seconds (it's the delta window),
    # so we don't add an outer sleep — that would double the wall time.
    cpu="$(cpu_window "$INTERVAL")"
    rss="$(sum_rss_for)"
    cpu_samples+=("$cpu")
    if (( rss > rss_peak )); then
        rss_peak="$rss"
    fi
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
