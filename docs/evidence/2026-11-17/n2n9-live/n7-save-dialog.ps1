
param([string]$Dest, [int]$WaitSeconds = 25)
Write-Output 'start'
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class W {
  public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumWindowsProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, string l);
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
Write-Output 'compiled'
$script:target = [IntPtr]::Zero
$script:others = @()
$deadline = (Get-Date).AddSeconds($WaitSeconds)
while ((Get-Date) -lt $deadline -and $script:target -eq [IntPtr]::Zero) {
  $script:others = @()
  [W]::EnumWindows({ param($h, $l)
    if (-not [W]::IsWindowVisible($h)) { return $true }
    $cls = New-Object System.Text.StringBuilder 256
    [void][W]::GetClassName($h, $cls, 256)
    if ($cls.ToString() -eq '#32770') {
      $title = New-Object System.Text.StringBuilder 512
      [void][W]::GetWindowText($h, $title, 512)
      $t = $title.ToString()
      $script:others += ('#32770|' + $t)
      if ($t -match '保存|Save|另存') { $script:target = $h; return $false }
    }
    return $true
  }, [IntPtr]::Zero) | Out-Null
  if ($script:target -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 400 }
}
Write-Output ('candidates=' + ($script:others -join ' ; '))
if ($script:target -eq [IntPtr]::Zero) { Write-Output 'no-dialog'; exit 3 }
Write-Output ('dialog=' + $script:target)
$script:bestEdit = [IntPtr]::Zero
$script:bestWidth = 0
$script:button = [IntPtr]::Zero
[W]::EnumChildWindows($script:target, { param($h, $l)
  $cls = New-Object System.Text.StringBuilder 256
  [void][W]::GetClassName($h, $cls, 256)
  $c = $cls.ToString()
  if ($c -eq 'Edit') {
    $r = New-Object W+RECT
    [void][W]::GetWindowRect($h, [ref]$r)
    $w = $r.Right - $r.Left
    if ($w -gt $script:bestWidth) { $script:bestWidth = $w; $script:bestEdit = $h }
  }
  if ($c -eq 'Button') {
    $t = New-Object System.Text.StringBuilder 256
    [void][W]::GetWindowText($h, $t, 256)
    if ($t.ToString() -match '保存|Save') { $script:button = $h }
  }
  return $true
}, [IntPtr]::Zero) | Out-Null
Write-Output ('edit=' + $script:bestEdit + ' width=' + $script:bestWidth + ' button=' + $script:button)
if ($script:bestEdit -ne [IntPtr]::Zero) {
  [void][W]::SendMessage($script:bestEdit, 0x000C, [IntPtr]::Zero, $Dest)
  Start-Sleep -Milliseconds 400
}
if ($script:button -ne [IntPtr]::Zero) {
  [void][W]::SendMessage($script:button, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)
  Write-Output 'clicked'
  exit 0
}
[void][W]::SetForegroundWindow($script:target)
Start-Sleep -Milliseconds 500
$fg = [W]::GetForegroundWindow()
Write-Output ('fg=' + $fg)
if ($fg -ne $script:target) { Write-Output 'fg-mismatch'; exit 4 }
[System.Windows.Forms.SendKeys]::SendWait('^a')
[System.Windows.Forms.SendKeys]::SendWait($Dest)
Start-Sleep -Milliseconds 400
[System.Windows.Forms.SendKeys]::SendWait('{ENTER}')
Write-Output 'typed'
exit 0
