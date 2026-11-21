# Offline integration tests against the Windows binary built by this job.
$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$temporary = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
$fixture = Join-Path $temporary 'fixtures'
$destination = Join-Path $temporary 'installed'
New-Item -ItemType Directory -Path $fixture, $destination | Out-Null
$releaseVersion = (python "$root/scripts/release.py" version).Trim()
$stem = "bk-$releaseVersion-x86_64-pc-windows-msvc"
$artifact = "$stem.zip"
Copy-Item "$root/dist/$artifact" $fixture
$digest = (Get-FileHash "$fixture/$artifact" -Algorithm SHA256).Hash
$manifest = "$fixture/checksums-sha256.txt"
"$digest  $artifact" | Set-Content $manifest
. "$root/install.ps1" -Version $releaseVersion -InstallDir $destination
$script:bundleStatus = 404
$script:signatureFails = $false
$script:verified = @()

function Get-ReleaseFile([string]$Url, [string]$Destination) {
    if ($Url -notlike "https://github.com/Govcraft/beankeeper-desktop/releases/download/beankeeper-desktop-v$releaseVersion/*") {
        throw "Unexpected URL $Url"
    }
    if ($Url.EndsWith('.sigstore')) {
        if ($script:bundleStatus -eq 200) {
            'fixture' | Set-Content $Destination
        } else {
            $errorObject = [Exception]::new('Simulated HTTP error')
            $errorObject | Add-Member -NotePropertyName Response -NotePropertyValue ([pscustomobject]@{ StatusCode = $script:bundleStatus })
            throw $errorObject
        }
    } else {
        Copy-Item (Join-Path $fixture ($Url.Split('/')[-1])) $Destination
    }
}
function cosign {
    if ($args -notcontains "https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v$releaseVersion") {
        throw 'Wrong certificate identity'
    }
    if ($args -notcontains 'https://token.actions.githubusercontent.com') { throw 'Wrong OIDC issuer' }
    $script:verified += Split-Path $args[-1] -Leaf
    $global:LASTEXITCODE = if ($script:signatureFails) { 1 } else { 0 }
}
function Assert-Rejected([string]$Message) {
    'previous version' | Set-Content "$destination/bk.exe"
    $rejected = $false
    try { Install-Beankeeper } catch {
        if ($_.Exception.Message -notlike "*$Message*") { throw }
        $rejected = $true
    }
    if (-not $rejected) { throw "Expected failure: $Message" }
    if ((Get-Content "$destination/bk.exe").Trim() -ne 'previous version') { throw 'Replaced binary before verification' }
}
try {
    Install-Beankeeper # legacy bundle 404 fallback
    if (-not (Test-Path "$destination/bk.exe")) { throw 'Legacy install failed' }
    $script:bundleStatus = 200
    Install-Beankeeper
    if ($script:verified.Count -ne 2 -or $script:verified -notcontains $artifact -or $script:verified -notcontains 'checksums-sha256.txt') {
        throw 'Expected both bundles to be verified'
    }
    $script:signatureFails = $true
    Assert-Rejected 'Sigstore verification failed'
    $script:signatureFails = $false
    $script:bundleStatus = 503
    Assert-Rejected 'Simulated HTTP error'
    $script:bundleStatus = 404
    "$('0' * 64)  $artifact" | Set-Content $manifest
    Assert-Rejected 'Checksum mismatch'
    '' | Set-Content $manifest
    Assert-Rejected 'exactly one SHA-256'
    "$digest  $artifact`n$digest  $artifact" | Set-Content $manifest
    Assert-Rejected 'exactly one SHA-256'
    # GitHub's pwsh wrapper propagates LASTEXITCODE, including mock failures.
    $global:LASTEXITCODE = 0
    Write-Host 'Windows installer verification tests passed'
} finally {
    Remove-Item -LiteralPath $temporary -Force -Recurse
}
