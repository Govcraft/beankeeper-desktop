# Run the documented Windows guide against fixture downloads and the native service.
[CmdletBinding()]
param([string]$ArtifactDirectory)

$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_OS -ne 'Windows') {
    throw 'The skill install check requires a disposable Windows GitHub Actions runner.'
}

$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $ArtifactDirectory) { $ArtifactDirectory = Join-Path $root 'dist' }
$versionText = & python "$root/scripts/release.py" version
if ($LASTEXITCODE -ne 0) { throw 'Could not read the built release version.' }
$version = $versionText.Trim()
$archive = "bk-$version-x86_64-pc-windows-msvc.zip"
$releaseUrl = "https://github.com/Govcraft/beankeeper-desktop/releases/download/beankeeper-desktop-v$version"
$skillExpectedIdentity = "https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v$version"
$installerUrl = 'https://raw.githubusercontent.com/Govcraft/beankeeper-desktop/main/install.ps1'
$guide = [regex]::Match((Get-Content -LiteralPath "$root/SKILL.md" -Raw), '(?s)```powershell\r?\n(.*?)```')
if (-not $guide.Success) { throw 'Missing the Windows installation block in SKILL.md.' }

# The native scheduler identifies the service by the logged-in user's SID.
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
if (Get-ScheduledTask -TaskName "Beankeeper-Desktop-$sid" -ErrorAction SilentlyContinue) {
    throw 'Remove the pre-existing fixture service before running this check.'
}

$sandbox = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
$fixtures = Join-Path $sandbox 'fixtures'
$userDirectory = Join-Path $sandbox 'user'
$localDirectory = Join-Path $sandbox 'local'
$roamingDirectory = Join-Path $sandbox 'roaming'
$codexDirectory = Join-Path $userDirectory '.codex'
$books = Join-Path $sandbox 'data\books.db'
$binary = Join-Path $localDirectory 'Programs\Beankeeper Desktop\bk.exe'
$environment = @{
    BK_VERSION = $null
    BEANKEEPER_DB = $null
    BEANKEEPER_PASSPHRASE_CMD = $null
    BK_AGENT_TOKEN = $null
    BEANKEEPER_NO_UPDATE_CHECK = '1'
    BEANKEEPER_SERVICE_DATA_DIR = Join-Path $sandbox 'data'
    BEANKEEPER_DESKTOP_STATE_DIR = Join-Path $sandbox 'state'
    BEANKEEPER_SERVICE_LOG_DIR = Join-Path $sandbox 'logs'
    USERPROFILE = $userDirectory
    WINDOWS_USERPROFILE = $userDirectory
    LOCALAPPDATA = $localDirectory
    APPDATA = $roamingDirectory
    CLAUDE_CONFIG_DIR = Join-Path $userDirectory '.claude'
    CODEX_HOME = $codexDirectory
    COPILOT_HOME = Join-Path $userDirectory '.copilot'
    GROK_HOME = Join-Path $userDirectory '.grok'
}
$previous = @{}
$fixtureFiles = @($archive, 'checksums-sha256.txt', "$archive.sigstore", 'checksums-sha256.txt.sigstore')
$verified = [System.Collections.Generic.List[string]]::new()
$guidePassed = $false

function Set-FixtureEnvironment([string]$Key, $Value) {
    if ($null -eq $Value) {
        Remove-Item -LiteralPath "Env:$Key" -ErrorAction SilentlyContinue
    } else {
        Set-Item -LiteralPath "Env:$Key" -Value $Value
    }
}

function Invoke-WebRequest {
    param([switch]$UseBasicParsing, [uri]$Uri, [string]$OutFile)
    if ($Uri.AbsoluteUri -eq $installerUrl) {
        Copy-Item -LiteralPath "$root/install.ps1" -Destination $OutFile
        return
    }
    if (-not $Uri.AbsoluteUri.StartsWith("$releaseUrl/", [StringComparison]::Ordinal)) {
        throw "Unexpected fixture download: $Uri"
    }
    $name = $Uri.AbsoluteUri.Substring($releaseUrl.Length + 1)
    if ($fixtureFiles -notcontains $name) { throw "Unexpected fixture file: $name" }
    Copy-Item -LiteralPath (Join-Path $fixtures $name) -Destination $OutFile
}

function Invoke-RestMethod([string]$Uri) {
    if ($Uri -ne 'https://api.github.com/repos/Govcraft/beankeeper-desktop/releases?per_page=100') {
        throw "Unexpected discovery request: $Uri"
    }
    @(
        [pscustomobject]@{ tag_name = 'beankeeper-desktop-v99.0.0'; draft = $true; prerelease = $false },
        [pscustomobject]@{ tag_name = 'beankeeper-desktop-v98.0.0'; draft = $false; prerelease = $false; assets = @() },
        [pscustomobject]@{
            tag_name = "beankeeper-desktop-v$version"
            draft = $false
            prerelease = $version.Contains('-rc.')
            assets = @($fixtureFiles | ForEach-Object { @{ name = $_ } })
        }
    )
}

function cosign {
    if ($args[0] -ne 'verify-blob' -or $args -notcontains '--certificate-identity' -or
        $args -notcontains $skillExpectedIdentity -or $args -notcontains '--certificate-oidc-issuer' -or
        $args -notcontains 'https://token.actions.githubusercontent.com') {
        throw 'The installer did not verify the expected Govcraft release identity.'
    }
    $name = Split-Path $args[-1] -Leaf
    $bundle = [array]::IndexOf($args, '--bundle')
    if ($bundle -lt 0 -or (Split-Path $args[$bundle + 1] -Leaf) -ne "$name.sigstore") {
        throw 'The installer did not verify the matching signature bundle.'
    }
    $verified.Add($name)
    $global:LASTEXITCODE = 0
}

try {
    New-Item -ItemType Directory -Force -Path $fixtures, $codexDirectory, $localDirectory, $roamingDirectory | Out-Null
    $archivePath = Join-Path $fixtures $archive
    Copy-Item -LiteralPath (Join-Path $ArtifactDirectory $archive) -Destination $archivePath
    $digest = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
    "$digest  $archive" | Set-Content -LiteralPath (Join-Path $fixtures 'checksums-sha256.txt')
    foreach ($name in @("$archive.sigstore", 'checksums-sha256.txt.sigstore')) {
        'fixture bundle' | Set-Content -LiteralPath (Join-Path $fixtures $name)
    }
    foreach ($key in $environment.Keys) {
        $previous[$key] = [Environment]::GetEnvironmentVariable($key)
        Set-FixtureEnvironment $key $environment[$key]
    }

    $null = & ([ScriptBlock]::Create($guide.Groups[1].Value))
    if ($verified.Count -ne 2 -or $verified -notcontains $archive -or
        $verified -notcontains 'checksums-sha256.txt') {
        throw 'The skill block did not verify both the archive and checksum fixtures.'
    }
    $statusText = & $binary service status --json
    if ($LASTEXITCODE -ne 0) { throw 'Could not read service status after the guide ran.' }
    $status = $statusText | ConvertFrom-Json
    # Rust canonicalization may return Windows's extended-length path spelling.
    $reportedBooks = [string]$status.books
    if ($reportedBooks.StartsWith('\\?\')) { $reportedBooks = $reportedBooks.Substring(4) }
    if (-not $status.installed -or -not $status.running -or
        $status.version -ne $version -or $reportedBooks -ne $books) {
        throw 'The skill block did not install and start its isolated service.'
    }
    if (-not (Test-Path -LiteralPath ([IO.Path]::ChangeExtension($books, 'recovery.txt')))) {
        throw 'First-run setup did not create the owner recovery kit.'
    }
    $doctorText = & $binary agents doctor codex --json
    if ($LASTEXITCODE -ne 0) { throw 'The installed Codex connection did not pass its MCP check.' }
    $doctor = $doctorText | ConvertFrom-Json
    if ($doctor.data.agents[0].state -ne 'configured' -or $doctor.data.service.state -ne 'ready') {
        throw 'The skill block did not configure a usable agent connection.'
    }
    $guidePassed = $true
    Write-Host 'Windows skill guide installed its fixture release and passed the native MCP check.'
} finally {
    try {
        if (Test-Path -LiteralPath $binary) {
            try {
                $null = & $binary service uninstall
                if ($LASTEXITCODE -ne 0) { throw 'Could not remove the fixture service.' }
            } catch {
                if ($guidePassed) { throw }
                Write-Warning 'Could not remove the fixture service after the guide failed.'
            }
        }
    } finally {
        foreach ($key in $previous.Keys) {
            Set-FixtureEnvironment $key $previous[$key]
        }
        Remove-Item -LiteralPath $sandbox -Force -Recurse -ErrorAction SilentlyContinue
    }
}
