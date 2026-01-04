<#
dump_windows.ps1
Enumerate top-level windows and print useful details to help identify the real game window.

Usage:
  .\dump_windows.ps1                 # list all visible windows with details
  .\dump_windows.ps1 -Pid 39556     # list windows belonging to PID 39556 (and descendants)
  .\dump_windows.ps1 -Name Wow       # case-insensitive substring match of process name
  .\dump_windows.ps1 -SaveCsv out.csv
  .\dump_windows.ps1 -SaveJson out.json
#>

param(
    [int]$ProcessId = 0,
    [string]$Name = '',
    [switch]$All,
    [string]$SaveCsv = '',
    [string]$SaveJson = ''
)

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class Win32 {
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);

    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern int GetWindowTextW(IntPtr hWnd, StringBuilder lpString, int nMaxCount);

    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern int GetClassNameW(IntPtr hWnd, StringBuilder lpClassName, int nMaxCount);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);

    [DllImport("user32.dll")]
    public static extern IntPtr GetWindow(IntPtr hWnd, uint uCmd);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT {
        public int left;
        public int top;
        public int right;
        public int bottom;
    }
}
"@

function Get-WindowInfo {
    param([IntPtr]$hWnd)

    $sbTitle = New-Object System.Text.StringBuilder 1024
    [Win32]::GetWindowTextW($hWnd, $sbTitle, $sbTitle.Capacity) | Out-Null
    $title = $sbTitle.ToString()

    $sbClass = New-Object System.Text.StringBuilder 256
    [Win32]::GetClassNameW($hWnd, $sbClass, $sbClass.Capacity) | Out-Null
    $class = $sbClass.ToString()

    $visible = [Win32]::IsWindowVisible($hWnd)

    $winPid = 0
    [Win32]::GetWindowThreadProcessId($hWnd, [ref]$winPid) | Out-Null

    $owner = [Win32]::GetWindow($hWnd, 4)  # GW_OWNER = 4

    $rect = New-Object Win32+RECT
    $hasRect = [Win32]::GetWindowRect($hWnd, [ref]$rect)
    if ($hasRect) {
        $width = $rect.right - $rect.left
        $height = $rect.bottom - $rect.top
    } else {
        $width = 0; $height = 0
    }

    $procName = ''
    try {
        if ($winPid -ne 0) { $procName = (Get-Process -Id $winPid -ErrorAction SilentlyContinue).ProcessName }
    } catch { }

    [pscustomobject]@{
        Handle = ('0x{0:X}' -f $hWnd.ToInt64())
        Hwnd = $hWnd
        PID = $pid
        ProcessName = $procName
        Title = $title
        Class = $class
        Visible = $visible
        OwnerHandle = ('0x{0:X}' -f $owner.ToInt64())
        Left = $rect.left
        Top = $rect.top
        Right = $rect.right
        Bottom = $rect.bottom
        Width = $width
        Height = $height
        Area = ($width * $height)
    }
}

# Collect all top-level windows
$windows = [System.Collections.Generic.List[object]]::new()
[Win32]::EnumWindows({ param($hWnd,$l)
    $windows.Add((Get-WindowInfo -hWnd $hWnd)) | Out-Null
    return $true
}, [IntPtr]::Zero) | Out-Null

$results = $windows | Where-Object {
    if ($ProcessId -ne 0 -and $_.PID -ne $ProcessId) { return $false }
    if ($Name -ne '' -and -not ($_.ProcessName -ilike "*$Name*")) { return $false }
    return $true
}

# Prefer visible, large windows first
$sorted = $results | Sort-Object -Property @{Expression = { -not $_.Visible }; Ascending = $true}, @{Expression = { -$_.Area }; Ascending = $true}

# Default to visible windows unless -All specified
if (-not $All) {
    $sorted = $sorted | Where-Object { $_.Visible -eq $true }
}

# Human-readable header explaining the main table
if ($All) {
    Write-Host "Top-level windows (showing visible + invisible; pass -All to include invisible windows)"
} else {
    Write-Host "Top-level windows (visible only by default; use -All to include invisible windows)"
}
Write-Host "Columns: Handle (HWND hex), PID (Process ID), ProcessName, Visible (True/False), Width, Height, Area (pixels), Class (window class), Title"
Write-Host "Use -Pid <id> or -Name <substring> to filter, and -SaveCsv/-SaveJson to save results."
Write-Host ""

# Output table with wrapped titles for readability
$sorted | Select-Object Handle, PID, ProcessName, Visible, Width, Height, Area, Class, Title | Format-Table -Wrap -AutoSize

if ($SaveCsv -ne '') {
    $sorted | Select-Object Handle, PID, ProcessName, Visible, Left, Top, Width, Height, Area, Class, Title | Export-Csv -Path $SaveCsv -NoTypeInformation -Encoding UTF8
    Write-Host "Saved CSV to $SaveCsv"
} 

if ($SaveJson -ne '') {
    $sorted | ConvertTo-Json -Depth 5 | Out-File -FilePath $SaveJson -Encoding UTF8
    Write-Host "Saved JSON to $SaveJson"
}

# Also print a concise list of candidate windows by PID for quick inspection
Write-Host "\nSummary by PID:"
$grouped = $sorted | Group-Object PID
foreach ($g in $grouped) {
    Write-Host "PID: $($g.Name) - Windows: $($g.Count) - ProcessName: $($g.Group[0].ProcessName)"
    $g.Group | Select-Object Handle, Visible, Width, Height, Area, Class,
        @{Name='Title';Expression={
            if ([string]::IsNullOrEmpty($_.Title)) { '' }
            elseif ($_.Title.Length -gt 80) { $_.Title.Substring(0,80) + '...' }
            else { $_.Title }
        }} | Format-Table -AutoSize
    Write-Host ""
}

Write-Host "Done."