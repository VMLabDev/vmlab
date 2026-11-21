# Sourced by adapter.sh.
# check-shell-paths: no-errexit the adapter suite records failed checks and continues.

CHROME=
stage_chrome() { # once: Chrome for Testing into C:\apps\chrome-win64
    [ -z "$CHROME" ] || return 0
    local zip
    CHROME=failed
    zip=$(fetch_pin chrome-for-testing) || return 1
    note "copying Chrome for Testing into the guest"
    in_lab vmlab cp "$zip" "$VM:C:/apps/chrome-win64.zip" || return 1
    gps 'Set-Location C:\apps; if (Test-Path chrome-win64) { Remove-Item chrome-win64 -Recurse -Force }; tar.exe -xf chrome-win64.zip; Remove-Item chrome-win64.zip; Test-Path C:\apps\chrome-win64\chrome.exe' 900 | grep -q True || return 1
    CHROME=ok
}

check_page() { # check_page NAME chrome|edge PAGE SECONDS [FEATURES]
    local name=$1 browser=$2 page=$3 secs=$4 features=${5:-} rc spid
    note "$name"
    if [ "$browser" = chrome ]; then
        stage_chrome
        [ "$CHROME" = ok ] || { skip "$name" "Chrome for Testing did not download or unpack"; return; }
    fi
    (sleep $((secs - 6)); capture "$STAGE/$name.png") &
    spid=$!
    in_lab vmlab exec --timeout $((secs + 400)) "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \
        "$G\\guest\\Run-Page.ps1" -Browser "$browser" -Page "$page" -Name "$name" -OutDir "$R" -Seconds "$secs" \
        ${features:+-Features "$features"} >"$STAGE/$name.out" 2>&1
    rc=$?
    wait "$spid"
    keep_png "$STAGE/$name.png" "$EV/console-$name.png"
    pull "$name.page.json"
    note "  Run-Page exit $rc"
    if [ $rc = 2 ]; then skip "$name" "$browser is not installed in the guest"
    elif [ -f "$EV/$name.page.json" ]; then ran "$name"
    else skip "$name" "Run-Page.ps1 wrote no result"; fi
}

check_svb_chrome() { check_page svb-chrome chrome svb.html 25; }
check_venc_chrome() { check_page venc-chrome chrome venc.html 45 D3D12VideoEncodeAccelerator,D3D12VideoDecoder; }
check_svb_edge() { check_page svb-edge edge svb.html 25; }
check_venc_edge() { check_page venc-edge edge venc.html 45 D3D12VideoEncodeAccelerator,D3D12VideoDecoder; }

ALL+=(svb-chrome venc-chrome svb-edge venc-edge)
