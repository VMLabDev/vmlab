#!/bin/sh
# vmlab guest bootstrap for macOS, first boot: create the administrator the
# build named (macos/install.sh --user), as root from a one-shot LaunchDaemon,
# with no GUI and no Setup Assistant. Then remove this daemon and the file
# holding the password, so neither outlives the boot that needed them.
#
# Reads /Library/vmlab/firstboot.conf (user=, full_name=, password=), written
# by install.sh from the Recovery Terminal. Logs to
# /var/log/vmlab-firstboot.log, never the password.

conf=/Library/vmlab/firstboot.conf
plist=/Library/LaunchDaemons/dev.vmlab.firstboot.plist

log() {
    echo "vmlab firstboot: $*"
}

finish() {
    rm -f "$conf" "$plist" /Library/vmlab/firstboot.sh
    exit "$1"
}

[ -f "$conf" ] || { log "no $conf; nothing to do"; finish 0; }
user="$(sed -n 's/^user=//p' "$conf")"
full_name="$(sed -n 's/^full_name=//p' "$conf")"
password="$(sed -n 's/^password=//p' "$conf")"
[ -n "$user" ] && [ -n "$password" ] || { log "$conf names no user and password"; finish 1; }

# opendirectoryd may still be starting this early in boot.
i=0
until dscl . -list /Users >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -ge 60 ] && { log "the directory service never answered"; finish 1; }
    sleep 2
done

if id "$user" >/dev/null 2>&1; then
    log "account $user already exists; left as it is"
else
    if ! sysadminctl -addUser "$user" -fullName "$full_name" -password "$password" -admin; then
        log "sysadminctl could not create $user"
        finish 1
    fi
    log "created administrator $user (uid $(id -u "$user"))"
fi

# The per-user setup screens a first login would otherwise walk through, each
# marked seen, so that login lands on the desktop.
home="$(dscl . -read "/Users/$user" NFSHomeDirectory | sed 's/^NFSHomeDirectory: //')"
prefs="$home/Library/Preferences/com.apple.SetupAssistant"
mkdir -p "$home/Library/Preferences"
for key in DidSeeCloudSetup DidSeePrivacy DidSeeSiriSetup DidSeeTouchIDSetup \
    DidSeeScreenTime DidSeeAppearanceSetup DidSeeAccessibility DidSeeActivationLock \
    DidSeeTrueTonePrivacy DidSeeApplePaySetup DidSeeAvatarSetup DidSeeLockdownMode \
    DidSeeIntelligence DidSeeTermsOfAddress DidSeeWallpaper DidSeeSoftwareUpdate \
    SkipFirstLoginOptimization; do
    defaults write "$prefs" "$key" -bool true
done
defaults write "$prefs" LastSeenCloudProductVersion "$(sw_vers -productVersion)"
defaults write "$prefs" LastSeenBuddyBuildVersion "$(sw_vers -buildVersion)"
defaults write "$prefs" LastPrivacyBundleVersion -string 2
chown -R "$user:staff" "$home/Library/Preferences"

finish 0
