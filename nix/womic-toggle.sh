#!/usr/bin/env bash
# Start WO Mic in the background, or stop it if it is already running.
#   womic-toggle            toggle
#   womic-toggle start|stop
#   womic-toggle mute       toggle mute (keeps the "WO Mic" input alive)
#   womic-toggle status     prints connected | muted | connecting | off
set -u
dir="${XDG_RUNTIME_DIR:-/tmp}/womic"
pidfile="$dir/womic.pid"
logfile="$dir/womic.log"
mutefile="$dir/muted"
icon="${WOMIC_ICON:-audio-input-microphone}"
mkdir -p "$dir"

running() { [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; }

status() {
  if ! running; then
    echo off
  elif [ -f "$mutefile" ]; then
    echo muted
  # Last connection event in the log decides: a later "disconnected" wins.
  elif grep -E "streaming started|disconnected" "$logfile" 2>/dev/null | tail -n 1 | grep -q "streaming started"; then
    echo connected
  else
    echo connecting
  fi
}

stop() {
  running && kill "$(cat "$pidfile")"
  rm -f "$pidfile"
  notify-send -a "WO Mic" -i "$icon" "WO Mic" "Stopped"
}

start() {
  setsid womic "$@" >"$logfile" 2>&1 &
  echo $! >"$pidfile"
  notify-send -a "WO Mic" -i "$icon" "WO Mic" \
    "Started. Open WO Mic on the phone (Transport: USB) and press Start."

  # Report the outcome of the handshake once.
  for _ in $(seq 1 30); do
    sleep 1
    if ! running; then
      notify-send -u critical -a "WO Mic" -i "$icon" "WO Mic failed" "$(tail -n 3 "$logfile")"
      rm -f "$pidfile"
      return 1
    fi
    if [ "$(status)" = connected ]; then
      notify-send -a "WO Mic" -i "$icon" "WO Mic" "Connected — select the \"WO Mic\" input"
      return 0
    fi
  done
}

case "${1:-toggle}" in
  status) status ;;
  stop) running && stop ;;
  mute) running && kill -USR1 "$(cat "$pidfile")" ;;
  start) shift; running || start "$@" ;;
  toggle) if running; then stop; else start; fi ;;
  *) start "$@" ;;
esac
