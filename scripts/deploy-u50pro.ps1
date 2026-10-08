param(
    [string]$Serial = '',
    [string]$AdbPath = '',
    [switch]$PreflightOnly
)
$ErrorActionPreference = 'Stop'
$remoteStage = ''
$launched = $false
$remoteDir = '/cache/zwrt-datad'

# Quote native argv explicitly, including embedded shell quotes and paths with spaces.
function Quote-NativeArgument([string]$Value) {
    return '"' + [regex]::Replace([regex]::Replace($Value, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1') + '"'
}
function File-Sha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $hash = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($hash.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
    finally { $hash.Dispose(); $stream.Dispose() }
}
function Invoke-Adb([string[]]$Arguments, [int]$TimeoutSeconds = 20, [switch]$AllowFailure) {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $script:AdbPath
    $info.Arguments = ($Arguments | ForEach-Object { Quote-NativeArgument $_ }) -join ' '
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $info
    try {
        [void]$process.Start()
        $outTask = $process.StandardOutput.ReadToEndAsync()
        $errTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            $process.WaitForExit()
            throw "ADB timeout after $TimeoutSeconds seconds"
        }
        $result = [pscustomobject]@{
            Code = $process.ExitCode
            Output = $outTask.Result.Trim()
            Error = $errTask.Result.Trim()
        }
        if ($result.Code -ne 0 -and -not $AllowFailure) {
            throw "ADB failed ($($result.Code)): $($result.Output) $($result.Error)"
        }
        return $result
    } finally { $process.Dispose() }
}
function Device-Shell([string]$Command, [switch]$AllowFailure, [int]$TimeoutSeconds = 20) {
    return Invoke-Adb -Arguments @('-s', $script:Serial, 'shell', $Command) -AllowFailure:$AllowFailure -TimeoutSeconds $TimeoutSeconds
}

try {
    Write-Host '[1/5] Checking local package...'
    if (-not $AdbPath) { $AdbPath = Join-Path $PSScriptRoot 'tools/adb.exe' }
    $AdbPath = (Resolve-Path -LiteralPath $AdbPath).Path
    $payloadDir = Join-Path $PSScriptRoot 'payload'
    $names = @('zwrt-datad', 'start.sh', 'zwrt-datad.service', 'service-control.sh', 'deploy-transaction.sh')
    $expected = @{}
    foreach ($line in Get-Content -LiteralPath (Join-Path $payloadDir 'SHA256SUMS')) {
        if ($line -notmatch '^([0-9a-f]{64})  ([a-zA-Z0-9.-]+)$') { throw 'Invalid package manifest' }
        $name = $Matches[2]
        if ($name -notin $names -or $expected.ContainsKey($name)) { throw 'Unexpected or duplicate package file' }
        $expected[$name] = $Matches[1]
    }
    if ($expected.Count -ne $names.Count) { throw 'Incomplete package manifest' }
    foreach ($name in $names) {
        $actual = File-Sha256 (Join-Path $payloadDir $name)
        if ($actual -ne $expected[$name]) { throw "Local SHA-256 mismatch: $name. Extract a fresh package." }
    }
    $binary = Join-Path $payloadDir 'zwrt-datad'
    $bytes = [IO.File]::ReadAllBytes($binary)
    if ($bytes.Length -lt 64 -or [BitConverter]::ToString($bytes, 0, 6) -ne '7F-45-4C-46-01-01' -or
        $bytes[18] -ne 40 -or $bytes[19] -ne 0) { throw 'Expected little-endian ELF32 ARM binary' }

    Write-Host '[2/5] Finding a root ADB connection to U50 Pro / MU5120...'
    if (-not $Serial) {
        $listing = Invoke-Adb -Arguments @('devices')
        $candidates = @()
        foreach ($line in ($listing.Output -split "`n")) {
            if ($line.Trim() -match '^(\S+)\s+device$') {
                $candidate = $Matches[1]
                $model = Invoke-Adb -Arguments @('-s', $candidate, 'shell', 'cfg get model_name') -AllowFailure
                if ($model.Code -eq 0 -and $model.Output.Trim() -eq 'MU5120') { $candidates += $candidate }
            }
        }
        if ($candidates.Count -eq 0) { throw 'No MU5120 found. Connect the modem with a data cable and enable/authorize ADB.' }
        if ($candidates.Count -ne 1) {
            Write-Host ('MU5120 devices: ' + ($candidates -join ', '))
            throw 'More than one MU5120 found. Run INSTALL.bat DEVICE_SERIAL explicitly.'
        }
        $Serial = $candidates[0]
    }
    $info = Device-Shell 'id -u; uname -m; cfg get model_name'
    $identity = @($info.Output -split '[\r\n]+' | Where-Object { $_ -ne '' })
    if (($identity -join '|') -ne '0|armv7l|MU5120') { throw 'Requires root ADB on MU5120 / armv7l. No deployment performed.' }
    Write-Host "Device: $Serial"
    $requiredKb = [long][Math]::Ceiling(($bytes.LongLength * 3 + 4194304) / 1024.0)
    $preflight = 'for t in sha256sum timeout nohup mktemp df awk tail readlink cp mv chmod dirname cmp ln sleep mkdir rm rmdir sync grep; do command -v $t >/dev/null || exit 1; done; test ! -L /cache/zwrt-datad && test ! -e /cache/zwrt-datad/.deploy-lock && free=$(df -Pk /cache | tail -1 | awk ''{print $4}'') && test "$free" -ge ' + $requiredKb
    [void](Device-Shell $preflight)
    if ($PreflightOnly) {
        Write-Host 'PREFLIGHT OK. No files copied and no service changes.'
        exit 0
    }

    Write-Host '[3/5] Copying package over USB ADB...'
    $stage = Device-Shell 'umask 077; mkdir -p /cache/zwrt-datad && mktemp -d /cache/zwrt-datad/.deploy.XXXXXX'
    $remoteStage = $stage.Output.Trim()
    if ($remoteStage -notmatch '^/cache/zwrt-datad/\.deploy\.[a-zA-Z0-9]+$') { throw 'Invalid device staging path' }
    Write-Host "Device logs: $remoteStage"
    $toPush = @($names) + @('SHA256SUMS')
    for ($index = 0; $index -lt $toPush.Count; $index++) {
        $name = $toPush[$index]
        Write-Host ("  {0}/{1} {2}" -f ($index + 1), $toPush.Count, $name)
        [void](Invoke-Adb -Arguments @('-s', $Serial, 'push', (Join-Path $payloadDir $name), "$remoteStage/$name") -TimeoutSeconds 120)
    }
    $verified = Device-Shell "cd $remoteStage && sha256sum -c SHA256SUMS && chmod 700 zwrt-datad && . ./service-control.sh && run_timeout 10 ./zwrt-datad --version"
    Write-Host $verified.Output
    if ($verified.Output -notmatch '(?m)^zwrt-datad \d+\.\d+\.\d+\s*$') { throw 'Unexpected binary version response; existing service unchanged' }

    Write-Host '[4/5] Installing and verifying service health...'
    # Once started, the device owns the transaction and rollback even if ADB is lost.
    $launched = $true
    [void](Device-Shell "nohup sh $remoteStage/deploy-transaction.sh $remoteStage > $remoteStage/deploy.log 2>&1 < /dev/null &")
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt 180) {
        $result = ''
        try {
            $reply = Device-Shell "cat $remoteStage/result 2>/dev/null" -AllowFailure -TimeoutSeconds 5
            if ($reply.Code -eq 0) { $result = $reply.Output.Trim() }
        } catch { Write-Host 'Waiting for ADB to reconnect; the device transaction continues...' }
        if ($result.StartsWith('SUCCESS:')) {
            Write-Host "[5/5] $result" -ForegroundColor Green
            Write-Host 'App: ZWRT mode / device LAN IP / port 9461 / admin + device web password'
            if ($result.StartsWith('SUCCESS: session;')) {
                Write-Host 'No boot autostart. After reboot run:' -ForegroundColor Yellow
                Write-Host "tools\adb.exe -s $Serial shell sh /cache/zwrt-datad/start.sh"
            }
            exit 0
        }
        if ($result.StartsWith('FAILED:')) {
            $log = Device-Shell "cat $remoteStage/deploy.log" -AllowFailure
            Write-Host $log.Output
            throw $result
        }
        Write-Progress -Activity 'Device deployment' -Status ('Waiting for verified result ({0}s)' -f [int]$timer.Elapsed.TotalSeconds)
        Start-Sleep -Seconds 2
    }
    throw 'No final result received within 180 seconds. Inspect device logs before retrying.'
} catch {
    Write-Host ("FAILED: " + $_.Exception.Message) -ForegroundColor Red
    if ($remoteStage) {
        Write-Host "Inspect: $remoteStage/result and $remoteStage/deploy.log"
        if ($launched) { Write-Host 'The device may still be completing deployment or rollback. Do not remove its lock or retry blindly.' }
    }
    exit 1
}
