#!/usr/bin/env bash
# Start WO Mic in the background, or stop it if it is already running.
set -u
dir="${XDG_RUNTIME_DIR:-/tmp}/womic"
pidfile="$dir/womic.pid"
logfile="$dir/womic.log"
mkdir -p "$dir"

if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
  kill "$(cat "$pidfile")"
  rm -f "$pidfile"
  notify-send -a "WO Mic" -i audio-input-microphone "WO Mic" "Stopped"
  exit 0
fi

setsid womic "$@" >"$logfile" 2>&1 &
echo $! >"$pidfile"
notify-send -a "WO Mic" -i audio-input-microphone "WO Mic" \
  "Started. Open WO Mic on the phone (Transport: USB) and press Start."

# Report the outcome of the handshake once.
for _ in $(seq 1 30); do
  sleep 1
  if ! kill -0 "$(cat "$pidfile")" 2>/dev/null; then
    notify-send -u critical -a "WO Mic" -i audio-input-microphone "WO Mic failed" "$(tail -n 3 "$logfile")"
    rm -f "$pidfile"
    exit 1
  fi
  if grep -q "streaming started" "$logfile"; then
    notify-send -a "WO Mic" -i audio-input-microphone "WO Mic" "Connected — select the \"WO Mic\" input"
    exit 0
  fi
done
