# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_d2d() {
    [ -n "$EV" ] && [ "$EV" != / ] && [ -d "$EV" ] || return 1
    note "d2d"
    probe d2d 30 d2d_probe.exe --bmp "$R\\d2d.bmp" && ran d2d || skip d2d "the Direct2D probe wrote no result"
    pull d2d.bmp && magick "$EV/d2d.bmp" -strip "$EV/d2d.png" && rm -f "$EV/d2d.bmp"
}

ALL+=(d2d)
