# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_duplication() {
    note "duplication"
    probe duplication 10 dupl_probe.exe && ran duplication || skip duplication "the duplication probe wrote no result"
}

ALL+=(duplication)
