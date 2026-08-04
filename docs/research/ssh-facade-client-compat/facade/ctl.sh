#!/bin/bash
# start/stop the stand-in facade via a pidfile, so nothing has to pgrep for a
# pattern that its own command line contains.
F=/run/user/1000/vf
PY=/tmp/claude-1000/-home-wil-orca-vmlab/20a91bdb-7753-45f8-bd70-5ff8d430f737/scratchpad/venv/bin/python
PIDFILE=$F/run/facade.pid

case "$1" in
  start)
    if [ -f "$PIDFILE" ] && kill -0 "$(cat $PIDFILE)" 2>/dev/null; then
      echo "already running pid=$(cat $PIDFILE)"; exit 0
    fi
    [ "$2" = "--fresh" ] && rm -f $F/run/trace.jsonl $F/run/facade.log
    cd $F
    nohup "$PY" $F/facade.py \
      --socket $F/run/lab.sock \
      --host-key $F/run/host_ed25519 \
      --log $F/run/facade.log \
      --trace $F/run/trace.jsonl > $F/run/stdout.log 2>&1 &
    echo $! > $PIDFILE
    sleep 3
    cat $F/run/stdout.log
    ;;
  stop)
    if [ -f "$PIDFILE" ]; then kill "$(cat $PIDFILE)" 2>/dev/null; rm -f $PIDFILE; fi
    sleep 1; echo stopped
    ;;
  status)
    if [ -f "$PIDFILE" ] && kill -0 "$(cat $PIDFILE)" 2>/dev/null; then
      echo "running pid=$(cat $PIDFILE)"
    else echo "not running"; fi
    ;;
esac
