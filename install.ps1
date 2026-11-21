# Install Beankeeper Desktop from GitHub Releases. Supports Windows PowerShell 5.1+.
[CmdletBinding()]
param(
    [string]$Version = $env:BK_VERSION,
    [string]$InstallDir = $(if ($env:BK_INSTALL) { $env:BK_INSTALL } else {
        Join-Path $env:LOCALAPPDATA 'Programs\Beankeeper Desktop'
    })
)
$ErrorActionPreference = 'Stop'
$Repository = 'Govcraft/beankeeper-desktop'

function Get-ReleaseFile([string]$Url, [string]$Destination) {
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Destination
}

function Test-ReleaseSignature([string]$BaseUrl, [string]$Artifact, [string]$Directory, [string]$ReleaseVersion) {
    if (-not (Get-Command cosign -ErrorAction SilentlyContinue)) {
        Write-Warning 'cosign not found: only SHA-256 will be checked; release authenticity is not verified'
        return
    }
    $bundle = Join-Path $Directory "$Artifact.sigstore"
    try {
        Get-ReleaseFile "$BaseUrl/$Artifact.sigstore" $bundle
    } catch {
        if ($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -eq 404) {
            Write-Warning "Sigstore bundle missing for $Artifact (HTTP 404); only SHA-256 will be checked for this artifact"
            return
        }
        throw
    }
    $identity = "https://github.com/$Repository/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v$ReleaseVersion"
    & cosign verify-blob --bundle $bundle --certificate-identity $identity `
        --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' (Join-Path $Directory $Artifact)
    if ($LASTEXITCODE -ne 0) { throw "Sigstore verification failed for $Artifact" }
}

function Install-Beankeeper {
    $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($env:OS -ne 'Windows_NT' -or $architecture -ne 'AMD64') {
        throw 'This installer requires x64 Windows; ARM64 Windows binaries are not yet provided'
    }
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $releaseVersion = $Version
    if (-not $releaseVersion) {
        $release = Invoke-RestMethod "https://api.github.com/repos/$Repository/releases/latest"
        $releaseVersion = $release.tag_name -replace '^beankeeper-desktop-v', ''
    }
    if ($releaseVersion -notmatch '^\d+\.\d+\.\d+(-rc\.\d+)?$') { throw "Invalid release version: $releaseVersion" }
    $stem = "bk-$releaseVersion-x86_64-pc-windows-msvc"
    $artifact = "$stem.zip"
    $url = "https://github.com/$Repository/releases/download/beankeeper-desktop-v$releaseVersion"
    $temporary = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $temporary | Out-Null
    try {
        Get-ReleaseFile "$url/$artifact" (Join-Path $temporary $artifact)
        Get-ReleaseFile "$url/checksums-sha256.txt" (Join-Path $temporary 'checksums-sha256.txt')
        Test-ReleaseSignature $url $artifact $temporary $releaseVersion
        Test-ReleaseSignature $url 'checksums-sha256.txt' $temporary $releaseVersion
        $checksums = @(Get-Content (Join-Path $temporary 'checksums-sha256.txt') | Where-Object {
            $_ -match ('^[a-fA-F0-9]{64}  ' + [regex]::Escape($artifact) + '$')
        })
        if ($checksums.Count -ne 1) { throw "Expected exactly one SHA-256 checksum for $artifact" }
        $expected = ($checksums[0] -split '  ')[0]
        $actual = (Get-FileHash (Join-Path $temporary $artifact) -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw "Checksum mismatch for $artifact" }
        Expand-Archive -LiteralPath (Join-Path $temporary $artifact) -DestinationPath $temporary
        $binary = Join-Path $temporary "$stem/bk.exe"
        $reportedVersion = & $binary --version
        if ($LASTEXITCODE -ne 0 -or $reportedVersion -ne "bk $releaseVersion") {
            throw "Downloaded binary does not report bk $releaseVersion"
        }
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        Copy-Item -LiteralPath $binary -Destination (Join-Path $InstallDir 'bk.exe') -Force
        Write-Host "Installed bk $releaseVersion to $InstallDir. Add this directory to your PATH."
    } finally {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
}

if ($MyInvocation.InvocationName -ne '.') { Install-Beankeeper }
