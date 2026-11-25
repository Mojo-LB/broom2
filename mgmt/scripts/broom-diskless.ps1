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
#    writable, under their letters (broom-games.ps1 -LinesFile). In both, a guard keeps the golden's network cards'
#    drivers flagged for network boot while the session runs (task BroomBootCard; a driver installer drops the flag).
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
# Windows Update / browser update / driver search (they would start over after every wipe), the games drives before
# the logon and its pagefile ($pagefile GB, 0 = none, -1 = left alone). Those four are the Diskless page's choice
# ("Windows on the machines"; $w = the server's answer "windows"): one turned off there is left as the golden has it
# and reported as update=on / browserupdate=on / drivers=on / gamesfirst=off. Not $pc = the golden itself
# (installing, its maintenance machine or VM): updates and drivers stay on. Every step on its own: one that fails is
# reported, the others still count. Returns what it did.
function Prep([bool]$pc, [int]$pagefile = -1, $w = $null, [string]$gs = '') {
  $did = @()
  # A setting the server does not name (an older server, nothing chosen yet) is on.
  function Want($n) { ($null -eq $w) -or ($null -eq $w.$n) -or [bool]$w.$n }
  # ...and one of those that are off unless ticked there (Defender is broom-games.ps1's) is off.
  function Opt($n) { ($null -ne $w) -and ($null -ne $w.$n) -and [bool]$w.$n }
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
  # Reading a file does not write its "last access" time: on a disk of 128 GB or less Windows keeps that on, and every
  # file read at a boot then costs a write into the machine's session.
  try { Dw 'HKLM\SYSTEM\CurrentControlSet\Control\FileSystem' NtfsDisableLastAccessUpdate 2147483649; $did += 'lastaccess=off' } catch { $did += 'lastaccess=FAILED' }
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
    if (Want block_update) {
      try {
        Dw "$wu\AU" NoAutoUpdate 1
        Dw $ws AutoDownload 2
        # The update orchestrator too, not only the policy: an update the golden was saved with half installed is taken
        # up again by it at every boot whatever the policy says (seen: 2.7 GB written by its installer in a boot's
        # first minutes, every boot, never finished: the session is dropped at the reset). The Windows Update service
        # itself is only stopped and stays on demand: Windows put it back to manual two seconds after it was disabled
        # here, and had it running again later in the boot (both seen on a lab machine).
        Off UsoSvc
        foreach ($svc in 'wuauserv', 'DoSvc') { Stop-Service -Name $svc -Force -ErrorAction SilentlyContinue }
        $did += 'update=off'
      } catch { $did += 'update=FAILED' }
    } else { $did += 'update=on' }
    # The browsers update themselves (Edge, Chrome and whatever else comes with Google's updater), and that too starts
    # over after every wipe: seen on a lab machine, Edge fetched and installed its next version 22 minutes into a
    # boot, 1.4 GB into a session that is dropped. Their updater services and tasks are off on the machines; the
    # golden keeps them, so the browsers are updated where Windows is: in a maintenance session.
    if (Want block_browser_update) {
      try {
        $n = 0
        $svcs = Get-Service | Where-Object { $_.Name -match '^(edgeupdate|edgeupdatem|gupdate|gupdatem|GoogleUpdater(Internal)?Service.*)$' }
        foreach ($s in $svcs) { Off $s.Name; $n++ }
        $tasks = Get-ScheduledTask | Where-Object { $_.TaskName -match '^(MicrosoftEdgeUpdateTask|GoogleUpdateTask|GoogleUpdaterTask)' }
        foreach ($t in $tasks) { $t | Disable-ScheduledTask -ErrorAction Stop | Out-Null; $n++ }
        $did += $(if ($n) { 'browserupdate=off' } else { 'browserupdate=none' })
      } catch { $did += 'browserupdate=FAILED' }
    } else { $did += 'browserupdate=on' }
    if (Want block_driver_search) {
      try {
        Dw $wu ExcludeWUDriversInQualityUpdate 1
        Dw $ds SearchOrderConfig 0
        Dw $cur SearchOrderConfig 0
        Dw $dm PreventDeviceMetadataFromNetwork 1
        $did += 'drivers=off'
      } catch { $did += 'drivers=FAILED' }
    } else { $did += 'drivers=on' }
    # Off unless ticked on the Diskless page: the firewall, UAC's question, Game Mode.
    if (Opt firewall_off) {
      # Windows Firewall off, all three profiles: the machine takes connections from whatever is on its network.
      try {
        Set-NetFirewallProfile -All -Enabled False -ErrorAction Stop
        if (@(Get-NetFirewallProfile | Where-Object { "$($_.Enabled)" -ne 'False' }).Count) { throw 'a profile is still on' }
        $did += 'firewall=off'
      } catch { step "firewall NOT turned off: $_"; $did += 'firewall=FAILED' }
    }
    if (Opt uac_no_prompt) {
      # UAC stays on (EnableLUA is not touched: without it every program of an administrator runs with full rights
      # and Windows changes how programs start); an administrator is just no longer asked - what requests
      # administrator rights (a game's installer, an anti-cheat's service) gets them at once.
      try {
        $sys = 'HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
        Dw $sys ConsentPromptBehaviorAdmin 0
        Dw $sys PromptOnSecureDesktop 0
        $did += 'uac=noprompt'
      } catch { $did += 'uac=FAILED' }
    }
    if (Opt game_mode) {
      # Game Mode is each user's own setting (HKCU\Software\Microsoft\GameBar, AutoGameModeEnabled): set for every
      # user profile of this Windows, signed in or not, and for the profiles made later (Default).
      try {
        $n = 0
        $pl = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList'
        $users = @(Get-ChildItem $pl | Where-Object { $_.PSChildName -match '^S-1-5-21-' } |
          ForEach-Object { @{ sid = $_.PSChildName; dir = [Environment]::ExpandEnvironmentVariables([string]$_.GetValue('ProfileImagePath')) } })
        $users += @{ sid = ''; dir = "$env:SystemDrive\Users\Default" }
        foreach ($u in $users) {
          $loaded = $u.sid -and (Test-Path "Registry::HKEY_USERS\$($u.sid)")
          $root = if ($loaded) { "HKU\$($u.sid)" } else { 'HKU\BroomGameMode' }
          if (-not $loaded) { & reg.exe load $root "$($u.dir)\NTUSER.DAT" 2>&1 | Out-Null; if ($LASTEXITCODE) { continue } }
          try { Dw "$root\Software\Microsoft\GameBar" AutoGameModeEnabled 1; $n++ }
          finally { if (-not $loaded) { & reg.exe unload $root 2>&1 | Out-Null } }
        }
        if (-not $n) { throw 'no user profile could be opened' }
        $did += 'gamemode=on'
      } catch { step "Game Mode NOT set: $_"; $did += 'gamemode=FAILED' }
    }
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
    # And the update services as Windows sets them, should a golden ever have been saved with them off.
    foreach ($svc in 'UsoSvc', 'wuauserv') {
      if ((Get-Service -Name $svc -ErrorAction SilentlyContinue).StartType -eq 'Disabled') { & sc.exe config $svc start= demand | Out-Null }
    }
    $did += 'update=on'; $did += 'drivers=on'
  }
  # The games drives before the logon. Left to the agent (three PowerShell processes one after the other) they came
  # 50 s after the desktop on a lab machine: a launcher that starts with the desktop finds its library missing. So
  # broom-games.ps1 also gets a task of its own (BroomGames, no trigger), and a machine startup script of the local
  # Group Policy starts that task and waits for it - Windows is told to wait for startup scripts, so the logon, the
  # desktop and what starts with it come after the drives. The task, not the startup script itself: a startup
  # script's process has no SeManageVolumePrivilege and cannot attach a VHDX ("A required privilege is not held by
  # the client"; a task run as SYSTEM can). The task leaves broom-games.done when it ends, however it ends; the
  # startup script waits for that file for 2 minutes at most (the task itself gives up on the server after 30 s).
  # The golden's maintenance VM gets the same ($gs = its copy of broom-games.ps1, kept in the golden with Save): its
  # games disks are there before its desktop too (seen without: G: after the logon and after Steam, whose library was
  # missing at first). A machine's first boot inherits that and makes it its own here.
  $gfile = if ($pc) { "$dir\broom-games.ps1" } else { $gs }
  if ($gfile -and (Want games_first)) {
    try {
      $done = "$env:SystemRoot\Temp\broom-games.done"
      $cmd = "/c powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$gfile`" -Srv $srv -WaitS 30 & echo.> `"$done`""
      $act = New-ScheduledTaskAction -Execute cmd.exe -Argument $cmd
      $set = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 5) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
      Register-ScheduledTask -TaskName BroomGames -Action $act -Settings $set -User SYSTEM -RunLevel Highest -Force | Out-Null
      $gp = "$env:SystemRoot\System32\GroupPolicy"
      $ini = "$gp\Machine\Scripts\scripts.ini"
      $have = if (Test-Path $ini) { [IO.File]::ReadAllText($ini) } else { '' }
      if ($have -notmatch 'BroomGames') {
        # Startup scripts someone set up on the golden: not rewritten here (and the drives keep coming the old way).
        if ($have -match '(?m)^\s*\d+CmdLine=') { throw 'this Windows has startup scripts of its own' }
        New-Item -ItemType Directory -Force -Path "$gp\Machine\Scripts\Startup", "$gp\Machine\Scripts\Shutdown" | Out-Null
        if (Test-Path $ini) { Remove-Item -LiteralPath $ini -Force }
        $wait = "/c del `"$done`" 2>nul & schtasks /run /tn BroomGames >nul 2>&1 && for /l %i in (1,1,120) do @if not exist `"$done`" ping -n 2 127.0.0.1 >nul"
        [IO.File]::WriteAllText($ini, "`r`n[Startup]`r`n0CmdLine=cmd.exe`r`n0Parameters=$wait`r`n", [Text.Encoding]::Unicode)
        (Get-Item -LiteralPath $ini -Force).Attributes = 'Hidden'
        # gpt.ini: the Scripts extension among the machine's, and a higher version, or Windows does not read the file.
        $g = "$gp\gpt.ini"
        $t = if (Test-Path $g) { [IO.File]::ReadAllText($g) } else { "[General]`r`n" }
        $ext = @('[{42B5FAAE-6536-11D2-AE5A-0000F87571E3}{40B6664F-4972-11D1-A7CA-0000F87571E3}]')
        if ($t -match '(?m)^gPCMachineExtensionNames=(.*)$') { $ext += [regex]::Matches($Matches[1], '\[[^\]]+\]') | ForEach-Object { $_.Value } }
        $line = 'gPCMachineExtensionNames=' + (($ext | Sort-Object -Unique) -join '')
        if ($t -match '(?m)^gPCMachineExtensionNames=') { $t = [regex]::Replace($t, '(?m)^gPCMachineExtensionNames=.*$', { $line }) } else { $t = $t.TrimEnd() + "`r`n$line`r`n" }
        $v = 0; if ($t -match '(?m)^Version=(\d+)') { $v = [int]$Matches[1] }
        $v = ($v -band 0x7FFF0000) + ((($v -band 0xFFFF) + 1) -band 0xFFFF)
        if ($t -match '(?m)^Version=\d+') { $t = [regex]::Replace($t, '(?m)^Version=\d+', "Version=$v") } else { $t = $t.TrimEnd() + "`r`nVersion=$v`r`n" }
        [IO.File]::WriteAllText($g, $t, [Text.Encoding]::ASCII)
      }
      Dw 'HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' RunStartupScriptSync 1
      Dw 'HKLM\SOFTWARE\Policies\Microsoft\Windows NT\CurrentVersion\Winlogon' SyncForegroundPolicy 1
      & gpupdate.exe /target:computer /force | Out-Null
      # What Windows took over from scripts.ini (read with .NET: Get-ItemProperty fails on this key, "Specified cast is
      # not valid").
      $reg = [Microsoft.Win32.Registry]::GetValue('HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Group Policy\Scripts\Startup\0\0', 'Parameters', '')
      if ([string]$reg -notmatch 'BroomGames') { throw 'the startup script did not register' }
      $did += 'gamesfirst=on'
    } catch { step "games drives before the logon NOT set up: $_"; $did += 'gamesfirst=FAILED' }
  } elseif ($gfile) {
    # Unticked: Broom's own startup script and task go, where a golden brought them (its maintenance VM sets them up).
    try {
      $gp = "$env:SystemRoot\System32\GroupPolicy"; $ini = "$gp\Machine\Scripts\scripts.ini"
      if ((Test-Path $ini) -and [IO.File]::ReadAllText($ini) -match 'BroomGames') {
        Remove-Item -LiteralPath $ini -Force
        $g = "$gp\gpt.ini"
        if (Test-Path $g) {
          $t = [IO.File]::ReadAllText($g); $v = 0; if ($t -match '(?m)^Version=(\d+)') { $v = [int]$Matches[1] }
          $v = ($v -band 0x7FFF0000) + ((($v -band 0xFFFF) + 1) -band 0xFFFF)
          [IO.File]::WriteAllText($g, [regex]::Replace($t, '(?m)^Version=\d+', "Version=$v"), [Text.Encoding]::ASCII)
        }
        & gpupdate.exe /target:computer /force | Out-Null
      }
      Unregister-ScheduledTask -TaskName BroomGames -Confirm:$false -ErrorAction SilentlyContinue
      $did += 'gamesfirst=off'
    } catch { step "games drives before the logon NOT taken out: $_"; $did += 'gamesfirst=FAILED' }
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
  # The maintenance VM on the server with games disks: its copy of broom-games.ps1 first (Prep sets up the games
  # drives before the logon with it, for the VM's next start).
  $gs = ''
  if ($info.vm -and $info.games) {
    try { $gs = "$env:SystemRoot\Temp\broom-games-vm.ps1"; Fetch 'broom-games.ps1' $gs } catch { step "broom-games.ps1 NOT fetched: $_"; $gs = '' }
  }
  step ("golden prepared: " + (Prep $false -1 $info.windows $gs))
  # Its writes go into the golden: no machine's key or agent files may end up there. .NET's Directory.Delete does not
  # follow a folder mount point (it only removes it), so a volume mounted inside is never emptied with it.
  if (Test-Path $dir) {
    try { [IO.Directory]::Delete($dir, $true); step "removed $dir from the golden" } catch { step "could not remove ${dir}: $_" }
  }
  # The maintenance VM on the server: its games disks (found by their disk serial), writable, under their letters.
  if ($gs) {
    try {
      $lf = "$env:SystemRoot\Temp\broom-games-vm.txt"
      [IO.File]::WriteAllText($lf, [string]$info.games, [Text.Encoding]::ASCII)
      # With them: what the Diskless page chose for Defender and the games drives (the game downloads run here).
      $ga = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $gs, '-Srv', $srv, '-LinesFile', $lf)
      if ($null -ne $info.windows -and $null -ne $info.windows.defender_skip_games) {
        $ga += '-DefenderSkip', $(if ($info.windows.defender_skip_games) { '1' } else { '0' })
      }
      & powershell.exe @ga
      step ("VM: games disks {0} (log: broom-games.log)" -f (($info.games -split "`n" | Where-Object { $_ } | ForEach-Object { ($_ -split ' ')[0] }) -join ', '))
    } catch { step "VM games disks FAILED: $_" }
  }
  # Network-boot driver guard. A network card's driver starts Windows from the network only while its service is
  # flagged for it (BootFlags bit 1; Add hardware sets it). A vendor's driver installer run in this session registers
  # the service anew without the flag: saved like that, every PC with that card stops at INACCESSIBLE_BOOT_DEVICE.
  # So while this session runs, a task (BroomBootCard, SYSTEM; started here only, never on a diskless machine) looks
  # once a second at the cards the server knows for this golden and at this PC's own, sets the flag again where it
  # went, and tells the server, which shows it next to Save. One card a line for it: device instance, driver
  # service, 1 = always flagged (the server's list) / 0 = this PC's own, guarded once it was seen flagged.
  try {
    $cards = @()
    foreach ($c in @($info.boot_cards)) { if ($c -and $c.instance) { $cards += ("{0}`t{1}`t1" -f $c.instance, $c.service) } }
    if (-not $info.vm) {
      Get-CimInstance Win32_NetworkAdapter -Filter 'PhysicalAdapter=True' -ErrorAction SilentlyContinue |
        Where-Object { $_.PNPDeviceID -like 'PCI\*' } | ForEach-Object {
          $id = [string]$_.PNPDeviceID
          if (-not ($cards | Where-Object { ($_ -split "`t")[0] -ieq $id })) { $cards += ("{0}`t{1}`t0" -f $id, $_.ServiceName) }
        }
    }
    $bl = "$env:SystemRoot\Temp\broom-bootcard.txt"; $bp = "$env:SystemRoot\Temp\broom-bootcard.ps1"
    [IO.File]::WriteAllLines($bl, [string[]]$cards, [Text.Encoding]::ASCII)
    [IO.File]::WriteAllText($bp, @'
param([string]$Srv, [string]$List)
$reg = [Microsoft.Win32.Registry]
$log = "$env:SystemRoot\Temp\broom-bootcard.log"
function Say($m) { try { [IO.File]::AppendAllText($log, ('{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $m) + "`r`n") } catch { } }
$cards = @(Get-Content -LiteralPath $List -ErrorAction SilentlyContinue | Where-Object { $_ } | ForEach-Object {
  $p = $_ -split "`t"; [pscustomobject]@{ i = $p[0]; s = $p[1]; must = ($p[2] -eq '1') } })
Say ("guard started: {0} card(s)" -f $cards.Count)
$failed = @{}
# One look at every card: the services flagged, those flagged again just now, those gone or not to be flagged.
function Look {
  $ok = @(); $fixed = @(); $gone = @()
  foreach ($c in $cards) {
    # The service the card is bound to now (a new driver may come with another one), else the one it was added with.
    $s = [string]$reg::GetValue("HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Enum\$($c.i)", 'Service', '')
    if (-not $s) { $s = $c.s }
    if (-not $s) { continue }
    $k = "HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\$s"
    if ($null -eq $reg::GetValue($k, 'Type', $null)) { if ($c.must) { $gone += $s }; continue }
    $bf = [int]$reg::GetValue($k, 'BootFlags', 0)
    if ($bf -band 1) { $ok += $s; $c.must = $true; continue }
    if (-not $c.must) { continue }
    try {
      $reg::SetValue($k, 'BootFlags', [int]($bf -bor 1), [Microsoft.Win32.RegistryValueKind]::DWord)
      $fixed += $s; $failed.Remove($s); Say "network-boot flag of $s set again (BootFlags $bf -> $($bf -bor 1))"
    } catch { $gone += $s; if (-not $failed[$s]) { $failed[$s] = $true; Say "network-boot flag of $s NOT set: $_" } }
  }
  [pscustomobject]@{ ok = [string[]]@($ok + $fixed | Sort-Object -Unique); fixed = [string[]]@($fixed | Sort-Object -Unique); gone = [string[]]@($gone | Sort-Object -Unique) }
}
$first = $true; $last = $null; $wait = 0
while ($true) {
  $seen = Look
  $now = ($seen.ok -join ',') + '|' + ($seen.gone -join ',')
  if ($wait -gt 0) { $wait-- }
  if (($first -or $seen.fixed.Count -or $now -ne $last) -and $wait -eq 0) {
    $body = @{ start = $first; ok = $seen.ok; restored = $seen.fixed; missing = $seen.gone } | ConvertTo-Json -Compress
    try {
      Invoke-RestMethod -UseBasicParsing -TimeoutSec 5 -Method Post -ContentType 'application/json' -Uri "http://$Srv/api/diskless/client/boot-cards" -Body $body | Out-Null
      $first = $false; $last = $now
    } catch { Say "report not sent: $_"; $wait = 15 }
  }
  Start-Sleep -Seconds 1
}
'@, [Text.Encoding]::ASCII)
    $act = New-ScheduledTaskAction -Execute powershell.exe -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$bp`" -Srv $srv -List `"$bl`""
    $set = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName BroomBootCard -Action $act -Settings $set -User SYSTEM -RunLevel Highest -Force | Out-Null
    Start-ScheduledTask -TaskName BroomBootCard
    step ("network-boot driver guard started: {0} card(s) (log: broom-bootcard.log)" -f $cards.Count)
  } catch { step "network-boot driver guard NOT started: $_" }
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
  $prep = Prep $true $pfg $info.windows
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
