if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
  W=$(printf '\033[38;2;255;255;255m'); A=$(printf '\033[38;2;216;180;254m')
  G=$(printf '\033[38;2;122;189;0m'); X=$(printf '\033[38;2;255;45;45m'); R=$(printf '\033[0m')
else
  W=; A=; G=; X=; R=
fi
if [ -t 2 ]; then TTY=1; else TTY=; fi
SPIN=
LOG=$(mktemp)
trap 'spin_stop; rm -f "$LOG"' EXIT

spin_stop() {
  [ -n "$SPIN" ] || return 0
  kill "$SPIN" 2>/dev/null; wait "$SPIN" 2>/dev/null; SPIN=
  printf '\r\033[2K' >&2
}
spin_start() {
  if [ -z "$TTY" ]; then printf '%s│%s   %s\n' "$W" "$R" "$1" >&2; return; fi
  (while :; do for f in ⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏; do
    printf '\r%s│%s   %s%s%s %s ' "$W" "$R" "$A" "$f" "$R" "$1" >&2; sleep 0.08
  done; done) &
  SPIN=$!
}
phase() { spin_stop; printf '\n%s┌──%s %s%s%s\n%s│%s\n' "$W" "$R" "$A" "$1" "$R" "$W" "$R" >&2; }
phase_end() { spin_stop; printf '%s│%s\n%s└%s\n' "$W" "$R" "$W" "$R" >&2; }
ok() { spin_stop; printf '%s│%s   %s[ok]:%s %s\n' "$W" "$R" "$G" "$R" "$1" >&2; }
note() { spin_stop; printf '%s│%s   %s[*]:%s %s\n' "$W" "$R" "$A" "$R" "$1" >&2; }
field() { spin_stop; printf '%s│%s      %-18s%s\n' "$W" "$R" "$1" "$2" >&2; }
row() { spin_stop; printf '%s│%s      %s\n' "$W" "$R" "$1" >&2; }
fail() {
  spin_stop
  printf '%s│%s   %s[FAIL]:%s %s\n' "$W" "$R" "$X" "$R" "$1" >&2
  [ -s "$LOG" ] && sed "s/^/${W}│${R}      /" "$LOG" >&2
  phase_end
  exit 1
}
act() {
  local doing=$1 done=$2; shift 2
  spin_start "$doing"
  if "$@" >"$LOG" 2>&1; then ok "$done"; else fail "$doing failed"; fi
}
