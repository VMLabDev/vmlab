#!/bin/sh
# vmlab guest bootstrap for macOS: install the vmlab-agent LaunchDaemon onto a
# freshly installed macOS, from the Recovery Terminal, before its first boot.
# Recovery mounts the VMLAB ISO at /Volumes/VMLAB and the new system's Data
# volume read-write, so everything here is a file written onto that volume:
#
#   /Volumes/VMLAB/macos/install.sh [--user NAME --password PW
#       [--full-name TEXT]] [DATA-VOLUME]
#
# DATA-VOLUME defaults to "/Volumes/Macintosh HD - Data". It installs:
#
#   /Library/vmlab/vmlab-agent                      the agent
#   /Library/LaunchDaemons/dev.vmlab.agent.plist    root, RunAtLoad, KeepAlive
#   /var/db/.AppleSetupDone                         Setup Assistant skipped
#
# and, with --user, a one-shot first-boot daemon (firstboot.sh) that creates
# that account as an administrator with sysadminctl, marks its own per-user
# setup screens seen, and then removes itself and the file holding the
# password. Without --user no account is made: the agent, as root, is the
# only way in.
#
# The account is an argument rather than a file on the ISO because the build
# types this command anyway, and the ISO stays the same for every template —
# no password is ever written into vmlab's media cache.
set -e

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
user=""
password=""
full_name=""
data="/Volumes/Macintosh HD - Data"

die() {
    echo "vmlab bootstrap: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --user) [ $# -ge 2 ] || die "--user needs a name"; user="$2"; shift 2 ;;
        --password) [ $# -ge 2 ] || die "--password needs a value"; password="$2"; shift 2 ;;
        --full-name) [ $# -ge 2 ] || die "--full-name needs a value"; full_name="$2"; shift 2 ;;
        -*) die "unknown option $1" ;;
        *) data="$1"; shift ;;
    esac
done

[ -f "$here/vmlab-agent" ] || die "no macOS agent binary beside $0 on the VMLAB ISO"
[ -d "$data" ] || die "no Data volume at $data (is it mounted? see: ls /Volumes)"
[ -d "$data/private/var" ] || die "$data is not a macOS Data volume (no private/var)"
if [ -n "$user" ] && [ -z "$password" ]; then
    die "--user $user needs --password"
fi
case "$user" in
    *[!a-z0-9_.-]*) die "--user $user: a short name is lowercase letters, digits, '_', '.', '-'" ;;
esac

mkdir -p "$data/Library/vmlab" "$data/Library/LaunchDaemons" "$data/private/var/db"
cp "$here/vmlab-agent" "$data/Library/vmlab/vmlab-agent"
chown root:wheel "$data/Library/vmlab" "$data/Library/vmlab/vmlab-agent"
chmod 0755 "$data/Library/vmlab" "$data/Library/vmlab/vmlab-agent"

# The agent finds its port by name (/dev/cu.vmlab.agent.0) and retries until
# the virtio console has bound, so it needs no arguments and no ordering.
# launchd starts a daemon with PATH=/usr/bin:/bin:/usr/sbin:/sbin; the one
# below is /etc/paths', which every login gets, so an exec finds what a
# person's shell finds — /usr/local/bin is where Homebrew lives on Intel.
cat > "$data/Library/LaunchDaemons/dev.vmlab.agent.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>dev.vmlab.agent</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Library/vmlab/vmlab-agent</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/usr/local/bin:/System/Cryptexes/App/usr/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
	<key>StandardErrorPath</key>
	<string>/var/log/vmlab-agent.log</string>
</dict>
</plist>
EOF
chown root:wheel "$data/Library/LaunchDaemons/dev.vmlab.agent.plist"
chmod 0644 "$data/Library/LaunchDaemons/dev.vmlab.agent.plist"

if [ -n "$user" ]; then
    cp "$here/firstboot.sh" "$data/Library/vmlab/firstboot.sh"
    chown root:wheel "$data/Library/vmlab/firstboot.sh"
    chmod 0755 "$data/Library/vmlab/firstboot.sh"
    # Root-only, and deleted by firstboot.sh once the account exists.
    umask 077
    {
        printf 'user=%s\n' "$user"
        printf 'full_name=%s\n' "${full_name:-$user}"
        printf 'password=%s\n' "$password"
    } > "$data/Library/vmlab/firstboot.conf"
    umask 022
    chown root:wheel "$data/Library/vmlab/firstboot.conf"
    cat > "$data/Library/LaunchDaemons/dev.vmlab.firstboot.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>dev.vmlab.firstboot</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Library/vmlab/firstboot.sh</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>StandardOutPath</key>
	<string>/var/log/vmlab-firstboot.log</string>
	<key>StandardErrorPath</key>
	<string>/var/log/vmlab-firstboot.log</string>
</dict>
</plist>
EOF
    chown root:wheel "$data/Library/LaunchDaemons/dev.vmlab.firstboot.plist"
    chmod 0644 "$data/Library/LaunchDaemons/dev.vmlab.firstboot.plist"
fi

# Setup Assistant runs while .AppleSetupDone is absent. A session that ever
# reached its Terms page leaves .AppleSetupTermsOfService behind, which brings
# that page back on its own, so it goes too.
touch "$data/private/var/db/.AppleSetupDone"
chown root:wheel "$data/private/var/db/.AppleSetupDone"
rm -f "$data/private/var/db/.AppleSetupTermsOfService"

echo "vmlab bootstrap: agent installed on $data${user:+; first boot creates admin $user}"
