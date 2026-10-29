# broom-diskless.ps1 - diskless goldens (Windows): each machine's own first-boot setup.
# Install it ON THE GOLDEN, from its maintenance machine (PowerShell as Administrator), before Save:
#   irm http://<server>/broom-diskless.ps1 | iex
# It copies itself to C:\Windows\Setup\Scripts\broom-diskless.ps1 and registers a SYSTEM startup task. At every start
# it asks the server what this machine is (GET /api/diskless/client, answered by its IP + MAC):
#  - a diskless machine on its first boot of this golden version: takes its own name from the server (Devices page),
#    tells the server the setup is done (POST /api/diskless/client/base-done) and restarts at once - that boot is
#    kept as the machine's base layer; every later boot starts from it, everything else is wiped at each boot.
#  - anything else (later boots, the maintenance machine, the server unreachable): does nothing.
# ASCII only. Logs to C:\Windows\Temp\broom-diskless.log (in the session layer: gone at the next boot).
$ErrorActionPreference = 'Stop'
$srv = '__SERVER__'
$dst = "$env:SystemRoot\Setup\Scripts\broom-diskless.ps1"
$log = "$env:SystemRoot\Temp\broom-diskless.log"
function step($m) {
  $l = '{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $m
  Write-Host "[broom] $m" -ForegroundColor Cyan
  try { [IO.File]::AppendAllText($log, $l + "`r`n") } catch { }
}

# 1. Installing (run by hand on the golden): copy + startup task, then stop.
if ($MyInvocation.MyCommand.Path -ne $dst) {
  $id = [Security.Principal.WindowsIdentity]::GetCurrent()
  if (-not (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Run PowerShell as Administrator.' -ForegroundColor Red; return
  }
  New-Item -ItemType Directory -Force -Path (Split-Path $dst) | Out-Null
  $self = (Invoke-WebRequest -UseBasicParsing "http://$srv/broom-diskless.ps1").Content
  [IO.File]::WriteAllText($dst, $self, [Text.Encoding]::ASCII)
  $a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File $dst"
  $t = New-ScheduledTaskTrigger -AtStartup
  $s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 10) -StartWhenAvailable
  Register-ScheduledTask -TaskName BroomDiskless -Action $a -Trigger $t -Settings $s -User SYSTEM -RunLevel Highest -Force | Out-Null
  # A diskless machine's disk is on the network: no hibernation / fast startup (resume from a stale session layer).
  powercfg /hibernate off | Out-Null
  Write-Host "Installed: task BroomDiskless runs $dst at every start (server $srv)." -ForegroundColor Green
  Write-Host 'Shut down this machine, then Save the golden on the Diskless page.' -ForegroundColor Green
  return
}

# 2. At start (SYSTEM): the network may come a little after the task starts.
$info = $null
for ($i = 0; $i -lt 30 -and -not $info; $i++) {
  try { $info = Invoke-RestMethod -UseBasicParsing -TimeoutSec 5 "http://$srv/api/diskless/client" } catch { Start-Sleep -Seconds 2 }
}
if (-not $info) { step "server $srv not reachable - nothing to do"; exit }
step ("role {0}, golden {1} g{2}, first boot: {3}, name: {4}" -f $info.role, $info.golden, $info.ver, $info.first, $info.host)
if ($info.role -ne 'diskless' -or -not $info.first) { exit }

# First boot of this machine on this golden version: its own setup, then the restart that keeps it as its base.
if ($info.host -and ($info.host -ne $env:COMPUTERNAME)) {
  step "machine name $env:COMPUTERNAME -> $($info.host)"
  try { Rename-Computer -NewName $info.host -Force -ErrorAction Stop } catch { step "rename FAILED: $_" }
}
try {
  Invoke-RestMethod -UseBasicParsing -TimeoutSec 10 -Method Post "http://$srv/api/diskless/client/base-done" | Out-Null
  step 'base done - restarting to keep it'
} catch { step "base-done FAILED: $_ (set up again at the next boot)"; exit }
Restart-Computer -Force
