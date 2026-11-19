<#
.SYNOPSIS
Reads the flip trace of a trace-build kernel driver (build/kmd/build.sh with
KMD_TRACE=1, patches/kmd/0006) into a CSV file.

.DESCRIPTION
Polls the driver's DRIVERPRIVATE escape (magic 0x56475446) for -Seconds and
appends every record: seq, time in 100 ns units of the interrupt clock,
event, context, flags, fence, response, pending. build/kmd/README.md lists
the events. A release driver has no such escape: the script then exits 3.
#>
param(
    [int]$Seconds = 10,
    [string]$Out = 'C:\win3d\results\fliptrace.csv',
    # Start with the records already in the ring (about the last 65536).
    [switch]$FromStart,
    # An experiment of the trace build: stall every present copy for this many
    # microseconds when it is half done (0 turns it off), then exit.
    [int]$Stall = -1,
    # A comparison of the trace build: 1 makes a vsync report queued flips as
    # the driver did before patches/kmd/0007, also one that was not sent to
    # the host yet. 0 is the patched order. Sets it, then exits.
    [int]$ReportUnsent = -1
)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;

public static class Win3dFlipTrace {
    [StructLayout(LayoutKind.Sequential)]
    struct ENUMADAPTERS2 { public uint NumAdapters; public IntPtr pAdapters; }
    [StructLayout(LayoutKind.Sequential)]
    struct ESCAPE {
        public uint hAdapter, hDevice; public int Type; public uint Flags;
        public IntPtr pPrivateDriverData; public uint PrivateDriverDataSize; public uint hContext;
    }
    [DllImport("gdi32.dll")] static extern int D3DKMTEnumAdapters2(ref ENUMADAPTERS2 e);
    [DllImport("gdi32.dll")] static extern int D3DKMTEscape(ref ESCAPE e);

    const int Records = 1024, RecordSize = 56, Header = 32, Size = Header + Records * RecordSize;
    static uint adapter;
    static IntPtr buffer = Marshal.AllocHGlobal(Size);

    static int Read(uint handle, ulong after) {
        for (int i = 0; i < Header; i++) Marshal.WriteByte(buffer, i, 0);
        Marshal.WriteInt32(buffer, 0, 0x56475446);
        Marshal.WriteInt32(buffer, 4, 1);
        Marshal.WriteInt32(buffer, 8, Size);
        Marshal.WriteInt64(buffer, 16, (long)after);
        var e = new ESCAPE { hAdapter = handle, Type = 0, pPrivateDriverData = buffer, PrivateDriverDataSize = (uint)Size };
        return D3DKMTEscape(ref e);
    }

    // The adapter that answers the escape, or false when none does.
    public static bool Open() {
        var list = new ENUMADAPTERS2();
        if (D3DKMTEnumAdapters2(ref list) != 0 || list.NumAdapters == 0) return false;
        list.pAdapters = Marshal.AllocHGlobal((int)list.NumAdapters * 20);
        if (D3DKMTEnumAdapters2(ref list) != 0) return false;
        for (int i = 0; i < list.NumAdapters; i++) {
            uint handle = (uint)Marshal.ReadInt32(list.pAdapters, i * 20);
            if (Read(handle, ulong.MaxValue >> 1) == 0) { adapter = handle; return true; }
        }
        return false;
    }

    // Knob 1: microseconds to hold each present copy. Knob 2: report unsent flips.
    public static bool SetKnob(uint knob, uint value) {
        Marshal.WriteInt32(buffer, 0, 0x56475453);
        Marshal.WriteInt32(buffer, 4, (int)knob);
        Marshal.WriteInt32(buffer, 8, (int)value);
        var e = new ESCAPE { hAdapter = adapter, Type = 0, pPrivateDriverData = buffer, PrivateDriverDataSize = 12 };
        return D3DKMTEscape(ref e) == 0;
    }

    public static ulong Last() {
        Read(adapter, ulong.MaxValue >> 1);
        return (ulong)Marshal.ReadInt64(buffer, 24);
    }

    // Appends the records after `after`; returns the sequence to continue from.
    public static ulong Drain(ulong after, TextWriter w) {
        while (true) {
            if (Read(adapter, after) != 0) return after;
            int count = Marshal.ReadInt32(buffer, 12);
            ulong last = (ulong)Marshal.ReadInt64(buffer, 24);
            for (int i = 0; i < count; i++) {
                int o = Header + i * RecordSize;
                ulong seq = (ulong)Marshal.ReadInt64(buffer, o);
                w.WriteLine("{0},{1},{2},{3},{4},{5},{6},{7}", seq, Marshal.ReadInt64(buffer, o + 8),
                    Marshal.ReadInt32(buffer, o + 20), (uint)Marshal.ReadInt32(buffer, o + 32),
                    (uint)Marshal.ReadInt32(buffer, o + 36), Marshal.ReadInt64(buffer, o + 40),
                    (uint)Marshal.ReadInt32(buffer, o + 48), (uint)Marshal.ReadInt32(buffer, o + 52));
                after = seq;
            }
            if (count < Records) return count == 0 ? Math.Max(after, 0) : after;
        }
    }
}
'@
if (-not [Win3dFlipTrace]::Open()) {
    Write-Output 'no adapter answers the flip trace escape: not a trace build of viogpu3d.sys'
    exit 3
}
if ($Stall -ge 0) {
    "stall $Stall us: {0}" -f [Win3dFlipTrace]::SetKnob(1, [uint32]$Stall)
    exit 0
}
if ($ReportUnsent -ge 0) {
    "report unsent flips $ReportUnsent`: {0}" -f [Win3dFlipTrace]::SetKnob(2, [uint32]$ReportUnsent)
    exit 0
}
New-Item -ItemType Directory -Force (Split-Path -Parent $Out) | Out-Null
$writer = New-Object IO.StreamWriter $Out, $false
$writer.WriteLine('seq,time,event,context,flags,fence,response,pending')
$after = if ($FromStart) { [uint64]0 } else { [Win3dFlipTrace]::Last() }
$first = $after
$end = (Get-Date).AddSeconds($Seconds)
do {
    $after = [Win3dFlipTrace]::Drain($after, $writer)
    Start-Sleep -Milliseconds 100
} while ((Get-Date) -lt $end)
$after = [Win3dFlipTrace]::Drain($after, $writer)
$writer.Close()
"records after sequence $first up to $after in $Out"
