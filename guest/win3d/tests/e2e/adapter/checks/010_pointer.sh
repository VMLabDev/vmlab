# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_pointer() {
    note "pointer"
    local w h x y i pid shot=$STAGE/pointer.png
    # The probe fills the display for 30 s; the pointer is moved while it does.
    probe pointer 30 pointer_probe.exe &
    pid=$!
    for i in $(seq 1 60); do
        sleep 2
        capture "$shot" || continue
        python3 "$A/pointerframe.py" "$shot" 0 0 >"$STAGE/pointer-wait.json" 2>/dev/null || continue
        python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1]))["background"] > 0.9 else 1)' "$STAGE/pointer-wait.json" && break
        kill -0 "$pid" 2>/dev/null || break
    done
    if [ -f "$STAGE/pointer-wait.json" ]; then
        read -r w h < <(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["width"], d["height"])' "$STAGE/pointer-wait.json")
        i=0
        for p in "$((w / 4)) $((h / 4))" "$((w * 3 / 5)) $((h * 3 / 5))"; do
            i=$((i + 1))
            read -r x y <<<"$p"
            in_lab vmlab vm mouse-move "$VM" "$x" "$y" >/dev/null 2>&1 || note "  mouse-move failed"
            sleep 2
            capture "$STAGE/pointer-$i.png" && python3 "$A/pointerframe.py" "$STAGE/pointer-$i.png" "$x" "$y" >"$EV/pointer-frame-$i.json"
            keep_png "$STAGE/pointer-$i.png" "$EV/console-pointer-$i.png"
        done
    fi
    wait "$pid" && ran pointer || skip pointer "the pointer probe wrote no result"
}

ALL+=(pointer)
