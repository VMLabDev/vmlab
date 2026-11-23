# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_draws() {
    note "draws"
    probe draws 10 draws_probe.exe && ran draws || skip draws "the draws probe wrote no result"
}

ALL+=(draws)
