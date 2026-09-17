<#
Installs, finds and uninstalls NOBS SQL Editor for the upgrade test (.github/workflows/upgrade.yml).

  installer.ps1 -Action install -File <setup.exe | .msi>   silently, and waits until it is done
  installer.ps1 -Action list                               the installed copies, as Windows lists them
  installer.ps1 -Action uninstall                          silently, every installed copy

"list" prints one line per copy: version, then the path of the app's exe.
#>
param(
    [Parameter(Mandatory)][ValidateSet('install', 'list', 'uninstall')][string]$Action,
    [string]$File
)
$ErrorActionPreference = 'Stop'

# The entries Windows shows under Settings -> Apps: per user for the setup.exe, per machine for
# the MSI.
function Get-Installs {
    $keys = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    foreach ($k in $keys) {
        Get-ItemProperty $k -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -like 'NOBS SQL Editor*' } | ForEach-Object {
            $dir = [string]$_.InstallLocation
            if (-not $dir -and $_.UninstallString -notmatch 'msiexec') { $dir = Split-Path ($_.UninstallString -replace '"', '') }
            if (-not $dir) { $dir = Join-Path $env:ProgramFiles 'NOBS SQL Editor' }
            $exe = Get-ChildItem -LiteralPath $dir -Filter *.exe -ErrorAction SilentlyContinue | Where-Object { $_.Name -notmatch 'uninstall' } | Select-Object -First 1
            [pscustomobject]@{ Version = $_.DisplayVersion; Exe = $(if ($exe) { $exe.FullName } else { '' }); Key = $_.PSChildName; Uninstall = $_.UninstallString; Msi = ($_.UninstallString -match 'msiexec') }
        }
    }
}

function Wait-Ok([System.Diagnostics.Process]$p, [string]$what) {
    $null = $p.Handle   # without it, ExitCode stays empty
    $p.WaitForExit()
    if ($p.ExitCode -ne 0) { throw "$what exited with code $($p.ExitCode)" }
}

switch ($Action) {
    'install' {
        $full = (Resolve-Path $File).Path
        if ($full -like '*.msi') {
            $log = Join-Path $env:RUNNER_TEMP ('msi-' + [IO.Path]::GetFileNameWithoutExtension($full) + '.log')
            Wait-Ok (Start-Process msiexec -ArgumentList '/i', "`"$full`"", '/qn', '/norestart', '/l*v', "`"$log`"" -PassThru) "msiexec /i $full"
        } else {
            Wait-Ok (Start-Process $full -ArgumentList '/S' -PassThru) $full
        }
        # The setup.exe returns before its entry is written on some runs; give it a moment.
        for ($i = 0; $i -lt 60 -and -not (Get-Installs | Where-Object Exe); $i++) { Start-Sleep -Milliseconds 500 }
        Get-Installs | ForEach-Object { "$($_.Version) $($_.Exe)" }
    }
    'list' { Get-Installs | ForEach-Object { "$($_.Version) $($_.Exe)" } }
    'uninstall' {
        foreach ($i in @(Get-Installs)) {
            if ($i.Msi) {
                Wait-Ok (Start-Process msiexec -ArgumentList '/x', $i.Key, '/qn', '/norestart' -PassThru) "msiexec /x $($i.Key)"
            } else {
                # The NSIS uninstaller copies itself away and returns at once, so wait for the files.
                $un = ($i.Uninstall -replace '"', '')
                Start-Process $un -ArgumentList '/S' | Out-Null
                for ($n = 0; $n -lt 120 -and (Test-Path -LiteralPath $un); $n++) { Start-Sleep -Milliseconds 500 }
            }
        }
        for ($n = 0; $n -lt 60 -and (Get-Installs); $n++) { Start-Sleep -Milliseconds 500 }
        Get-Installs | ForEach-Object { "still installed: $($_.Version) $($_.Exe)" }
    }
}
