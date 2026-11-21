# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_pointer_shapes() {
    [ -n "$EV" ] && [ "$EV" != / ] && [ -d "$EV" ] || return 1
    # The check of the hardware pointer (docs/pointer.md) is a script of its own; it
    # writes <evidence-dir>/pointer/ and its verdict there.
    note "pointer-shapes"
    local top rc
    top=$(dirname "$EV")
    [ -n "$top" ] && [ -d "$top" ] || { skip pointer-shapes "no evidence directory"; return; }
    "$HERE/pointer.sh" "$LABDIR" "$VM" "$top" >"$STAGE/pointer-shapes.out" 2>&1
    rc=$?
    grep -E '^(PASS|FAIL) ' "$STAGE/pointer-shapes.out" | while read -r l; do note "  $l"; done
    if [ -f "$top/pointer/pointer.json" ]; then
        cp "$top/pointer/pointer.json" "$EV/pointer-shapes.json"
        ran pointer-shapes
    else
        skip pointer-shapes "tests/e2e/pointer.sh ended with exit $rc and no verdict: $(tail -1 "$STAGE/pointer-shapes.out" | cut -c1-160)"
    fi
}

ALL+=(pointer-shapes)
