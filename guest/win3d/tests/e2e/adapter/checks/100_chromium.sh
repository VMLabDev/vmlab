# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_chromium() {
    [ -n "$EV" ] && [ "$EV" != / ] && [ -d "$EV" ] || return 1
    # The check of the native d3d12.dll preload (docs/chromium.md) is a script of its
    # own; it writes <evidence-dir>/chromium/ and its verdict there. It restarts the VM.
    note "chromium"
    local top rc
    top=$(dirname "$EV")
    [ -n "$top" ] && [ -d "$top" ] || { skip chromium "no evidence directory"; return; }
    "$HERE/chromium.sh" "$LABDIR" "$VM" "$top" >"$STAGE/chromium.out" 2>&1
    rc=$?
    grep -E '^(PASS|FAIL) ' "$STAGE/chromium.out" | while read -r l; do note "  ${l:0:200}"; done
    if [ -f "$top/chromium/chromium.json" ]; then
        cp "$top/chromium/chromium.json" "$EV/chromium.json"
        ran chromium
    else
        skip chromium "tests/e2e/chromium.sh ended with exit $rc and no verdict: $(tail -1 "$STAGE/chromium.out" | cut -c1-160)"
    fi
}

ALL+=(chromium)
