# GUI spike measurement harness (Windows PowerShell 5.1).
# Usage: powershell -ExecutionPolicy Bypass -File spikes\measure.ps1 -OutDir <dir>
param([Parameter(Mandatory = $true)][string]$OutDir, [int]$Runs = 3)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Drawing
Add-Type -Namespace Spike -Name Win -MemberDefinition @'
[System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
public struct RECT { public int Left, Top, Right, Bottom; }
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr h);
[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
'@
[Spike.Win]::SetProcessDPIAware() | Out-Null

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$slintExe = Join-Path $root 'gui-slint\target\release\gui-spike-slint.exe'
$eguiExe = Join-Path $root 'gui-egui\target\release\gui-spike-egui.exe'
# name -> (executable, SLINT_BACKEND value or '')
$apps = [ordered]@{
    'slint-gl' = @($slintExe, '')
    'slint-sw' = @($slintExe, 'winit-software')
    'egui'     = @($eguiExe, '')
}
$cores = [Environment]::ProcessorCount
$results = @()

foreach ($run in 1..$Runs) {
foreach ($name in $apps.Keys) {
    foreach ($mode in 'active', 'idle') {
        $stdout = Join-Path $OutDir "$name-$mode-$run.out.txt"
        if ($apps[$name][1]) { $env:SLINT_BACKEND = $apps[$name][1] } else { Remove-Item Env:SLINT_BACKEND -ErrorAction SilentlyContinue }
        $p = Start-Process -FilePath $apps[$name][0] -ArgumentList '--mode', $mode, '--seconds', '34' -PassThru -RedirectStandardOutput $stdout
        Start-Sleep -Seconds 4
        $p.Refresh()
        $hwnd = $p.MainWindowHandle
        $uia = ''
        if ($run -eq 1 -and $mode -eq 'active' -and $hwnd -ne [IntPtr]::Zero) {
            [Spike.Win]::SetForegroundWindow($hwnd) | Out-Null
            Start-Sleep -Milliseconds 700
            $r = New-Object Spike.Win+RECT
            [Spike.Win]::GetWindowRect($hwnd, [ref]$r) | Out-Null
            $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
            $bmp = New-Object System.Drawing.Bitmap $w, $h
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            $g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
            $bmp.Save((Join-Path $OutDir "$name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
            $g.Dispose(); $bmp.Dispose()
            try {
                $el = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
                $all = $el.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
                $byType = @{}
                $samples = @()
                foreach ($e in $all) {
                    $t = $e.Current.ControlType.ProgrammaticName -replace 'ControlType\.', ''
                    $byType[$t] = 1 + [int]$byType[$t]
                    if ($samples.Count -lt 6 -and $e.Current.Name -and $t -in 'Slider', 'CheckBox', 'Button') {
                        $v = ''
                        try { $v = $e.GetCurrentPropertyValue([System.Windows.Automation.RangeValuePattern]::ValueProperty) } catch {}
                        $samples += "$t '$($e.Current.Name)' value=$v"
                    }
                }
                $uia = (($byType.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ', ') + ' | ' + ($samples -join '; ')
            } catch { $uia = "UIA error: $($_.Exception.Message)" }
        }
        Start-Sleep -Seconds (10 - 4 - 1)
        $p.Refresh(); $cpu0 = $p.TotalProcessorTime; $t0 = Get-Date
        Start-Sleep -Seconds 20
        $p.Refresh(); $cpu1 = $p.TotalProcessorTime; $t1 = Get-Date
        $priv = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
        $ws = [math]::Round($p.WorkingSet64 / 1MB, 1)
        $p.WaitForExit(15000) | Out-Null
        if (-not $p.HasExited) { $p.Kill() }
        $cpuPct = [math]::Round(100 * ($cpu1 - $cpu0).TotalSeconds / ($t1 - $t0).TotalSeconds, 2)
        $first = (Get-Content $stdout -ErrorAction SilentlyContinue | Select-String 'first_frame' | Select-Object -First 1).Line
        $results += [pscustomobject]@{
            run = $run; app = $name; mode = $mode; cpu_pct_of_one_core = $cpuPct; private_mb = $priv; working_set_mb = $ws
            first_frame = $first; uia = $uia
        }
    }
}
}
Remove-Item Env:SLINT_BACKEND -ErrorAction SilentlyContinue
$results | Format-Table run, app, mode, cpu_pct_of_one_core, private_mb, working_set_mb, first_frame -AutoSize | Out-String -Width 400 | Tee-Object (Join-Path $OutDir 'results.txt')
$results | Where-Object uia | ForEach-Object { "$($_.app) UIA: $($_.uia)" }
"binary sizes (MB): slint=" + [math]::Round((Get-Item $slintExe).Length / 1MB, 1) + ", egui=" + [math]::Round((Get-Item $eguiExe).Length / 1MB, 1)
"logical cores: $cores"
