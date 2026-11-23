#!/usr/bin/env bash
# stop_server.sh: stop the bench server recorded in server.pid (only if its cmdline is eval/bench/server.py).
set -u
here="$(cd "$(dirname "$0")" && pwd)"
pidfile="$here/server.pid"
if [ ! -f "$pidfile" ]; then echo "no server.pid: nothing to stop"; exit 0; fi
pid="$(cat "$pidfile")"
if [ -r "/proc/$pid/cmdline" ] && tr '\0' ' ' < "/proc/$pid/cmdline" | grep -q "$here/server.py"; then
  kill -TERM "$pid"
  for _ in $(seq 1 30); do [ -e "/proc/$pid" ] || break; sleep 0.1; done
  if [ -e "/proc/$pid" ]; then kill -KILL "$pid"; echo "KILL $pid"; else echo "TERM $pid ok"; fi
else
  echo "pid $pid is not eval/bench/server.py; not touched"
fi
rm -f "$pidfile"
if ss -ltn | grep -q ':18295 '; then echo "port 18295 still listening"; ss -ltnp | grep ':18295 '; else echo "port 18295 released"; fi
