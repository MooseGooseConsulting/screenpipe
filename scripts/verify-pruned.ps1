[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$failures = [System.Collections.Generic.List[string]]::new()

$forbiddenPaths = @(
    'ee',
    'apps',
    'packages'
)

foreach ($relativePath in $forbiddenPaths) {
    if (Test-Path -LiteralPath (Join-Path $root $relativePath)) {
        $failures.Add("tracked surface remains: $relativePath")
    }
}

if ($failures.Count -eq 0) {
    $metadataJson = & cargo metadata `
        --format-version 1 `
        --manifest-path (Join-Path $root 'Cargo.toml') `
        --no-deps
    if ($LASTEXITCODE -ne 0) {
        $failures.Add('cargo metadata failed')
    }
    else {
        $metadata = $metadataJson | ConvertFrom-Json
        $actualWorkspacePackages = @($metadata.workspace_members | ForEach-Object {
            ($_ -split '#')[0] -replace '^path\+file:///', ''
        })
        $expectedPackageNames = @(
            'screenpipe-audio',
            'screenpipe-cli',
            'screenpipe-memory',
            'screenpipe-screen'
        )
        $actualPackageNames = @(
            $metadata.packages |
                Where-Object { $_.id -in $metadata.workspace_members } |
                Select-Object -ExpandProperty name |
                Sort-Object
        )
        if (($actualPackageNames -join ',') -ne ($expectedPackageNames -join ',')) {
            $failures.Add("unexpected workspace packages: $($actualPackageNames -join ', ')")
        }

        $expectedDefaultPackageNames = @(
            'screenpipe-cli',
            'screenpipe-memory',
            'screenpipe-screen'
        )
        $actualDefaultPackageNames = @(
            $metadata.packages |
                Where-Object { $_.id -in $metadata.workspace_default_members } |
                Select-Object -ExpandProperty name |
                Sort-Object
        )
        if (($actualDefaultPackageNames -join ',') -ne ($expectedDefaultPackageNames -join ',')) {
            $failures.Add(
                "unexpected default workspace packages: $($actualDefaultPackageNames -join ', ')"
            )
        }

        $forbiddenDependencies = @(
            'screenpipe-db',
            'screenpipe-sync',
            'tauri'
        )
        $dependencyNames = @($metadata.packages.dependencies.name | Sort-Object -Unique)
        foreach ($dependency in $forbiddenDependencies) {
            if ($dependencyNames -contains $dependency) {
                $failures.Add("forbidden dependency remains: $dependency")
            }
        }
    }
}

if ($failures.Count -gt 0) {
    foreach ($failure in $failures) {
        Write-Error $failure -ErrorAction Continue
    }
    exit 1
}

Write-Output 'PASS: Goal 1 workspace members include screenpipe-audio; default members are the non-audio screenpipe-screen, screenpipe-memory, and screenpipe-cli packages'
