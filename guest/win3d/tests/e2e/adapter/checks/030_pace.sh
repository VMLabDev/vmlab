# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_pace() {
    note "pace"
    probe pace 10 pace_probe.exe && ran pace || skip pace "the pace probe wrote no result"
}

ALL+=(pace)
