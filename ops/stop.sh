#!/bin/bash
# Stop the autonomous campaign cleanly.
# 1. Touches kill-switch so the loop exits at its next check.
# 2. Waits up to 30s for the in-flight claude iteration to finish.
# 3. Kills the runner if it's still alive.

set -u

REPO="$(cd "$(dirname "$0")/.." && pwd)"
STATE="$REPO/.agentic"
PID_FILE="$STATE/runner.pid"
KILL="$STATE/kill-switch"

mkdir -p "$STATE"
touch "$KILL"
echo "kill-switch placed at $KILL"

if [ ! -f "$PID_FILE" ]; then
  echo "no pid file — runner not tracked"
  exit 0
fi

pid="$(cat "$PID_FILE")"
if ! kill -0 "$pid" 2>/dev/null; then
  echo "pid $pid not running — cleaning up"
  rm -f "$PID_FILE"
  exit 0
fi

echo "waiting up to 30s for pid $pid to exit gracefully…"
for _ in $(seq 1 30); do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "runner exited"
    rm -f "$PID_FILE"
    exit 0
  fi
  sleep 1
done

echo "runner still alive — sending SIGTERM"
kill "$pid" 2>/dev/null || true
sleep 2
kill -0 "$pid" 2>/dev/null && { echo "still alive — SIGKILL"; kill -9 "$pid"; }
rm -f "$PID_FILE"
