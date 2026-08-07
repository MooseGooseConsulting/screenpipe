#Requires -Version 7
<#
.SYNOPSIS
    Applies one source mutation, runs a scoped test selection, and reports
    whether the suite KILLED the mutation or SURVIVED it.

.DESCRIPTION
    A surviving mutation means the tests do not actually protect the behaviour
    they claim to. The file is always restored, including on failure or Ctrl+C.

    Mutations are described in a JSON manifest so a whole audit can be replayed:

    [
      {
        "id": "fingerprint-drops-green",
        "file": "crates/screenpipe-screen/src/types.rs",
        "find": "digest.update(&pixel[..3]);",
        "replace": "digest.update(&[pixel[0], pixel[2]]);",
        "package": "screenpipe-screen",
        "filter": "fingerprint",
        "claim": "the fingerprint digests every colour channel"
      }
    ]

    `find` must match exactly once, or the mutation is reported INVALID rather
    than silently applied to the wrong place.

.EXAMPLE
    doppler run -- pwsh -File scripts/Test-Mutation.ps1 -Manifest scripts/mutations/audit.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Manifest,
    [string]$Id,
    [switch]$FailOnSurvivor
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$mutations = @(Get-Content -LiteralPath $Manifest -Raw | ConvertFrom-Json)
if ($Id) { $mutations = @($mutations | Where-Object { $_.id -eq $Id }) }
if ($mutations.Count -eq 0) { throw "no mutations selected from $Manifest" }

$targets = @($mutations | ForEach-Object { $_.file } | Sort-Object -Unique)

# Restoration is by exact content, held in memory and rewritten in a `finally`.
# That covers exceptions but not a hard kill, so each original is also written
# to a sidecar under .mutation-backup first. An orphaned sidecar means a
# previous run died mid-mutation and the tree is still poisoned - refuse to do
# anything until it is reconciled, because every verdict after that is garbage.
$backupRoot = Join-Path $repoRoot '.mutation-backup'
if (Test-Path $backupRoot) {
    $orphans = @(Get-ChildItem $backupRoot -File -Recurse -ErrorAction SilentlyContinue)
    if ($orphans.Count -gt 0) {
        throw ("a previous mutation run left {0} unrestored file(s) in {1}. The working tree may still contain a mutation. Restore each one over its source path, then delete the directory." -f $orphans.Count, $backupRoot)
    }
}
New-Item -ItemType Directory -Force -Path $backupRoot | Out-Null

# Pristine content captured once, up front, so the final check compares against
# what we actually started with rather than against whatever the last iteration
# happened to write back.
$originals = @{}
foreach ($target in $targets) {
    $targetPath = Join-Path $repoRoot $target
    if (-not (Test-Path -LiteralPath $targetPath)) { throw "missing mutation target: $target" }
    $originals[$target] = [IO.File]::ReadAllText($targetPath)
}

function Save-MutationBackup {
    param([string]$RelativePath, [string]$Content)
    $destination = Join-Path $backupRoot ($RelativePath -replace '[\\/]', '__')
    [IO.File]::WriteAllText($destination, $Content)
    return $destination
}

$results = [System.Collections.Generic.List[object]]::new()

foreach ($mutation in $mutations) {
    $path = Join-Path $repoRoot $mutation.file
    if (-not (Test-Path -LiteralPath $path)) { throw "mutation $($mutation.id): missing file $($mutation.file)" }

    $original = [IO.File]::ReadAllText($path)

    # Match the file's own line endings.
    #
    # JSON carries `\n`, and `[IO.File]::ReadAllText` returns the file's bytes
    # verbatim - no universal-newline translation. So every multi-line `find`
    # silently matched ZERO times in a CRLF source file and was filed INVALID,
    # while the identical string matched fine in an LF file. The verdict looked
    # like a stale manifest entry rather than a newline mismatch, and this
    # repository contains both kinds of file.
    $find = $mutation.find
    $replace = $mutation.replace
    if ($original.Contains("`r`n")) {
        $find = $find -replace "(?<!`r)`n", "`r`n"
        $replace = $replace -replace "(?<!`r)`n", "`r`n"
    }

    $occurrences = ([regex]::Escape($find) | ForEach-Object { [regex]::Matches($original, $_).Count })
    if ($occurrences -ne 1) {
        Write-Host "INVALID  $($mutation.id) - 'find' matched $occurrences times (need exactly 1)" -ForegroundColor Magenta
        $results.Add([pscustomobject]@{ id = $mutation.id; verdict = 'INVALID'; claim = $mutation.claim })
        continue
    }

    $mutated = $original.Replace($find, $replace)
    if ($mutated -eq $original) { throw "mutation $($mutation.id): replacement was a no-op" }

    $backup = Save-MutationBackup -RelativePath $mutation.file -Content $original
    try {
        [IO.File]::WriteAllText($path, $mutated)

        # StrictMode makes an absent JSON key throw on property access, so probe
        # for presence rather than truthiness.
        $has = { param($name) $mutation.PSObject.Properties.Name -contains $name }

        $cargoArgs = @('test')
        if ((& $has 'package')) { $cargoArgs += @('-p', $mutation.package) }
        if ((& $has 'test')) { $cargoArgs += @('--test', $mutation.test) }
        $cargoArgs += @('--quiet')
        if ((& $has 'filter')) { $cargoArgs += @('--', $mutation.filter) }

        $output = & cargo @cargoArgs 2>&1
        $exit = $LASTEXITCODE

        # A mutation that does not compile is not evidence about the tests.
        # Match only genuine compiler diagnostics: cargo also prints a bare
        # "error: test failed" line for an ordinary failing test, which is a
        # KILL, not a build problem.
        # A const-evaluation panic is the STRONGEST possible kill, not an
        # invalid mutation.
        #
        # Invariants over constants - the idle-gap margin, the
        # consecutive-failure bounds - are asserted in `const` blocks precisely
        # so that violating them fails `cargo build` rather than only
        # `cargo test`. When such a mutation lands, rustc reports
        # `error[E0080]: evaluation panicked: <the assertion message>`. Matching
        # that as "does not compile" filed the loudest result the suite can
        # produce under "no evidence either way", and
        # `idle-gap-margin-collapses-to-one-second` was reported UNAUDITED when
        # the test had in fact stopped the build.
        $constAssertionFired = @($output | Where-Object {
                $_ -match 'error\[E0080\]' -and $_ -match 'evaluation panicked'
            }).Count -gt 0

        $compileFailed = -not $constAssertionFired -and @($output | Where-Object {
                $_ -match 'error\[E\d+\]' -or $_ -match 'could not compile'
            }).Count -gt 0

        # A KILL requires POSITIVE evidence that tests actually ran.
        #
        # Without this floor, any cargo invocation that fails before reaching a
        # test reports KILLED. Rename a package or delete a `--test` target and
        # cargo prints "error: package ID specification ... did not match any
        # packages" or "error: no test target named ...", exits nonzero, and
        # matches neither compiler pattern - so every entry in the manifest goes
        # green while zero tests executed. That is strictly worse than a false
        # SURVIVED: a survivor prompts investigation, a kill is believed.
        $testsRan = @($output | Where-Object {
                $_ -match 'running \d+ test' -or $_ -match '^test result:'
            }).Count -gt 0

        if ($constAssertionFired) {
            # Killed at compile time. Nothing needed to run.
            $verdict = 'KILLED'
            $colour = 'Green'
        } elseif ($compileFailed) {
            $verdict = 'UNCOMPILABLE'
            $colour = 'Magenta'
        } elseif ($exit -eq 0) {
            $verdict = 'SURVIVED'
            $colour = 'Red'
        } elseif (-not $testsRan) {
            $verdict = 'HARNESS-ERROR'
            $colour = 'Magenta'
        } else {
            $verdict = 'KILLED'
            $colour = 'Green'
        }

        Write-Host ("{0,-13} {1}" -f $verdict, $mutation.id) -ForegroundColor $colour
        if ($verdict -eq 'SURVIVED') {
            Write-Host "              claim not protected: $($mutation.claim)" -ForegroundColor DarkYellow
        }
        if ($verdict -eq 'UNCOMPILABLE') {
            # An uncompilable mutation is not evidence either way, so surface the
            # reason - usually the mutation needs reshaping to stay type-correct.
            $firstErrors = @($output | Where-Object { $_ -match '^error' } | Select-Object -First 3)
            foreach ($line in $firstErrors) { Write-Host "              $line" -ForegroundColor DarkGray }
        }
        $results.Add([pscustomobject]@{ id = $mutation.id; verdict = $verdict; claim = $mutation.claim })
    }
    finally {
        [IO.File]::WriteAllText($path, $original)
        Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    }
}

# Prove every mutated file is byte-identical to what we found before reporting.
foreach ($target in $targets) {
    $path = Join-Path $repoRoot $target
    $expected = $originals[$target]
    if ([IO.File]::ReadAllText($path) -ne $expected) {
        throw "FATAL: $target was not restored to its pre-mutation content"
    }
}
Remove-Item -LiteralPath $backupRoot -Recurse -Force -ErrorAction SilentlyContinue

$survived = @($results | Where-Object { $_.verdict -eq 'SURVIVED' })
$killed = @($results | Where-Object { $_.verdict -eq 'KILLED' })
$invalid = @($results | Where-Object { $_.verdict -in @('INVALID', 'UNCOMPILABLE', 'HARNESS-ERROR') })

Write-Host ''
Write-Host "killed=$($killed.Count) survived=$($survived.Count) invalid=$($invalid.Count)"
if ($survived.Count -gt 0) {
    Write-Host 'SURVIVING MUTATIONS (these tests are lies):' -ForegroundColor Red
    foreach ($s in $survived) { Write-Host "  - $($s.id): $($s.claim)" -ForegroundColor Red }
}
if ($invalid.Count -gt 0) {
    Write-Host 'UNAUDITED MUTATIONS (no evidence was produced):' -ForegroundColor Magenta
    foreach ($i in $invalid) { Write-Host "  - $($i.id) [$($i.verdict)]: $($i.claim)" -ForegroundColor Magenta }
}

# INVALID counts as failure, not as a pass.
#
# Gating on survivors alone meant a manifest whose `find` string no longer
# matched the source exited 0 - a green audit that tested nothing. That was not
# hypothetical: `wrapper-path-apostrophe-escaping-deleted` had lost a Rust
# escape, matched zero times, and its `.Replace()` was a verified no-op, so the
# apostrophe-escaping claim was reported as audited while the source was never
# touched.
if ($FailOnSurvivor -and ($survived.Count + $invalid.Count) -gt 0) { exit 1 }
exit 0
