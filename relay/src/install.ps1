# aiva installer for Windows — https://github.com/PrajwalShete/aiva
#
#   irm __AIVA_SERVER__/install.ps1 | iex
#
# From cmd.exe:  powershell -ExecutionPolicy Bypass -c "irm __AIVA_SERVER__/install.ps1 | iex"
# Env: AIVA_VERSION (default latest), AIVA_NO_SETUP=1 (install only), AIVA_HOME (default ~\.aiva)
#
# Installs aiva.exe and aivaw.exe (the window-less background launcher) to ~\.aiva\bin, adds that
# folder to your PATH, then runs `aiva setup`. No admin rights needed.

& {
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue' # the progress bar makes downloads very slow in PowerShell 5.1
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $server = '__AIVA_SERVER__'
  $version = if ($env:AIVA_VERSION) { $env:AIVA_VERSION } else { 'latest' }

  # The real CPU, even when this PowerShell runs as x64 under emulation on an ARM PC.
  $cpu = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  try { if ((Get-CimInstance Win32_Processor -ErrorAction Stop | Select-Object -First 1).Architecture -eq 12) { $cpu = 'ARM64' } } catch {}
  $arch = switch ($cpu) {
    'ARM64' { 'arm64' }
    'AMD64' { 'x64' }
    default { throw "aiva install: unsupported CPU $cpu (aiva runs on 64-bit Windows: x64 or ARM64)." }
  }

  $aivaHome = if ($env:AIVA_HOME) { $env:AIVA_HOME } else { Join-Path $env:USERPROFILE '.aiva' }
  $bin = Join-Path $aivaHome 'bin'
  New-Item -ItemType Directory -Force -Path $bin | Out-Null
  $exe = Join-Path $bin 'aiva.exe'

  function Get-Verified([string]$name) {
    $asset = "$name-windows-$arch.exe"
    $url = "$server/dl/$version/$asset"
    $tmp = Join-Path $bin "$name.download.exe"
    for ($try = 1; ; $try++) {
      try {
        Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing -TimeoutSec 300
        break
      } catch {
        if ($try -ge 3) { throw "aiva install: download failed: $url ($($_.Exception.Message))" }
        Start-Sleep -Seconds 1
      }
    }
    $expected = $null
    try { $expected = ((Invoke-RestMethod -Uri "$url.sha256" -UseBasicParsing -TimeoutSec 20) -split '\s+')[0] } catch {}
    if ($expected -and ((Get-FileHash -Algorithm SHA256 -Path $tmp).Hash -ne $expected)) {
      Remove-Item -Force $tmp
      throw 'aiva install: checksum mismatch; the download is corrupt or was tampered with.'
    }
    return $tmp
  }

  # Puts a new .exe in place. Windows can't overwrite a running program but can rename it, so a
  # running one moves aside (aiva deletes it on its next start).
  function Install-Exe([string]$tmp, [string]$dest) {
    if (Test-Path $dest) {
      $old = [IO.Path]::ChangeExtension($dest, 'old.exe')
      Remove-Item -Force $old -ErrorAction SilentlyContinue
      try { Move-Item -Force $dest $old } catch { Remove-Item -Force $tmp; throw "aiva install: couldn't replace $dest ($($_.Exception.Message))." }
    }
    Move-Item -Force $tmp $dest
  }

  Write-Host "Downloading aiva for windows/$arch..."
  $newAiva = Get-Verified 'aiva'
  $newLauncher = Get-Verified 'aivaw'
  $installed = & $newAiva --version 2>$null
  if ($LASTEXITCODE -ne 0 -or -not $installed) {
    Remove-Item -Force $newAiva, $newLauncher -ErrorAction SilentlyContinue
    throw "aiva install: the downloaded program doesn't run on this PC."
  }

  # An earlier install may be running in the background; stop it while files are swapped.
  $hadService = $false
  if (Test-Path $exe) {
    & $exe stop *> $null
    $hadService = $LASTEXITCODE -eq 0
  }
  Install-Exe $newAiva $exe
  Install-Exe $newLauncher (Join-Path $bin 'aivaw.exe')
  Write-Host "Installed $installed at $exe"

  # Add ~\.aiva\bin to the user PATH, keeping other entries exactly as they are (unexpanded).
  $key = Get-Item -Path 'HKCU:\Environment'
  $path = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
  $entries = @($path -split ';' | Where-Object { $_ })
  if (-not ($entries | Where-Object { $_.TrimEnd('\') -ieq $bin.TrimEnd('\') })) {
    Set-ItemProperty -Path 'HKCU:\Environment' -Name 'Path' -Type ExpandString -Value ((@($entries) + $bin) -join ';')
    # Tell Windows the environment changed, so new terminals see it.
    [Environment]::SetEnvironmentVariable('AIVA_INSTALL', '1', 'User')
    [Environment]::SetEnvironmentVariable('AIVA_INSTALL', $null, 'User')
    Write-Host "Added $bin to your PATH (new terminals pick it up)."
  }
  if (-not (($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $bin.TrimEnd('\') })) { $env:Path = "$bin;$env:Path" }

  $env:AIVA_SERVER = $server
  if ($env:AIVA_NO_SETUP -eq '1') { Write-Host 'Next, run:  aiva setup'; return }
  if (Test-Path (Join-Path $aivaHome 'config.json')) {
    if ($hadService) { & $exe start *> $null }
    Write-Host 'Updated. Your existing setup was kept. Check it with: aiva status'
    return
  }
  & $exe setup --server $server
}
