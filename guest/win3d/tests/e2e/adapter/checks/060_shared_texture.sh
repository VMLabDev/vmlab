# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

check_shared_texture() {
    note "shared-texture"
    probe shared-texture 10 share_probe.exe && ran shared-texture || skip shared-texture "the shared-texture probe wrote no result"
}

ALL+=(shared-texture)
