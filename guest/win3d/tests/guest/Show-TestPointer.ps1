<#
.SYNOPSIS
Shows a flat full-screen window and steps the mouse pointer through a list of
shapes, one position each, for tests/e2e/pointer.sh.

.DESCRIPTION
Run it in the interactive user's session (tests/guest/Run-Probe.ps1 -Raw).
Each step sets the window's pointer to one shape, moves the pointer to the
step's position and holds it for -SecondsEach. Step i is at
(X0 + i * StepX, Y0) in physical pixels, so the host knows each position
without reading this script's output. "hidden" hides the pointer the way an
application does while the user types (ShowCursor).

The window is one flat colour (-Background), so a console capture shows
whether the pointer is part of the frame.

Writes -OutFile as JSON: the screen size, the background, and for each step
the shape, the position Windows reports, the shape's hotspot and the UTC time.
#>
[CmdletBinding()]
param(
    [string]$OutFile = 'C:\win3d\results\pointer\guest.json',
    # One string, comma-separated: a scheduled task passes no arrays.
    [string]$Shapes = 'arrow,ibeam,sizewe,hidden,arrow',
    [int]$SecondsEach = 6,
    [int]$X0 = 200,
    [int]$Y0 = 300,
    [int]$StepX = 120,
    [string]$Background = '32,96,160',
    [ValidateSet(0, 90, 180, 270)][int]$Rotation = 0
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

# Physical pixels: without this a scaled desktop gives virtual coordinates.
Add-Type -Namespace Win3d -Name Dpi -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();'
[void][Win3d.Dpi]::SetProcessDPIAware()
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
# The Unicode DEVMODE display fields. Keep the full buffer for restoring the mode.
Add-Type -Namespace Win3d -Name Display -MemberDefinition @'
[StructLayout(LayoutKind.Explicit, Size = 220)]
public struct Mode {
    [FieldOffset(68)] public ushort Size;
    [FieldOffset(72)] public uint Fields;
    [FieldOffset(84)] public uint Orientation;
    [FieldOffset(172)] public uint Width;
    [FieldOffset(176)] public uint Height;
}
[DllImport("user32.dll", EntryPoint = "EnumDisplaySettingsW")]
private static extern bool EnumDisplaySettings(IntPtr device, int index, ref Mode mode);
[DllImport("user32.dll", EntryPoint = "ChangeDisplaySettingsW")]
private static extern int ChangeDisplaySettings(ref Mode mode, uint flags);
public static Mode Current() {
    Mode mode = new Mode();
    mode.Size = (ushort)Marshal.SizeOf(typeof(Mode));
    if (!EnumDisplaySettings(IntPtr.Zero, -1, ref mode))
        throw new InvalidOperationException("EnumDisplaySettings failed");
    return mode;
}
public static void Apply(Mode mode) {
    int rc = ChangeDisplaySettings(ref mode, 0);
    if (rc != 0)
        throw new InvalidOperationException("ChangeDisplaySettings returned " + rc);
}
public static void Rotate(Mode mode, uint orientation) {
    if ((mode.Orientation & 1) != (orientation & 1)) {
        uint width = mode.Width;
        mode.Width = mode.Height;
        mode.Height = width;
    }
    mode.Orientation = orientation;
    mode.Fields |= 0x00180080; // DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYORIENTATION
    Apply(mode);
}
'@

$cursors = @{
    arrow  = [Windows.Forms.Cursors]::Arrow
    ibeam  = [Windows.Forms.Cursors]::IBeam
    sizewe = [Windows.Forms.Cursors]::SizeWE
    hand   = [Windows.Forms.Cursors]::Hand
    hidden = [Windows.Forms.Cursors]::Arrow
}
$list = @($Shapes.Split(',') | ForEach-Object { $_.Trim().ToLowerInvariant() } | Where-Object { $_ })
foreach ($s in $list) { if (-not $cursors.ContainsKey($s)) { throw "unknown shape '$s'" } }
$rgb = @($Background.Split(',') | ForEach-Object { [int]$_ })

if (Test-Path -LiteralPath $OutFile) { Remove-Item -LiteralPath $OutFile }
$original = [Win3d.Display]::Current()
$changed = $original.Orientation -ne ($Rotation / 90)
try {
    if ($changed) { [Win3d.Display]::Rotate($original, ($Rotation / 90)) }
    $mode = [Win3d.Display]::Current()
    if ($mode.Orientation -ne ($Rotation / 90)) { throw "Display orientation is $($mode.Orientation), expected $Rotation degrees" }
    $screen = New-Object Drawing.Rectangle(0, 0, $mode.Width, $mode.Height)
    $form = New-Object Windows.Forms.Form
    $form.FormBorderStyle = 'None'
    $form.StartPosition = 'Manual'
    $form.Bounds = $screen
    $form.TopMost = $true
    $form.ShowInTaskbar = $false
    $form.BackColor = [Drawing.Color]::FromArgb($rgb[0], $rgb[1], $rgb[2])

    $state = @{ index = -1; hidden = $false; steps = New-Object System.Collections.ArrayList }
    $timer = New-Object Windows.Forms.Timer
    $timer.Interval = 1500   # the first step, once the window is up
    $timer.Add_Tick({
        if ($state.hidden) { [Windows.Forms.Cursor]::Show(); $state.hidden = $false }
        $state.index++
        if ($state.index -ge $list.Count) { $timer.Stop(); $form.Close(); return }
        $timer.Interval = $SecondsEach * 1000
        $name = $list[$state.index]
        $form.Cursor = $cursors[$name]
        [Windows.Forms.Cursor]::Position = New-Object Drawing.Point(($X0 + $state.index * $StepX), $Y0)
        if ($name -eq 'hidden') { [Windows.Forms.Cursor]::Hide(); $state.hidden = $true }
        $pos = [Windows.Forms.Cursor]::Position
        [void]$state.steps.Add([ordered]@{
            index = $state.index
            shape = $name
            x     = $pos.X
            y     = $pos.Y
            hot_x = $form.Cursor.HotSpot.X
            hot_y = $form.Cursor.HotSpot.Y
            utc   = [DateTime]::UtcNow.ToString('o')
        })
    })
    $form.Add_Shown({ $form.Activate(); $timer.Start() })
    [Windows.Forms.Application]::Run($form)
} finally {
    if ($changed) { [Win3d.Display]::Apply($original) }
}

$dir = Split-Path -Parent $OutFile
if ($dir) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
[ordered]@{
    screen       = [ordered]@{ width = $screen.Width; height = $screen.Height }
    rotation     = $mode.Orientation * 90
    background   = $rgb
    seconds_each = $SecondsEach
    steps        = @($state.steps)
} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $OutFile -Encoding UTF8
