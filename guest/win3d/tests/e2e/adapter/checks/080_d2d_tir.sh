# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_d2d_tir() {
    # The check of target-independent rasterization (docs/d2d-coverage.md) is a
    # script of its own; it writes <evidence-dir>/tir/ and its verdict there.
    note "d2d-tir"
    local top rc
    top=$(dirname "$EV")
    [ -n "$top" ] && [ -d "$top" ] || { skip d2d-tir "no evidence directory"; return; }
    "$HERE/tir.sh" "$LABDIR" "$VM" "$top" >"$STAGE/d2d-tir.out" 2>&1
    rc=$?
    grep -E '^(pass|fail) ' "$STAGE/d2d-tir.out" | while read -r l; do note "  ${l:0:200}"; done
    if [ -s "$top/tir/tir.txt" ]; then
        cp "$top/tir/tir.txt" "$EV/d2d-tir.txt"
        ran d2d-tir
    else
        skip d2d-tir "tests/e2e/tir.sh ended with exit $rc and no verdict: $(tail -1 "$STAGE/d2d-tir.out" | cut -c1-160)"
    fi
}

ALL+=(d2d-tir)
