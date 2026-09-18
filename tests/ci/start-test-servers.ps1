<#
Starts a MariaDB and a MySQL server for the live test suites, loads the shared fixture into both,
and puts the client tools where the apps look for them. Used by CI in this repo and in
NOBS-SQL-Editor-PowerShell; runnable locally on spare ports:

  pwsh -File tests/ci/start-test-servers.ps1 -Root C:\nobs-ci -MariaPort 3316 -MysqlPort 3318 `
       -MysqlHome C:\nobs-ci\mysql -AppData C:\nobs-ci\appdata

In CI the defaults apply: MySQL is unpacked where a real installation lives
(Program Files\MySQL\MySQL Server <series>), which is exactly where the apps look for MySQL's own
client tools, and MariaDB's client tools go where the apps' own download puts them.

What it writes for later steps (to $env:GITHUB_ENV when set, and always to the console):
  NOBS_CI_MARIADB_DSN / NOBS_CI_MYSQL_DSN   host:port:user:password
  NOBS_CI_MARIADB_CA  / NOBS_CI_MYSQL_CA    each server's own CA certificate (.pem)
  NOBS_CI_REMOTE_HOST                        an address of this machine other than loopback
  MYSQL_BIN / MYSQLDUMP_BIN                  MariaDB's client tools, as the Tauri tests expect
#>
param(
    [string]$Root = 'C:\nobs-ci',
    [int]$MariaPort = 3306,
    [int]$MysqlPort = 3308,
    [string]$Password = 'CiTest.123',
    [string]$MariaVersion = '12.3.3',
    [string]$MysqlVersion = '8.4.9',
    [string]$MysqlHome,
    [string]$AppData = $env:APPDATA,
    [string]$Fixture = (Join-Path $PSScriptRoot '..\fixtures\seed.sql')
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$series = ($MysqlVersion.Split('.')[0..1]) -join '.'
if (-not $MysqlHome) { $MysqlHome = Join-Path $env:ProgramFiles "MySQL\MySQL Server $series" }
$cache = Join-Path $Root 'cache'
New-Item -ItemType Directory -Force $cache | Out-Null

function Get-Archive([string]$Name, [string[]]$Urls) {
    $file = Join-Path $cache $Name
    if (Test-Path $file) { return $file }
    foreach ($u in $Urls) {
        Write-Host "downloading $u"
        # dev.mysql.com and its CDN refuse browser-like user agents; curl's is accepted.
        & curl.exe -fsSL --retry 3 -A 'curl/8.0 NOBSSQL-CI' -o "$file.part" $u
        if ($LASTEXITCODE -eq 0) { Move-Item "$file.part" $file -Force; return $file }
        Remove-Item "$file.part" -Force -ErrorAction SilentlyContinue
    }
    throw "Could not download $Name"
}
function Expand-Flat([string]$Zip, [string]$Dest) {
    # Both archives hold a single top-level folder; its contents go straight into $Dest.
    $tmp = Join-Path $Root ('x-' + [Guid]::NewGuid().ToString('N'))
    Expand-Archive -LiteralPath $Zip -DestinationPath $tmp -Force
    $inner = Get-ChildItem -LiteralPath $tmp -Directory | Select-Object -First 1
    New-Item -ItemType Directory -Force $Dest | Out-Null
    Get-ChildItem -LiteralPath $inner.FullName | Move-Item -Destination $Dest -Force
    Remove-Item $tmp -Recurse -Force
}
function Wait-Port([int]$Port, [string]$What) {
    for ($i = 0; $i -lt 120; $i++) {
        $c = New-Object Net.Sockets.TcpClient
        try { $c.Connect('127.0.0.1', $Port); return } catch { Start-Sleep -Milliseconds 500 } finally { $c.Dispose() }
    }
    throw "$What did not start listening on $Port"
}
function Invoke-Sql([string]$Client, [int]$Port, [string]$Sql, [string]$File, [switch]$NoPassword, [string]$PluginDir) {
    $cargs = @('--protocol=TCP', '-h127.0.0.1', "-P$Port", '-uroot')
    if (-not $NoPassword) { $cargs += "-p$Password" }
    if ($PluginDir) { $cargs += "--plugin-dir=$PluginDir" }
    if ($File) {
        $p = Start-Process -FilePath $Client -ArgumentList $cargs -RedirectStandardInput $File -NoNewWindow -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "loading $File on port $Port failed ($($p.ExitCode))" }
    } else {
        & $Client @cargs -N -e $Sql
        if ($LASTEXITCODE -ne 0) { throw "'$Sql' on port $Port failed" }
    }
}

# --- MariaDB --------------------------------------------------------------------------------
$mariaHome = Join-Path $Root "mariadb-$MariaVersion"
if (-not (Test-Path (Join-Path $mariaHome 'bin\mariadbd.exe'))) {
    $z = Get-Archive "mariadb-$MariaVersion-winx64.zip" @(
        "https://mirror.mariadb.org/mariadb-$MariaVersion/winx64-packages/mariadb-$MariaVersion-winx64.zip",
        "https://archive.mariadb.org/mariadb-$MariaVersion/winx64-packages/mariadb-$MariaVersion-winx64.zip")
    Expand-Flat $z $mariaHome
}
$mariaData = Join-Path $Root 'mariadb-data'
if (-not (Test-Path $mariaData)) {
    & (Join-Path $mariaHome 'bin\mariadb-install-db.exe') "--datadir=$mariaData" "--password=$Password" "--port=$MariaPort" '--allow-remote-root-access'
    if ($LASTEXITCODE -ne 0) { throw 'mariadb-install-db failed' }
}
# The servers write their own log files. Redirecting their output instead hands them this script's
# output handles, and whoever waits for those to close - a CI step, a shell pipe - then waits for
# as long as the server runs.
Start-Process -FilePath (Join-Path $mariaHome 'bin\mariadbd.exe') -WindowStyle Hidden `
    -ArgumentList "--defaults-file=`"$mariaData\my.ini`"", "--log-error=`"$(Join-Path $Root 'mariadb.err')`""
Wait-Port $MariaPort 'MariaDB'
$mariaClient = Join-Path $mariaHome 'bin\mariadb.exe'

# --- MySQL ----------------------------------------------------------------------------------
if (-not (Test-Path (Join-Path $MysqlHome 'bin\mysqld.exe'))) {
    $file = "mysql-$MysqlVersion-winx64.zip"
    $z = Get-Archive $file @(
        "https://cdn.mysql.com/Downloads/MySQL-$series/$file",
        "https://downloads.mysql.com/archives/get/p/23/file/$file")
    Expand-Flat $z $MysqlHome
}
$mysqlData = Join-Path $Root 'mysql-data'
if (-not (Test-Path $mysqlData)) {
    & (Join-Path $MysqlHome 'bin\mysqld.exe') '--initialize-insecure' "--basedir=$MysqlHome" "--datadir=$mysqlData" '--console'
    if ($LASTEXITCODE -ne 0) { throw 'mysqld --initialize failed' }
    $fresh = $true
}
Start-Process -FilePath (Join-Path $MysqlHome 'bin\mysqld.exe') -ArgumentList "--basedir=`"$MysqlHome`"", "--datadir=`"$mysqlData`"", "--port=$MysqlPort", '--mysqlx=OFF', "--log-error=`"$(Join-Path $Root 'mysql.err')`"" `
    -WindowStyle Hidden
Wait-Port $MysqlPort 'MySQL'
$mysqlClient = Join-Path $MysqlHome 'bin\mysql.exe'
if ($fresh) {
    Invoke-Sql $mysqlClient $MysqlPort "ALTER USER 'root'@'localhost' IDENTIFIED BY '$Password'; CREATE USER 'root'@'%' IDENTIFIED BY '$Password'; GRANT ALL ON *.* TO 'root'@'%' WITH GRANT OPTION;" -NoPassword
}

# --- the fixture ------------------------------------------------------------------------------
Invoke-Sql $mariaClient $MariaPort -File (Resolve-Path $Fixture).Path
Invoke-Sql $mysqlClient $MysqlPort -File (Resolve-Path $Fixture).Path
foreach ($p in @(@{ C = $mariaClient; P = $MariaPort }, @{ C = $mysqlClient; P = $MysqlPort })) {
    Invoke-Sql $p.C $p.P 'SELECT VERSION(), (SELECT COUNT(*) FROM nobs_test.ro_canary)'
}

# --- MariaDB's client tools, where each app's own download puts them --------------------------
$authPlugins = 'caching_sha2_password.dll', 'sha256_password.dll', 'client_ed25519.dll', 'parsec.dll', 'dialog.dll',
               'mysql_clear_password.dll', 'auth_gssapi_client.dll', 'authentication_windows_client.dll', 'auth_named_pipe.dll'
foreach ($app in 'NOBSSQL', 'NOBSSQL-Desktop') {
    $bin = Join-Path $AppData "$app\bin"
    New-Item -ItemType Directory -Force (Join-Path $bin 'plugin') | Out-Null
    foreach ($exe in 'mysql.exe', 'mysqldump.exe', 'mariadb.exe', 'mariadb-dump.exe', 'mysqlcheck.exe', 'mysqlimport.exe') {
        $src = Join-Path $mariaHome "bin\$exe"
        if (Test-Path $src) { Copy-Item $src $bin -Force }
    }
    foreach ($dll in $authPlugins) {
        $src = Join-Path $mariaHome "lib\plugin\$dll"
        if (Test-Path $src) { Copy-Item $src (Join-Path $bin 'plugin') -Force }
    }
}
# The PowerShell edition reads its paths from config.json, as it would after a download.
$psCfg = Join-Path $AppData 'NOBSSQL\config.json'
if (-not (Test-Path $psCfg)) {
    @{ mysql_bin = (Join-Path $AppData 'NOBSSQL\bin\mysql.exe'); mysqldump_bin = (Join-Path $AppData 'NOBSSQL\bin\mysqldump.exe') } |
        ConvertTo-Json | Set-Content -Encoding utf8 $psCfg
}

# --- the CA certificates ------------------------------------------------------------------------
# MySQL keeps the CA it generated in its data directory. MariaDB 11.4+ generates a self-signed
# certificate in memory only, so it is taken off the wire - the last certificate the server sends.
$mysqlCa = Join-Path $mysqlData 'ca.pem'
$mariaCa = Join-Path $Root 'mariadb-ca.pem'
$openssl = @('openssl.exe', (Join-Path $env:ProgramFiles 'Git\usr\bin\openssl.exe')) |
    Where-Object { Get-Command $_ -ErrorAction SilentlyContinue } | Select-Object -First 1
$chain = ('' | & $openssl s_client -starttls mysql -connect "127.0.0.1:$MariaPort" -showcerts 2>$null) -join "`n"
$certs = [regex]::Matches($chain, '-----BEGIN CERTIFICATE-----[\s\S]*?-----END CERTIFICATE-----')
if ($certs.Count -eq 0) { throw 'could not read the MariaDB certificate' }
Set-Content -Encoding ascii -Path $mariaCa -Value $certs[$certs.Count - 1].Value

$remote = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -notmatch '^(127\.|169\.254\.)' } |
    Select-Object -First 1).IPAddress

$out = [ordered]@{
    NOBS_CI_MARIADB_DSN = "127.0.0.1:$($MariaPort):root:$Password"
    NOBS_CI_MYSQL_DSN   = "127.0.0.1:$($MysqlPort):root:$Password"
    NOBS_CI_MARIADB_CA  = ($mariaCa -replace '\\', '/')
    NOBS_CI_MYSQL_CA    = ($mysqlCa -replace '\\', '/')
    NOBS_CI_REMOTE_HOST = $remote
    MYSQL_BIN           = (Join-Path $AppData 'NOBSSQL-Desktop\bin\mysql.exe')
    MYSQLDUMP_BIN       = (Join-Path $AppData 'NOBSSQL-Desktop\bin\mysqldump.exe')
}
foreach ($k in $out.Keys) {
    "$k=$($out[$k])"
    if ($env:GITHUB_ENV) { Add-Content -Encoding utf8 -Path $env:GITHUB_ENV -Value "$k=$($out[$k])" }
}
