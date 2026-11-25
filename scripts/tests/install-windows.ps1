# Offline integration tests against the Windows binary built by this job.
param([switch]$DiscoveryOnly)

$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
# Validate the PowerShell example embedded in the agent skill on this runner.
$guide = Get-Content "$root/SKILL.md" -Raw
$fence = [regex]::Match($guide, '(?s)```powershell\r?\n(.*?)```')
if (-not $fence.Success) { throw 'Missing PowerShell install commands in SKILL.md' }
$guideTokens = $null
$guideErrors = $null
$guideAst = [System.Management.Automation.Language.Parser]::ParseInput($fence.Groups[1].Value, [ref]$guideTokens, [ref]$guideErrors)
if ($guideErrors.Count -gt 0) { throw "Invalid PowerShell install commands: $guideErrors" }
$guideCommands = $guideAst.FindAll({ param($ast) $ast -is [System.Management.Automation.Language.CommandAst] }, $true)
foreach ($required in @(
    '& $bkSetup -RequireSignatures -RequireAgentSetup -NoService -InstallDir $bkDir',
    '& $bk service install --no-agents',
    '& $bk agents connect --all --json',
    '& $bk service status --json'
)) {
    if ($guideCommands.Extent.Text -notcontains $required) { throw "Missing install command in SKILL.md: $required" }
}

function New-ReleaseFixture([string]$FixtureVersion, [switch]$Unsigned) {
    $archive = "bk-$FixtureVersion-x86_64-pc-windows-msvc.zip"
    $names = @($archive, "$archive.sigstore", 'checksums-sha256.txt')
    if (-not $Unsigned) { $names += 'checksums-sha256.txt.sigstore' }
    [pscustomobject]@{
        tag_name = "beankeeper-desktop-v$FixtureVersion"
        draft = $false
        prerelease = $FixtureVersion.Contains('-rc.')
        assets = @($names | ForEach-Object { @{ name = $_ } })
    }
}

function Test-ReleaseDiscovery {
    . "$root/install.ps1" -Version '' -InstallDir ([IO.Path]::GetTempPath()) -RequireAgentSetup:$false
    $discovery = @{
        Statuses = [System.Collections.Generic.Queue[int]]::new()
        Authorization = [System.Collections.Generic.List[string]]::new()
        Delays = [System.Collections.Generic.List[int]]::new()
        ResponseHeaders = @{}
    }
    function Invoke-RestMethod([string]$Uri, [switch]$UseBasicParsing, [int]$MaximumRedirection, [hashtable]$Headers) {
        if ($Uri -ne 'https://api.github.com/repos/Govcraft/beankeeper-desktop/releases?per_page=100') {
            throw "Unexpected discovery request: $Uri"
        }
        $discovery.Authorization.Add($(if ($Headers) { $Headers.Authorization } else { 'anonymous' }))
        if ($discovery.Statuses.Count -gt 0) {
            $status = $discovery.Statuses.Dequeue()
            $errorObject = [Exception]::new("Simulated HTTP $status")
            $errorObject | Add-Member -NotePropertyName Response -NotePropertyValue ([pscustomobject]@{
                StatusCode = $status
                Headers = $discovery.ResponseHeaders
            })
            throw $errorObject
        }
        $releases
    }
    function Start-Sleep([int]$Seconds) { $discovery.Delays.Add($Seconds) }

    $releases = @(
        (New-ReleaseFixture '0.2.0-rc.11' -Unsigned)
        (New-ReleaseFixture '0.2.0-rc.9')
        (New-ReleaseFixture '0.2.0-rc.10')
    )
    if ((Resolve-ReleaseVersion) -cne '0.2.0-rc.10') { throw 'Regular setup did not select the newest signed RC' }

    $releases += (New-ReleaseFixture '0.0.9'), (New-ReleaseFixture '0.1.0')
    if ((Resolve-ReleaseVersion) -cne '0.1.0') { throw 'Regular setup did not prefer the newest signed stable release' }

    $RequireAgentSetup = $true
    if ((Resolve-ReleaseVersion) -cne '0.2.0-rc.10') { throw 'Guided setup did not select the highest signed version' }

    $previousGhToken = $env:GH_TOKEN
    $previousGitHubToken = $env:GITHUB_TOKEN
    try {
        $discovery.Authorization.Clear()
        $env:GH_TOKEN = 'preferred'
        $env:GITHUB_TOKEN = 'fallback'
        $null = Resolve-ReleaseVersion
        $env:GH_TOKEN = ''
        $null = Resolve-ReleaseVersion
        if (($discovery.Authorization -join ',') -cne 'Bearer preferred,Bearer fallback') {
            throw 'Release discovery did not select the expected GitHub token'
        }

        $env:GITHUB_TOKEN = ''
        $discovery.Authorization.Clear()
        $discovery.Statuses.Enqueue(403)
        $discovery.Statuses.Enqueue(429)
        $discovery.ResponseHeaders = @{ 'Retry-After' = '75' }
        if ((Resolve-ReleaseVersion) -cne '0.2.0-rc.10' -or
            ($discovery.Delays -join ',') -cne '75,120' -or
            ($discovery.Authorization -join ',') -cne 'anonymous,anonymous,anonymous') {
            throw 'Anonymous release discovery did not retry with backoff'
        }

        $discovery.Delays.Clear()
        $discovery.ResponseHeaders = @{}
        foreach ($status in 403, 429, 429) { $discovery.Statuses.Enqueue($status) }
        $rateLimitMessage = $null
        try { $null = Resolve-ReleaseVersion } catch { $rateLimitMessage = $_.Exception.Message }
        if ($rateLimitMessage -cnotlike '*GitHub rate limit (HTTP 429)*BK_VERSION*' -or
            ($discovery.Delays -join ',') -cne '60,120') {
            throw 'Persistent rate limiting did not produce a bounded retry and a BK_VERSION hint'
        }

        $discovery.Delays.Clear()
        $discovery.Statuses.Enqueue(403)
        $discovery.ResponseHeaders = @{
            'X-RateLimit-Remaining' = '0'
            'X-RateLimit-Reset' = [string][DateTimeOffset]::UtcNow.AddMinutes(10).ToUnixTimeSeconds()
        }
        $rateLimitMessage = $null
        try { $null = Resolve-ReleaseVersion } catch { $rateLimitMessage = $_.Exception.Message }
        if ($rateLimitMessage -cnotlike '*GitHub rate limit (HTTP 403)*BK_VERSION*' -or $discovery.Delays.Count -ne 0) {
            throw 'Release discovery retried before the quota reset'
        }
    } finally {
        $env:GH_TOKEN = $previousGhToken
        $env:GITHUB_TOKEN = $previousGitHubToken
    }
}

Test-ReleaseDiscovery
Write-Host 'Windows installer release discovery tests passed'

function Test-AttestationFallback {
    $releaseVersion = '0.2.0-rc.10'
    $expectedRef = "refs/tags/beankeeper-desktop-v$releaseVersion"
    $archive = "bk-$releaseVersion-x86_64-pc-windows-msvc.zip"
    $directory = [IO.Path]::GetTempPath()
    . "$root/install.ps1" -Version $releaseVersion -InstallDir $directory -VerifyAttestation -RequireSignatures:$false
    $state = @{
        HasGh = $false
        FailOn = $null
        Checked = [System.Collections.Generic.List[string]]::new()
    }
    function Get-Command([string]$Name, [string]$ErrorAction) {
        if ($Name -eq 'gh' -and $state.HasGh) { return 'gh' }
        if ($Name -notin 'gh', 'cosign') { throw "Unexpected executable check: $Name" }
    }
    function gh {
        $artifact = Split-Path $args[2] -Leaf
        $expected = @(
            'attestation', 'verify', (Join-Path $directory $artifact), '--hostname', 'github.com',
            '--repo', 'Govcraft/beankeeper-desktop', '--cert-identity',
            "https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@$expectedRef",
            '--source-ref', $expectedRef
        )
        if (($args -join '|') -cne ($expected -join '|')) { throw 'Unexpected provenance verification arguments' }
        $state.Checked.Add($artifact)
        $global:LASTEXITCODE = if ($state.FailOn -ceq $artifact) { 1 } else { 0 }
    }
    function Assert-Failure([string]$Message) {
        try { Test-ReleaseSignatures 'unused' $archive $directory $releaseVersion }
        catch {
            if ($_.Exception.Message -notlike "*$Message*") { throw }
            return
        }
        throw "Expected failure: $Message"
    }

    Assert-Failure 'requires gh'
    $state.HasGh = $true
    $RequireSignatures = $true
    Assert-Failure 'cosign is required'
    $RequireSignatures = $false
    if ($state.Checked.Count -ne 0) { throw 'Strict signature mode attempted the provenance fallback' }

    Test-ReleaseSignatures 'unused' $archive $directory $releaseVersion
    if (($state.Checked -join ',') -cne "$archive,checksums-sha256.txt") { throw 'Expected both files to be attested' }
    $state.FailOn = 'checksums-sha256.txt'
    Assert-Failure 'GitHub build provenance verification failed for checksums-sha256.txt'
    $global:LASTEXITCODE = 0
}

Test-AttestationFallback
Write-Host 'Windows installer provenance tests passed'
if ($DiscoveryOnly) { return }

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
. "$root/install.ps1" -Version $releaseVersion -InstallDir $destination -NoService
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
    $original = (Get-FileHash -LiteralPath "$destination/bk.exe" -Algorithm SHA256).Hash
    $rejected = $false
    try { Install-Beankeeper } catch {
        if ($_.Exception.Message -notlike "*$Message*") { throw }
        $rejected = $true
    }
    if (-not $rejected) { throw "Expected failure: $Message" }
    if ((Get-FileHash -LiteralPath "$destination/bk.exe" -Algorithm SHA256).Hash -ne $original) {
        throw 'Replaced binary before verification'
    }
}
try {
    Install-Beankeeper # legacy bundle 404 fallback
    if (-not (Test-Path "$destination/bk.exe")) { throw 'Legacy install failed' }
    $RequireSignatures = $true
    Assert-Rejected 'Required Sigstore bundle missing'
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
    $script:bundleStatus = 200
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
