# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_wine() {
    note "wine"
    local exe base rc n=0 t0 exits
    if ! compgen -G "$OUT/winetests/x64/*.exe" >/dev/null; then
        skip wine "Wine's tests were not built (the pin did not download, or the build failed)"
        return
    fi
    in_lab vmlab cp "$OUT/winetests/x64" "$VM:C:/win3d/adapter/winetests" || { skip wine "the tests did not copy into the guest"; return; }
    for exe in "$OUT"/winetests/x64/*.exe; do basename "$exe" .exe; done >"$EV/wine.programs"
    for exe in "$OUT"/winetests/x64/*.exe; do
        base=$(basename "$exe" .exe)
        if ! guest_up && ! vm_restart; then note "  the guest does not answer; $base not run"; continue; fi
        note "  $base (at most $WINE_TIMEOUT s)"
        t0=$(date +%s)
        exits=$(hv_exits)
        in_lab vmlab exec --timeout $((WINE_TIMEOUT + 400)) "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \
            "$G\\guest\\Run-WineTest.ps1" -Path "$G\\winetests\\$base.exe" -OutDir "$R" -TimeoutSec "$WINE_TIMEOUT" \
            >"$STAGE/wine-$base.out" 2>&1
        rc=$?
        if pull "wine-$base.wine.json"; then
            pull "wine-$base.failures.txt"
        elif ! guest_up; then
            # The program took the VM with it: say so in its place.
            python3 -c 'import json,sys; json.dump({"name": sys.argv[1], "completed": False, "vm_lost": True, "duration_s": int(sys.argv[2]), "hypervisor": sys.argv[3]}, open(sys.argv[4], "w"), indent=2)' \
                "$base" "$(($(date +%s) - t0))" "$(hv_exit "${exits:-0}")" "$EV/wine-$base.wine.json"
        fi
        [ -f "$EV/wine-$base.wine.json" ] && n=$((n + 1))
        note "    exit $rc: $(python3 -c 'import json,sys; d=json.load(open(sys.argv[1],encoding="utf-8-sig")); print("the VM stopped: " + d["hypervisor"] if d.get("vm_lost") else "%s executed, %s failures, %s skipped, %s s%s%s" % (d["executed"], d["failures"], d["skipped"], d["duration_s"], ", timed out" if d["timed_out"] else "", ", unhandled exception" if d["crashed"] else ""))' "$EV/wine-$base.wine.json" 2>/dev/null || echo 'no result')"
    done
    [ $n -gt 0 ] && ran wine || skip wine "no Wine test program wrote a result"
}

ALL+=(wine)
