#!/bin/sh
# The functions runtime's entrypoint (busybox sh): runs workerd ("$@") and probes it. workerd runs
# every isolate on one thread and enforces no CPU limit here, so one bundle in a busy loop stops
# every other one. The probe (supervisor.js `probe`, loopback 127.0.0.1:9090) runs a trivial
# worker through the loader every 2s with a 1s timeout; after 3 failures in a row the watchdog
# kills workerd and exits 70, and the container's restart policy (always) starts it again.
# It also exits when workerd does, with its status.
bb=/usr/local/bin/busybox
"$@" &
child=$!
trap 'kill -TERM "$child" 2>/dev/null; wait "$child"; exit 143' TERM INT
fails=0
while kill -0 "$child" 2>/dev/null; do
  $bb sleep 2
  if $bb wget -q -T 1 -O /dev/null http://127.0.0.1:9090/ 2>/dev/null; then
    fails=0
  elif kill -0 "$child" 2>/dev/null; then
    fails=$((fails + 1))
    echo "watchdog: probe failed ($fails/3)" >&2
    if [ "$fails" -ge 3 ]; then
      echo "watchdog: runtime unresponsive, killing workerd" >&2
      kill -KILL "$child"
      wait "$child"
      exit 70
    fi
  fi
done
wait "$child"
exit $?
