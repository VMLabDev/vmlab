#!/bin/sh
# vmlab guest bootstrap: install the vmlab-agent service from the VMLAB ISO.
# Run by the template's own unattended-install hook during a template build
# (cloud-init runcmd on cloud images; subiquity late-commands on installer
# ISOs). Two modes:
#
#   install.sh              live system — install, register and START the
#                           service (systemd or OpenRC)
#   install.sh --root DIR   offline install into a mounted target tree
#                           (installer environments): install + enable only,
#                           the service starts on the target's first boot
set -e

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
arch="$(uname -m)"
bin="$here/linux/$arch/vmlab-agent"
if [ ! -f "$bin" ]; then
    echo "vmlab bootstrap: no agent binary for $arch on the VMLAB ISO" >&2
    exit 1
fi

root=""
if [ "$1" = "--root" ]; then
    root="$2"
    if [ ! -d "$root" ]; then
        echo "vmlab bootstrap: --root $root is not a directory" >&2
        exit 1
    fi
fi

mkdir -p "$root/usr/local/lib/vmlab"
cp "$bin" "$root/usr/local/lib/vmlab/vmlab-agent"
chmod 0755 "$root/usr/local/lib/vmlab/vmlab-agent"

write_unit() {
    cat > "$1" <<'EOF'
[Unit]
Description=vmlab guest agent (terminals/exec/files over virtio-serial)

[Service]
ExecStart=/usr/local/lib/vmlab/vmlab-agent
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF
}

# No `need` dependencies: Alpine's cloud image leaves `localmount` out of
# every runlevel, and OpenRC silently skips a service whose needs can't be
# scheduled. The agent retries its port until the device exists, so
# ordering is soft.
write_openrc() {
    cat > "$1" <<'EOF'
#!/sbin/openrc-run
description="vmlab guest agent (terminals/exec/files over virtio-serial)"
command="/usr/local/lib/vmlab/vmlab-agent"
command_background="yes"
pidfile="/run/vmlab-agent.pid"

depend() {
	after bootmisc
}
EOF
}

if [ -n "$root" ]; then
    # Offline target: every current installer-ISO distro is systemd; enable
    # via the wants symlink so the first real boot starts the agent.
    write_unit "$root/etc/systemd/system/vmlab-agent.service"
    mkdir -p "$root/etc/systemd/system/multi-user.target.wants"
    ln -sf ../vmlab-agent.service \
        "$root/etc/systemd/system/multi-user.target.wants/vmlab-agent.service"
    echo "vmlab bootstrap: agent installed into $root (systemd, enabled)"
elif [ -d /run/systemd/system ]; then
    write_unit /etc/systemd/system/vmlab-agent.service
    systemctl daemon-reload
    systemctl enable --now vmlab-agent.service
    echo "vmlab bootstrap: agent installed (systemd, started)"
elif [ -x /sbin/openrc-run ]; then
    write_openrc /etc/init.d/vmlab-agent
    chmod 0755 /etc/init.d/vmlab-agent
    rc-update add vmlab-agent default
    rc-service vmlab-agent start
    # OpenRC's deptree freshness check is whole-second granular, and
    # everything above lands within one second: the pre-vmlab-agent runtime
    # deptree (mtime-tied with /etc/init.d) gets persisted by savecache at
    # the seal shutdown and every clone boot then restores it as "fresh" —
    # the service silently never starts. Step out of the second, drop the
    # stale caches, and regenerate the deptree so what savecache persists
    # is correct.
    sleep 1
    touch /etc/init.d /etc/init.d/vmlab-agent
    rm -f /lib/rc/cache/deptree /lib/rc/cache/depconfig
    rc-update -u
    echo "vmlab bootstrap: agent installed (OpenRC, started)"
else
    echo "vmlab bootstrap: neither systemd nor OpenRC; binary installed but no service registered" >&2
    exit 1
fi
