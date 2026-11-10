#!/bin/sh
set -eu
seconds=${1:-60}
case "$seconds" in ''|*[!0-9]*|0) printf '%s\n' 'seconds must be a positive integer' >&2; exit 2;; esac
root=$(pwd)
out=${CARGO_TARGET_DIR:-$root/target}/fuzz-runs
case "$out" in ''|/) exit 2;; esac
mkdir -p "$out"
for target in qcow2 hv_proto agent_proto; do
    crate=$root/fuzz
    if [ "$target" = agent_proto ]; then crate=$root/guest/agent-proto/fuzz; fi
    corpus=$out/$target/corpus
    artifacts=$out/$target/artifacts
    case "$corpus" in ''|/) exit 2;; esac
    mkdir -p "$corpus" "$artifacts"
    cp -R "$crate/corpus/$target/." "$corpus/"
    (cd "$crate" && cargo +nightly fuzz run "$target" "$corpus" -- \
        -max_total_time="$seconds" -timeout=10 -rss_limit_mb=2048 \
        -max_len=1048576 -artifact_prefix="$artifacts/")
done
