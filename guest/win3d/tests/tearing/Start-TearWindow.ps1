<#
.SYNOPSIS
The tearing probe's content: a WPF window (hardware Direct3D 9) with eight
solid rectangles that all move together.

.DESCRIPTION
Each rectangle has its own colour and its own lane. On every WPF frame all
eight move to the same x position, so in a frame that was composed and
flipped in order, every row of every rectangle starts at the same column.
A row that starts elsewhere is a row from an earlier frame: a torn frame
(card #134). tests/tearing/rfbgrab.py reads the console and counts them.

Run it in the desktop session, for example through Run-Probe.ps1 -Raw.
It writes <Out> as JSON when it ends: the WPF render tier, the frames drawn
and the frame rate.

.PARAMETER Seconds
How long the window stays open.
.PARAMETER Animate
.PARAMETER Freeze
Animate for this many seconds, then hold still for Freeze seconds, repeated.
Freeze 0 (the default) animates all the time.
#>
param(
    [int]$Seconds = 60,
    [double]$Animate = 4,
    [double]$Freeze = 0,
    [int]$Step = 16,
    [string]$Out = 'C:\win3d\results\tearwindow.json'
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName PresentationCore, PresentationFramework, WindowsBase

# Keep in step with LANES in rfbgrab.py.
$colours = '#FE0114', '#01FE28', '#1401FE', '#FEC801', '#01C8FE', '#C801FE', '#FE6432', '#32FE96'
$laneHeight = 60; $rectWidth = 96; $rectHeight = 44; $width = 768
$range = $width - $rectWidth

$window = New-Object Windows.Window
$window.WindowStyle = 'None'; $window.ResizeMode = 'NoResize'; $window.Topmost = $true
$window.ShowInTaskbar = $false; $window.Left = 64; $window.Top = 64
$window.Width = $width; $window.Height = $laneHeight * $colours.Count
$window.Background = [Windows.Media.Brushes]::Black
$window.UseLayoutRounding = $true; $window.SnapsToDevicePixels = $true
$canvas = New-Object Windows.Controls.Canvas
[Windows.Media.RenderOptions]::SetEdgeMode($canvas, 'Aliased')
$window.Content = $canvas
$rects = @()
for ($i = 0; $i -lt $colours.Count; $i++) {
    $r = New-Object Windows.Shapes.Rectangle
    $r.Width = $rectWidth; $r.Height = $rectHeight
    $r.Fill = New-Object Windows.Media.SolidColorBrush ([Windows.Media.ColorConverter]::ConvertFromString($colours[$i]))
    [Windows.Controls.Canvas]::SetTop($r, $i * $laneHeight + 8)
    [Windows.Controls.Canvas]::SetLeft($r, 0)
    [void]$canvas.Children.Add($r); $rects += $r
}

$state = @{ Frames = 0; Moves = 0 }
$clock = [Diagnostics.Stopwatch]::StartNew()
$period = $Animate + $Freeze
$handler = [EventHandler] {
    $state.Frames++
    $t = $clock.Elapsed.TotalSeconds
    if ($t -ge $Seconds) { $window.Close(); return }
    if ($Freeze -gt 0 -and ($t % $period) -ge $Animate) { return }
    $state.Moves++
    $x = ($state.Moves * $Step) % $range
    foreach ($r in $rects) { [Windows.Controls.Canvas]::SetLeft($r, $x) }
}
[Windows.Media.CompositionTarget]::add_Rendering($handler)
[void]$window.ShowDialog()
[Windows.Media.CompositionTarget]::remove_Rendering($handler)

$elapsed = $clock.Elapsed.TotalSeconds
$result = [ordered]@{
    render_tier = [Windows.Media.RenderCapability]::Tier -shr 16
    frames = $state.Frames; moves = $state.Moves
    seconds = [math]::Round($elapsed, 2)
    fps = [math]::Round($state.Frames / [math]::Max($elapsed, 0.001), 1)
    step = $Step; freeze = $Freeze
}
New-Item -ItemType Directory -Force (Split-Path -Parent $Out) | Out-Null
$result | ConvertTo-Json | Set-Content -LiteralPath $Out -Encoding UTF8
