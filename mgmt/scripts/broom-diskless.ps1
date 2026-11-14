# broom-diskless.ps1 - diskless goldens (Windows): each machine's own first-boot setup + the Remote agent.
# Install it ON THE GOLDEN, from its maintenance machine (PowerShell as Administrator), before Save:
#   irm http://<server>/broom-diskless.ps1 | iex
# It copies itself to C:\Windows\Setup\Scripts\broom-diskless.ps1 and registers a SYSTEM startup task. At every start
# it runs the server's current version of this script (fetched again; the copy on the golden is only the way in, so
# a newer Broom reaches every machine without a new golden version), which asks the server what this machine is
# (GET /api/diskless/client, answered by its IP + MAC):
#  - a diskless machine on its first boot of this golden version: takes its own name from the server (Devices page),
#    gets its Remote agent key (agentauth.rs) and the agent's files into C:\ProgramData\Broom, tells the server the
#    setup is done (POST /api/diskless/client/base-done) and restarts at once - that boot is kept as the machine's base
#    layer; every later boot starts from it, everything else is wiped at each boot.
#  - a diskless machine on a later boot: starts the Remote agent and mounts its games disks (broom-watch.ps1 -Dir, its
#    own task; broom-games.ps1 at start and every 5 minutes).
#  - the maintenance machine: nothing - what it writes goes into the golden every machine shares (no key, no agent).
#    The maintenance VM on the server (Diskless page, Maintain in the browser: vmmaint.rs) also gets its games disks,
#    writable, under their letters (broom-games.ps1 -LinesFile).
#  - anything else (the server unreachable): nothing.
# Windows is also set for a disk that is on the network and starts fresh at every boot (Prep below): always no
# automatic BitLocker, hibernation, System Restore, defrag, search index, SysMain or crash dump; on the machines (in
# their base) also no Windows Update, no driver search and the pagefile the server asks for. On the golden's
# maintenance machine or VM Windows Update and drivers stay on: that is where they are installed.
# ASCII only. Logs to C:\Windows\Temp\broom-diskless.log (in the session layer: gone at the next boot).
$ErrorActionPreference = 'Stop'
$srv = '__SERVER__'
$dst = "$env:SystemRoot\Setup\Scripts\broom-diskless.ps1"
$run = "$env:SystemRoot\Temp\broom-diskless.run.ps1"
$log = "$env:SystemRoot\Temp\broom-diskless.log"
$dir = "$env:ProgramData\Broom"
function step($m) {
  $l = '{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $m
  Write-Host "[broom] $m" -ForegroundColor Cyan
  try { [IO.File]::AppendAllText($log, $l + "`r`n") } catch { }
}
$me = $MyInvocation.MyCommand.Path

# Windows settings for a disk on the network that is wiped at every boot. $pc = a machine building its base: also no
# Windows Update / driver search (they would start over after every wipe) and its pagefile ($pagefile GB, 0 = none,
# -1 = left alone). Not $pc = the golden itself (installing, its maintenance machine or VM): updates and drivers
# stay on. Every step on its own: one that fails is reported, the others still count. Returns what it did.
function Prep([bool]$pc, [int]$pagefile = -1) {
  $did = @()
  function Dw($k, $n, $v) { & reg.exe add $k /v $n /t REG_DWORD /d $v /f | Out-Null; if ($LASTEXITCODE) { throw "reg add $k $n" } }
  function Off($svc) { Set-Service -Name $svc -StartupType Disabled -ErrorAction Stop; Stop-Service -Name $svc -Force -ErrorAction SilentlyContinue }
  # No automatic device encryption (Windows 11 turns it on by itself on a PC with a TPM): the disk is the server's.
  try {
    Dw 'HKLM\SYSTEM\CurrentControlSet\Control\BitLocker' PreventDeviceEncryption 1
    $bl = 'prevented'
    try {
      $v = Get-BitLockerVolume -MountPoint $env:SystemDrive -ErrorAction Stop
      if ($v.VolumeStatus -ne 'FullyDecrypted') { & manage-bde.exe -off $env:SystemDrive | Out-Null; $bl = "was $($v.VolumeStatus): decrypting" }
    } catch { }
    $did += "bitlocker=$bl"
  } catch { $did += 'bitlocker=FAILED' }
  try { & powercfg.exe /hibernate off | Out-Null; $did += 'hibernate=off' } catch { $did += 'hibernate=FAILED' }
  try { Disable-ComputerRestore -Drive "$env:SystemDrive\" -ErrorAction Stop; $did += 'restore=off' } catch { $did += 'restore=FAILED' }
  try { Disable-ScheduledTask -TaskPath '\Microsoft\Windows\Defrag\' -TaskName ScheduledDefrag -ErrorAction Stop | Out-Null; $did += 'defrag=off' } catch { $did += 'defrag=FAILED' }
  try { Off WSearch; $did += 'search=off' } catch { $did += 'search=FAILED' }
  try { Off SysMain; $did += 'sysmain=off' } catch { $did += 'sysmain=FAILED' }
  # A crash dump would be written over the network into a session that is wiped anyway.
  try { Dw 'HKLM\SYSTEM\CurrentControlSet\Control\CrashControl' CrashDumpEnabled 0; $did += 'dump=off' } catch { $did += 'dump=FAILED' }
  # The server shows a machine without the agent as on when it answers a ping.
  try {
    & netsh.exe advfirewall firewall delete rule name="Broom ping" | Out-Null
    & netsh.exe advfirewall firewall add rule name="Broom ping" protocol=icmpv4:8,any dir=in action=allow profile=any | Out-Null
    $did += 'ping=allowed'
  } catch { $did += 'ping=FAILED' }
  $wu = 'HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate'
  $ds = 'HKLM\SOFTWARE\Policies\Microsoft\Windows\DriverSearching'
  $dm = 'HKLM\SOFTWARE\Policies\Microsoft\Windows\Device Metadata'
  $ws = 'HKLM\SOFTWARE\Policies\Microsoft\WindowsStore'
  $cur = 'HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\DriverSearching'
  if ($pc) {
    try {
      Dw "$wu\AU" NoAutoUpdate 1
      Dw $ws AutoDownload 2
      foreach ($svc in 'UsoSvc', 'wuauserv', 'DoSvc') { Stop-Service -Name $svc -Force -ErrorAction SilentlyContinue }
      $did += 'update=off'
    } catch { $did += 'update=FAILED' }
    try {
      Dw $wu ExcludeWUDriversInQualityUpdate 1
      Dw $ds SearchOrderConfig 0
      Dw $cur SearchOrderConfig 0
      Dw $dm PreventDeviceMetadataFromNetwork 1
      $did += 'drivers=off'
    } catch { $did += 'drivers=FAILED' }
    if ($pagefile -ge 0) {
      try {
        # Not Windows' own choice (as large as it likes, on a disk that is the server's): none, or one fixed size.
        $cs = Get-CimInstance Win32_ComputerSystem
        if ($cs.AutomaticManagedPagefile) { Set-CimInstance $cs -Property @{AutomaticManagedPagefile = $false} }
        Get-CimInstance Win32_PageFileSetting | Remove-CimInstance
        if ($pagefile -eq 0) { $did += 'pagefile=none' } else {
          $mb = $pagefile * 1024
          $mm = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management'
          Set-ItemProperty -Path $mm -Name PagingFiles -Value @("$env:SystemDrive\pagefile.sys $mb $mb") -Type MultiString -ErrorAction Stop
          $did += "pagefile=${pagefile}GB"
        }
      } catch { $did += 'pagefile=FAILED' }
    }
  } else {
    # The golden: whatever kept updates and drivers off (an older copy of this script, a hand) goes.
    foreach ($x in @(@("$wu\AU", 'NoAutoUpdate'), @($ws, 'AutoDownload'), @($wu, 'ExcludeWUDriversInQualityUpdate'), @($ds, 'SearchOrderConfig'), @($dm, 'PreventDeviceMetadataFromNetwork'))) {
      Remove-ItemProperty -Path ($x[0] -replace '^HKLM', 'HKLM:') -Name $x[1] -ErrorAction SilentlyContinue
    }
    try { Dw $cur SearchOrderConfig 1 } catch { }
    $did += 'update=on'; $did += 'drivers=on'
  }
  $did -join ' '
}

# 1. Installing (run by hand on the golden): copy + startup task, then stop.
if ($me -ne $dst -and $me -ne $run) {
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
  # A diskless machine's disk is on the network: no hibernation / fast startup (resume from a stale session layer),
  # and the rest of Prep.
  Write-Host ("Windows prepared: " + (Prep $false)) -ForegroundColor Green
  # No machine's agent files or key in the golden (an older version of this script on a maintenance boot, a copy).
  if (Test-Path $dir) { Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue }
  Write-Host "Installed: task BroomDiskless runs $dst at every start (server $srv)." -ForegroundColor Green
  Write-Host 'Shut down this machine, then Save the golden on the Diskless page.' -ForegroundColor Green
  return
}

# 2. At start (SYSTEM): the network may come a little after the task starts.
function Text($url) {
  $r = Invoke-WebRequest -UseBasicParsing -TimeoutSec 10 $url
  if ($r.Content -is [byte[]]) { [Text.Encoding]::ASCII.GetString($r.Content) } else { [string]$r.Content }
}
$info = $null
for ($i = 0; $i -lt 30 -and -not $info; $i++) {
  try { $info = Invoke-RestMethod -UseBasicParsing -TimeoutSec 5 "http://$srv/api/diskless/client" } catch { Start-Sleep -Seconds 2 }
}
if (-not $info) { step "server $srv not reachable - nothing to do"; exit }

# The server's current version of this script, when it differs from the golden's copy: that one runs instead.
# Same trust as the boot itself: this machine's whole disk comes from the same server.
if ($me -eq $dst) {
  try {
    $cur = Text "http://$srv/broom-diskless.ps1"
    if ($cur.Length -gt 100 -and $cur -ne [IO.File]::ReadAllText($dst)) {
      [IO.File]::WriteAllText($run, $cur, [Text.Encoding]::ASCII)
      & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $run
      exit
    }
  } catch { step "server's version of this script not fetched ($_) - running the golden's copy" }
}

step ("role {0}, golden {1} g{2}, first boot: {3}, name: {4}" -f $info.role, $info.golden, $info.ver, $info.first, $info.host)
# The LAN's router: when the server is on this machine's subnet, its boot gave Windows no gateway (Windows would send
# its whole disk through the router otherwise), so the default route is added now that the disk is connected - on the
# network card that reaches the server, for this boot only (not kept in the base or the golden).
if ($info.gateway) {
  try {
    if (-not (Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue)) {
      $if = (Find-NetRoute -RemoteIPAddress $srv -ErrorAction Stop | Select-Object -First 1).InterfaceIndex
      New-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -InterfaceIndex $if -NextHop $info.gateway -PolicyStore ActiveStore -ErrorAction Stop | Out-Null
      step "default route via $($info.gateway) added (interface $if)"
    }
  } catch { step "default route via $($info.gateway) NOT added: $_" }
}
# The server's scripts.sha256: a downloaded script runs only if it matches.
function Fetch($f, $to) {
  $sum = $null
  foreach ($l in ((Text "http://$srv/tftp/broom-scripts/scripts.sha256") -split "`n")) {
    $p = $l.Trim() -split '\s+'
    if ($p.Count -eq 2 -and $p[1] -eq $f) { $sum = $p[0] }
  }
  if (-not $sum) { throw "$f is not in the server's scripts.sha256" }
  Invoke-WebRequest -UseBasicParsing -TimeoutSec 60 -OutFile $to "http://$srv/tftp/broom-scripts/$f"
  if ((Get-FileHash -Algorithm SHA256 $to).Hash.ToLower() -ne $sum) { Remove-Item -LiteralPath $to -Force; throw "$f does not match scripts.sha256" }
}
if ($info.role -eq 'maintenance') {
  step ("golden prepared: " + (Prep $false))
  # Its writes go into the golden: no machine's key or agent files may end up there. .NET's Directory.Delete does not
  # follow a folder mount point (it only removes it), so a volume mounted inside is never emptied with it.
  if (Test-Path $dir) {
    try { [IO.Directory]::Delete($dir, $true); step "removed $dir from the golden" } catch { step "could not remove ${dir}: $_" }
  }
  # The maintenance VM on the server: its games disks (found by their disk serial), writable, under their letters.
  if ($info.vm -and $info.games) {
    try {
      $gs = "$env:SystemRoot\Temp\broom-games-vm.ps1"; $lf = "$env:SystemRoot\Temp\broom-games-vm.txt"
      Fetch 'broom-games.ps1' $gs
      [IO.File]::WriteAllText($lf, [string]$info.games, [Text.Encoding]::ASCII)
      & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $gs -Srv $srv -LinesFile $lf
      step ("VM: games disks {0} (log: broom-games.log)" -f (($info.games -split "`n" | Where-Object { $_ } | ForEach-Object { ($_ -split ' ')[0] }) -join ', '))
    } catch { step "VM games disks FAILED: $_" }
  }
  exit
}
if ($info.role -ne 'diskless') { exit }

# The agent's files (broom-watch.ps1, broom-games.ps1, broom-vnc.cs or the release's broom-vnc.exe) from the server's
# script folder,
# each checked against its scripts.sha256; unchanged ones stay. The folder: SYSTEM + Administrators only (the key).
function AgentFiles {
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  & icacls $dir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
  $sums = @{}
  foreach ($l in ((Text "http://$srv/tftp/broom-scripts/scripts.sha256") -split "`n")) {
    $p = $l.Trim() -split '\s+'
    if ($p.Count -eq 2 -and $p[0] -match '^[0-9a-f]{64}$') { $sums[$p[1]] = $p[0] }
  }
  foreach ($f in 'broom-watch.ps1', 'broom-games.ps1', 'broom-vnc.cs', 'broom-vnc.exe') {
    $p = "$dir\$f"
    if (-not $sums.ContainsKey($f)) {
      if (Test-Path $p) { Remove-Item -LiteralPath $p -Force }   # not in this release (a local build has no exe)
      continue
    }
    if ((Test-Path $p) -and (Get-FileHash -Algorithm SHA256 $p).Hash.ToLower() -eq $sums[$f]) { continue }
    Invoke-WebRequest -UseBasicParsing -TimeoutSec 60 -OutFile $p "http://$srv/tftp/broom-scripts/$f"
    if ((Get-FileHash -Algorithm SHA256 $p).Hash.ToLower() -ne $sums[$f]) {
      Remove-Item -LiteralPath $p -Force
      throw "$f does not match the server's scripts.sha256"
    }
  }
}
function WatchArgs { "-NoProfile -ExecutionPolicy Bypass -File `"$dir\broom-watch.ps1`" -Srv $srv -Dir `"$dir`"" }

if ($info.first) {
  # First boot of this machine on this golden version: its own setup, then the restart that keeps it as its base.
  $pfg = -1
  if ($null -ne $info.pagefile_gb) { $pfg = [int]$info.pagefile_gb }
  $prep = Prep $true $pfg
  step "prepared: $prep"
  if ($info.host -and ($info.host -ne $env:COMPUTERNAME)) {
    step "machine name $env:COMPUTERNAME -> $($info.host)"
    try { Rename-Computer -NewName $info.host -Force -ErrorAction Stop } catch { step "rename FAILED: $_" }
  }
  # The Remote agent: its files, this machine's key (handed out only on this boot), and its compiled copy, all kept in
  # the base. A failure here only leaves the machine without the agent.
  try {
    AgentFiles
    $k = (Text "http://$srv/api/diskless/client/agent-key").Trim()
    if ($k -notmatch '^[0-9a-f]{64}$') { throw 'no key from the server' }
    [IO.File]::WriteAllText("$dir\agent.key", $k, [Text.Encoding]::ASCII)
    $p = Start-Process powershell.exe -ArgumentList ((WatchArgs) + ' -Prepare') -WindowStyle Hidden -Wait -PassThru
    step 'agent files, key and build ready'
  } catch { step "agent setup FAILED: $_ (the machine works without it; set up again with its next base)" }
  try {
    Invoke-RestMethod -UseBasicParsing -TimeoutSec 10 -Method Post -ContentType 'text/plain' -Body $prep "http://$srv/api/diskless/client/base-done" | Out-Null
    step 'base done - restarting to keep it'
  } catch { step "base-done FAILED: $_ (set up again at the next boot)"; exit }
  Restart-Computer -Force
  exit
}

# A later boot: the agent, through broom-watch.ps1 (its own task: it loops; this task has a time limit).
try { AgentFiles } catch { step "agent files not updated: $_" }
if (-not (Test-Path "$dir\agent.key")) { step 'no agent key in this base - the agent starts after the next base'; exit }
if (-not (Test-Path "$dir\broom-watch.ps1")) { step 'no broom-watch.ps1 - no agent'; exit }
try {
  $act = New-ScheduledTaskAction -Execute powershell.exe -Argument (WatchArgs)
  $set = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
  Register-ScheduledTask -TaskName BroomWatch -Action $act -Settings $set -User SYSTEM -RunLevel Highest -Force | Out-Null
  Start-ScheduledTask -TaskName BroomWatch
  step 'agent started (task BroomWatch)'
} catch { step "agent task FAILED: $_" }
