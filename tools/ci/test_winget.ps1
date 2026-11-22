$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$manifestRoot = Join-Path $PSScriptRoot '../../packaging/winget'
$versions = @(Get-ChildItem -Directory -LiteralPath $manifestRoot | Sort-Object { [version]$_.Name })
$current = $versions[-1]
$testRoot = New-Item -ItemType Directory -Path (Join-Path $env:RUNNER_TEMP 'nao-winget')
$root = $testRoot.FullName

if ($versions.Count -gt 1) {
    $previous = $versions[-2].FullName
    $previousVersion = "nao $($versions[-2].Name)"
} else {
    # Exercise an in-place upgrade before there is a previous community catalog entry.
    # Use the exact published 0.1.5 binary under the same package ID and command alias.
    $previous = (New-Item -ItemType Directory -Path (Join-Path $root '0.1.5')).FullName
    $previousVersion = 'naome-protocol 0.1.5'
    Get-ChildItem -File -LiteralPath $current.FullName -Filter '*.yaml' | ForEach-Object {
        $manifest = (Get-Content -Raw -LiteralPath $_.FullName).
            Replace('0.1.6', '0.1.5').
            Replace('/nao-v0.1.5-', '/rep-v0.1.5-').
            Replace('RelativeFilePath: nao.exe', 'RelativeFilePath: rep.exe').
            Replace(
                '85C36A43C027EA7C9D30506A973064E7A0A9FC0F9CC7B7115E09E642EAB12F73',
                '9B44A148BF69915DDD1AFBF1754148C418D596AA0CDBE3EAC4D59D55450CF73A'
            )
        Set-Content -LiteralPath (Join-Path $previous $_.Name) -Value $manifest
    }
}

function Assert-NaoVersion([string]$Expected) {
    $actual = nao --version
    if ($actual -ne $Expected) {
        throw "Expected '$Expected' from the installed command, got '$actual'."
    }
    Write-Host $actual
}

function Update-ProcessPath {
    $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
        [Environment]::GetEnvironmentVariable('Path', 'User')
}

$installOptions = @('--silent', '--disable-interactivity', '--accept-package-agreements', '--accept-source-agreements')
winget --info
winget validate --manifest $previous
winget validate --manifest $current.FullName
winget settings --enable LocalManifestFiles

Push-Location $root
try {
    Write-Host 'Installing the previous version'
    winget install --manifest $previous --scope user @installOptions
    Update-ProcessPath
    Assert-NaoVersion $previousVersion
    Set-Content -LiteralPath 'reading.txt' -Value 'wingetcompatibility' -NoNewline
    $readingId = nao capture reading.txt text/plain | ConvertFrom-Json
    nao snapshot winget-check

    Write-Host 'Upgrading the installed package'
    winget upgrade --manifest $current.FullName @installOptions
    Update-ProcessPath
    Assert-NaoVersion "nao $($current.Name)"
    $report = (nao find wingetcompatibility | Out-String) | ConvertFrom-Json
    if ($readingId -notin $report.hits.id) {
        throw "The upgraded command did not find the captured evidence ($readingId)."
    }

    Write-Host 'Uninstalling to test a clean installation'
    winget uninstall --manifest $current.FullName --disable-interactivity --accept-source-agreements
    if (Get-Command nao -CommandType Application -ErrorAction SilentlyContinue) {
        throw "The 'nao' command still resolves after uninstalling NAOME.nao."
    }

    Write-Host 'Installing the current release into a clean environment'
    winget install --manifest $current.FullName --scope user @installOptions
    Update-ProcessPath
    Assert-NaoVersion "nao $($current.Name)"
} finally {
    Pop-Location
}
