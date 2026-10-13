#!/bin/sh
# Builds the app bundle and runs it as its own app, so the permission prompt
# names Boom3D and not the terminal. Shows its log; Ctrl-C quits the app.
# Arguments are passed on: scripts/run.sh --seconds 10
set -eu
cd "$(dirname "$0")/.."

# A running copy would keep playing instead of the new build, and replacing
# its binary under it can get it killed
if pgrep -x boom3d >/dev/null; then
	echo "stopping the running Boom3D"
	pkill -TERM -x boom3d || true
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		pgrep -x boom3d >/dev/null || break
		sleep 0.5
	done
	pkill -KILL -x boom3d || true
fi

scripts/bundle.sh

log="$PWD/target/boom3d.log"
: > "$log"
open -W --stdout "$log" --stderr "$log" target/Boom3D.app --args "$@" &
app=$!
tail -f "$log" &
log_tail=$!

trap 'pkill -TERM -x boom3d || true' INT TERM
while kill -0 "$app" 2>/dev/null; do
	wait "$app" || true
done
kill "$log_tail"
