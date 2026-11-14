#!/usr/bin/env bash
# usage: bench/mem.sh PID                sample a running process (Ctrl-C stops)
#        bench/mem.sh -- CMD [ARGS...]    start CMD, sample it, stop when it exits
# env:   INTERVAL=0.5  STOP_AFTER=<seconds, 0 = until exit>
set -uo pipefail

INTERVAL=${INTERVAL:-0.5}

rss_kb() { awk '/^VmRSS:/{print $2}' "/proc/$1/status" 2>/dev/null; }
hwm_kb() { awk '/^VmHWM:/{print $2}' "/proc/$1/status" 2>/dev/null; }
mb() { awk -v k="${1:-0}" 'BEGIN{printf "%.1f MB", k/1024}'; }

if [ $# -eq 0 ]; then
  sed -n '2,4p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi

child=
if [ "$1" = "--" ]; then
  shift
  [ $# -gt 0 ] || { echo "mem.sh: no command after --" >&2; exit 2; }
  "$@" &
  pid=$!
  child=$pid
  trap 'kill "$child" 2>/dev/null' INT TERM
else
  pid=$1
  case $pid in ''|*[!0-9]*) echo "mem.sh: expected a PID or -- CMD, got '$pid'" >&2; exit 2;; esac
fi

if [ ! -r "/proc/$pid/status" ]; then
  echo "mem.sh: no such process: $pid" >&2
  exit 1
fi

max_samples=$(awk -v s="${STOP_AFTER:-0}" -v i="$INTERVAL" 'BEGIN{print (s>0 ? int(s/i) : 0)}')
idle= peak=0 samples=0

echo "sampling pid $pid every ${INTERVAL}s"
while kill -0 "$pid" 2>/dev/null; do
  cur=$(rss_kb "$pid")
  [ -n "$cur" ] || break
  [ -n "$idle" ] || idle=$cur
  (( cur > peak )) && peak=$cur
  samples=$((samples + 1))
  if (( max_samples > 0 && samples >= max_samples )); then break; fi
  sleep "$INTERVAL"
done

if [ -n "$child" ] && kill -0 "$child" 2>/dev/null; then
  kill "$child" 2>/dev/null
fi

hwm=$(hwm_kb "$pid")
(( idle > 0 )) || idle=0
echo "idle RSS:  $(mb "$idle")"
echo "peak RSS:  $(mb "$peak")$( [ -n "${hwm:-}" ] && echo " (VmHWM $(mb "$hwm"))")"
echo "samples:   $samples over $(awk -v n="$samples" -v i="$INTERVAL" 'BEGIN{printf "%.1f s", n*i}')"
