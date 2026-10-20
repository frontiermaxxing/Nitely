param(
  [Parameter(Mandatory)][string]$Candidate,
  [Parameter(Mandatory)][string]$Baseline,
  [Parameter(Mandatory)][string]$Output,
  [Parameter(Mandatory)][string]$Source,
  [Parameter(Mandatory)][ValidateSet('stable','beta','nightly','test')][string]$Channel
)
$ErrorActionPreference = 'Stop'
if ($env:CI -ne 'true' -or $env:RUNNER_OS -ne 'Windows') { throw 'Installer checks run only on Windows CI' }
$Candidate = (Resolve-Path $Candidate).Path
$Baseline = (Resolve-Path $Baseline).Path
$Output = [IO.Path]::GetFullPath($Output)
if (Test-Path -LiteralPath $Output) { throw 'Release output must be new' }
New-Item -ItemType Directory $Output | Out-Null
$old = @(Get-ChildItem $Baseline -Filter '*-setup.exe')
$setup = @(Get-ChildItem (Join-Path $Candidate 'target/release/bundle/nsis') -Filter '*-setup.exe')
if ($old.Count -ne 1 -or $setup.Count -ne 1) { throw 'Expected exactly one baseline and candidate installer' }
$expected = Get-Content (Join-Path $Baseline 'SHA256SUMS.txt') | Where-Object { $_ -match ('^[a-f0-9]{64} [ *]' + [regex]::Escape($old[0].Name) + '$') }
if (@($expected).Count -ne 1 -or (Get-FileHash $old[0].FullName -Algorithm SHA256).Hash -ne $expected.Substring(0,64)) { throw 'Baseline installer checksum differs' }
$env:NITELY_HOME = Join-Path $env:RUNNER_TEMP 'release-upgrade-data'
$env:NITELY_AGENT_HOME = Join-Path $env:RUNNER_TEMP 'release-upgrade-agents'
New-Item -ItemType Directory $env:NITELY_HOME, $env:NITELY_AGENT_HOME | Out-Null
$env:PATH = ($env:PATH -split ';' | Where-Object { $_ -and $_ -notmatch '\\.cargo\\bin' }) -join ';'
function Install-Candidate([string]$Path) {
  $process = Start-Process -FilePath $Path -ArgumentList '/S' -WindowStyle Hidden -Wait -PassThru
  if ($process.ExitCode -ne 0) { throw "Installer exited $($process.ExitCode)" }
  # NSIS updates the user PATH, not this process's existing environment.
  $env:PATH = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
  $cli = (Get-Command nitely -ErrorAction Stop).Source
  if ($cli -notlike "$env:LOCALAPPDATA\*") { throw 'Installer did not provide the CLI on user PATH' }
  return $cli
}
$cli = Install-Candidate $old[0].FullName
& $cli status 'release-upgrade-preserve-me' --session release-upgrade
if ($LASTEXITCODE -ne 0) { throw 'Baseline could not create throwaway data' }
$cli = Install-Candidate $setup[0].FullName
$version = (Get-Content (Join-Path $Candidate 'package.json') -Raw | ConvertFrom-Json).version
$actual = & $cli --version
if ($LASTEXITCODE -ne 0 -or "$actual" -notmatch [regex]::Escape($version)) { throw 'Installed CLI version differs from candidate' }
if ((Get-FileHash $cli -Algorithm SHA256).Hash -ne (Get-FileHash (Join-Path $Candidate 'target/release/nitely.exe') -Algorithm SHA256).Hash) {
  throw 'Installed CLI bytes differ from the candidate artifact'
}
& $cli status 'release-upgrade-new-build' --session release-upgrade-new
if ($LASTEXITCODE -ne 0) { throw 'Candidate failed to open/migrate baseline data' }
python (Join-Path $PSScriptRoot 'release-upgrade-data.py') $cli (Join-Path $env:NITELY_HOME 'nitely.db')
if ($LASTEXITCODE -ne 0) { throw 'Migration/recovery check failed' }
$head = git -C $Candidate rev-parse HEAD
Copy-Item $setup[0].FullName (Join-Path $Output "Nitely_${version}_x64-setup.exe")
if (Test-Path -LiteralPath "$($setup[0].FullName).sig") {
  Copy-Item "$($setup[0].FullName).sig" (Join-Path $Output "Nitely_${version}_x64-setup.exe.sig")
}
Copy-Item (Join-Path $Candidate 'target/release/nitely.exe') (Join-Path $Output "nitely_${version}_x86_64-pc-windows-msvc.exe")
@{ candidate=$head; source=$Source; version=$version; channel=$Channel } | ConvertTo-Json | Set-Content (Join-Path $Output 'candidate.json') -Encoding utf8NoBOM
$sums = Get-ChildItem $Output -File | Sort-Object Name | ForEach-Object { (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() + '  ' + $_.Name }
$sums | Set-Content (Join-Path $Output 'SHA256SUMS.txt') -Encoding utf8NoBOM
