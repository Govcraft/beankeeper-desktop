# Install Beankeeper Desktop from GitHub Releases. Supports Windows PowerShell 5.1+.
[CmdletBinding()]
param(
    [string]$Version = $env:BK_VERSION,
    [string]$InstallDir = $(if ($env:BK_INSTALL) { $env:BK_INSTALL } else {
        Join-Path $env:LOCALAPPDATA 'Programs\Beankeeper Desktop'
    }),
    [switch]$NoService = ($env:BK_NO_SERVICE -eq '1'),
    [switch]$NoAgents = ($env:BK_NO_AGENTS -eq '1'),
    [switch]$RequireSignatures = ($env:BK_REQUIRE_SIGNATURES -eq '1'),
    [switch]$VerifyAttestation = ($env:BK_VERIFY_ATTESTATION -eq '1'),
    [switch]$RequireAgentSetup = ($env:BK_REQUIRE_AGENT_SETUP -eq '1')
)
$ErrorActionPreference = 'Stop'
$Repository = 'Govcraft/beankeeper-desktop'

function Get-ReleaseFile([string]$Url, [string]$Destination) {
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Destination
}

function Get-PublishedReleases {
    $token = if ($env:GH_TOKEN) { $env:GH_TOKEN } else { $env:GITHUB_TOKEN }
    $request = @{
        Uri = "https://api.github.com/repos/$Repository/releases?per_page=100"
        UseBasicParsing = $true
        MaximumRedirection = 0
    }
    if ($token) { $request.Headers = @{ Authorization = "Bearer $token" } }

    foreach ($backoff in 60, 120, 0) {
        try {
            return Invoke-RestMethod @request
        } catch {
            $response = $_.Exception.Response
            if (-not $response) { throw }
            $status = [int]$response.StatusCode
            if ($status -ne 429 -and ($status -ne 403 -or $token)) { throw }
            if ($token -or $backoff -eq 0) { break }

            # GitHub can ask us to wait longer than the local retry window.
            $retryAfter = $reset = [long]0
            $delay = $backoff
            if ([long]::TryParse([string]$response.Headers['Retry-After'], [ref]$retryAfter)) {
                $delay = [Math]::Max([long]$delay, $retryAfter)
            }
            if ($response.Headers['X-RateLimit-Remaining'] -eq '0' -and
                [long]::TryParse([string]$response.Headers['X-RateLimit-Reset'], [ref]$reset)) {
                $delay = [Math]::Max([long]$delay, $reset - [DateTimeOffset]::UtcNow.ToUnixTimeSeconds())
            }
            if ($delay -gt 120) { break }
            Write-Warning "GitHub rate limit (HTTP $status). Retrying release discovery in $delay seconds."
            Start-Sleep -Seconds $delay
        }
    }

    throw "GitHub rate limit (HTTP $status) while listing releases. Set BK_VERSION to a signed release to skip discovery."
}

function Find-SignedReleaseVersion([switch]$PreferStable) {
    $releases = Get-PublishedReleases
    $candidates = foreach ($release in $releases) {
        if ($release.draft -ne $false -or $release.tag_name -cnotmatch '\Abeankeeper-desktop-v(?<core>[0-9]+\.[0-9]+\.[0-9]+)(?:-rc\.(?<rc>[0-9]+))?\z') { continue }
        $number = [version]::new()
        $rc = [uint64]0
        $stable = -not $Matches.ContainsKey('rc')
        if ($release.prerelease -ne (-not $stable) -or
            -not [version]::TryParse($Matches.core, [ref]$number) -or
            (-not $stable -and -not [uint64]::TryParse($Matches.rc, [ref]$rc))) { continue }

        $version = $release.tag_name.Substring('beankeeper-desktop-v'.Length)
        $archive = "bk-$version-x86_64-pc-windows-msvc.zip"
        $names = $release.assets.name
        if ($names -cnotcontains $archive -or $names -cnotcontains "$archive.sigstore" -or
            $names -cnotcontains 'checksums-sha256.txt' -or $names -cnotcontains 'checksums-sha256.txt.sigstore') { continue }

        [pscustomobject]@{ Version = $version; Number = $number; Stable = $stable; Candidate = $rc }
    }
    $latest = $candidates | Sort-Object { $PreferStable -and $_.Stable }, Number, Stable, Candidate -Descending |
        Select-Object -First 1
    if (-not $latest) { throw 'No published release with signature bundles found for x64 Windows' }
    $latest.Version
}

function Resolve-ReleaseVersion {
    if ($Version) {
        if ($Version -notmatch '\A[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?\z') { throw "Invalid release version: $Version" }
        return $Version
    }
    Find-SignedReleaseVersion -PreferStable:(-not $RequireAgentSetup)
}

function Test-ReleaseSignature([string]$BaseUrl, [string]$Artifact, [string]$Directory, [string]$Identity) {
    $bundle = Join-Path $Directory "$Artifact.sigstore"
    try {
        Get-ReleaseFile "$BaseUrl/$Artifact.sigstore" $bundle
    } catch {
        if ($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -eq 404) {
            if ($RequireSignatures) { throw "Required Sigstore bundle missing for $Artifact (HTTP 404)" }
            Write-Warning "Sigstore bundle missing for $Artifact (HTTP 404); only SHA-256 will be checked for this artifact"
            return
        }
        throw
    }
    & cosign verify-blob --bundle $bundle --certificate-identity $Identity `
        --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' (Join-Path $Directory $Artifact)
    if ($LASTEXITCODE -ne 0) { throw "Sigstore verification failed for $Artifact" }
}

function Test-ReleaseSignatures([string]$BaseUrl, [string]$Archive, [string]$Directory, [string]$ReleaseVersion) {
    $releaseRef = "refs/tags/beankeeper-desktop-v$ReleaseVersion"
    $identity = "https://github.com/$Repository/.github/workflows/release.yml@$releaseRef"
    $artifacts = @($Archive, 'checksums-sha256.txt')
    if (Get-Command cosign -ErrorAction SilentlyContinue) {
        foreach ($artifact in $artifacts) { Test-ReleaseSignature $BaseUrl $artifact $Directory $identity }
        return
    }
    if ($RequireSignatures) { throw 'cosign is required to verify this release. Install it from a trusted package source and retry.' }
    if (-not $VerifyAttestation) {
        Write-Warning 'cosign not found: only SHA-256 will be checked; release authenticity is not verified'
        return
    }
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
        throw 'GitHub provenance verification requires gh. Install it and authenticate with gh auth login.'
    }
    foreach ($artifact in $artifacts) {
        Write-Host "Verifying GitHub build provenance for $artifact..."
        & gh attestation verify (Join-Path $Directory $artifact) --hostname github.com --repo $Repository `
            --cert-identity $identity --source-ref $releaseRef
        if ($LASTEXITCODE -ne 0) { throw "GitHub build provenance verification failed for $artifact" }
    }
}

function Install-Beankeeper {
    $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($env:OS -ne 'Windows_NT' -or $architecture -ne 'AMD64') {
        throw 'This installer requires x64 Windows; ARM64 Windows binaries are not yet provided'
    }
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $releaseVersion = Resolve-ReleaseVersion
    $stem = "bk-$releaseVersion-x86_64-pc-windows-msvc"
    $artifact = "$stem.zip"
    $url = "https://github.com/$Repository/releases/download/beankeeper-desktop-v$releaseVersion"
    $temporary = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $temporary | Out-Null
    try {
        Get-ReleaseFile "$url/$artifact" (Join-Path $temporary $artifact)
        Get-ReleaseFile "$url/checksums-sha256.txt" (Join-Path $temporary 'checksums-sha256.txt')
        Test-ReleaseSignatures $url $artifact $temporary $releaseVersion
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
        if ($RequireAgentSetup) {
            & $binary service install --help 2>&1 | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "bk $releaseVersion lacks managed service installation. Select a compatible signed release." }
            & $binary agents connect --help 2>&1 | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "bk $releaseVersion lacks agent registration. Select a compatible signed release." }
        }
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        $installed = Join-Path $InstallDir 'bk.exe'
        if (Test-Path -LiteralPath $installed) {
            & $installed service --help 2>&1 | Out-Null
            if ($LASTEXITCODE -eq 0) {
                & $installed service stop
                if ($LASTEXITCODE -ne 0) { throw 'Could not stop the existing background service' }
            }
        }
        $copied = $false
        for ($attempt = 0; $attempt -lt 40; $attempt++) {
            try { Copy-Item -LiteralPath $binary -Destination $installed -Force; $copied = $true; break }
            catch { Start-Sleep -Milliseconds 250 }
        }
        if (-not $copied) { throw 'The installed binary is still in use; close bk processes and retry' }
        if (-not $NoService) {
            & $installed service install --no-agents
            if ($LASTEXITCODE -ne 0) { throw 'Background setup failed. Run bk service install --no-agents in your desktop session.' }
            if (-not $NoAgents) {
                & $installed agents connect --all
                if ($LASTEXITCODE -ne 0) { throw 'Agent registration needs attention. Run bk agents doctor and retry bk agents connect --all.' }
            }
        }
        Write-Host "Installed bk $releaseVersion to $InstallDir. Add this directory to your PATH."
    } finally {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
}

if ($MyInvocation.InvocationName -ne '.') { Install-Beankeeper }
