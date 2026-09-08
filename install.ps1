[CmdletBinding()]
param()

$Version = $env:CONTRIBAI_VERSION
$ErrorActionPreference = "Stop"
$Repo = "tang-vu/ContribAI"
if ([string]::IsNullOrEmpty($Version)) {
    $ReleasePrefix = "https://github.com/$Repo/releases/tag/"
    try {
        $LatestResponse = Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest" -Method Head -MaximumRedirection 5 -TimeoutSec 60 -UseBasicParsing
        $LatestTarget = if ($PSVersionTable.PSVersion.Major -ge 6) {
            $LatestResponse.BaseResponse.RequestMessage.RequestUri.AbsoluteUri
        } else {
            $LatestResponse.BaseResponse.ResponseUri.AbsoluteUri
        }
    } catch {
        throw "Cannot resolve the latest published ContribAI release."
    }
    if (-not $LatestTarget -or -not $LatestTarget.StartsWith($ReleasePrefix, [StringComparison]::Ordinal)) {
        throw "Unexpected latest-release destination; refusing to install."
    }
    $Version = $LatestTarget.Substring($ReleasePrefix.Length)
}
if ($Version -cnotmatch '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\z') {
    throw "CONTRIBAI_VERSION must be a release tag such as v6.10.0."
}
$Binary = "contribai-$Version-windows-x86_64.exe"
$InstallDir = if ($env:CONTRIBAI_INSTALL_DIR) {
    $env:CONTRIBAI_INSTALL_DIR
} else {
    "$env:USERPROFILE\.contribai\bin"
}
$Url = "https://github.com/$Repo/releases/download/$Version/$Binary"
$ChecksumUrl = "$Url.sha256"

Write-Host "Installing ContribAI $Version for Windows..." -ForegroundColor Cyan
$OutPath = Join-Path $InstallDir "contribai.exe"
$TempPath = Join-Path ([IO.Path]::GetTempPath()) "contribai-$([Guid]::NewGuid()).tmp"
$ChecksumTempPath = "$TempPath.sha256"

try {
    Write-Host "  Downloading: $Url"
    try {
        Invoke-WebRequest -Uri $Url -OutFile $TempPath -UseBasicParsing
    } catch {
        throw "Release binary download failed."
    }
    try {
        Invoke-WebRequest -Uri $ChecksumUrl -OutFile $ChecksumTempPath -UseBasicParsing
    } catch {
        throw "Release checksum download failed."
    }

    $ExpectedSha256 = ((Get-Content -LiteralPath $ChecksumTempPath -TotalCount 1) -split '\s+')[0].ToLowerInvariant()
    if ($ExpectedSha256 -notmatch '^[0-9a-f]{64}$') {
        throw "Release checksum is malformed; refusing to install."
    }

    $ActualSha256 = (Get-FileHash -LiteralPath $TempPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($ActualSha256 -ne $ExpectedSha256) {
        throw "Checksum verification failed; refusing to install. Expected $ExpectedSha256, got $ActualSha256."
    }
    Write-Host "  SHA256 checksum verified." -ForegroundColor Green

    if (-not (Test-Path -LiteralPath $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }
    Move-Item -LiteralPath $TempPath -Destination $OutPath -Force
} finally {
    if (Test-Path -LiteralPath $TempPath) {
        Remove-Item -LiteralPath $TempPath -Force
    }
    if (Test-Path -LiteralPath $ChecksumTempPath) {
        Remove-Item -LiteralPath $ChecksumTempPath -Force
    }
}

# Add to user PATH unless an isolated smoke install requested otherwise.
$UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
$PathEntries = @($UserPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
$ShouldUpdatePath = $env:CONTRIBAI_NO_PATH_UPDATE -ne "1"
if ($ShouldUpdatePath -and $InstallDir -notin $PathEntries) {
    $NewUserPath = (@($PathEntries) + $InstallDir) -join ';'
    [Environment]::SetEnvironmentVariable("PATH", $NewUserPath, "User")
    Write-Host "  Added $InstallDir to PATH" -ForegroundColor Green
}

Write-Host ""
Write-Host "ContribAI installed successfully!" -ForegroundColor Green
Write-Host "  Location: $OutPath"
Write-Host "  Run 'contribai demo' before adding credentials." -ForegroundColor Yellow
