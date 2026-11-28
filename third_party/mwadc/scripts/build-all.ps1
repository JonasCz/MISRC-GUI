<#
.SYNOPSIS
  Build the mwadc CLI, copy mwadc into MISRC-GUI and build MISRC-GUI.

.DESCRIPTION
  1. scripts\build.ps1: dist\mwadc.exe (no MISRC-GUI dependency).
  2. Copy the working tree of this repository (tracked files plus new files git does not ignore, so
     uncommitted changes are included) into the MISRC-GUI checkout:
       misrc-gui\<path>  -> misrc_tools\misrc_gui\<path>   (the GUI's Magewell backend and settings panel)
       everything else   -> third_party\mwadc\<path>       (a mirror: files mwadc no longer has are deleted;
                                                            files MISRC-GUI's git ignores there are kept)
       docs\             -> not copied (the documentation exists only in this repository)
     AGENTS.md and CLAUDE.md are written into third_party\mwadc so that people and coding agents working
     in MISRC-GUI make these changes here instead. Nothing is committed in MISRC-GUI.

     Conflicts: the hash of every file as last copied is kept in .misrc-gui-sync.json (git-ignored). A
     file in MISRC-GUI that differs from both that copy and this repository's version was changed in
     MISRC-GUI (an upstream merge, the maintainer, an edit made there); so is a file that only exists
     in the mirror there. Then nothing is copied: the script shows the differences and stops. Bring
     those changes into this repository (once both sides match, the file is in sync again), or pass
     -Force to overwrite them.
  3. Build MISRC-GUI's misrc_gui target and mwadc_ffi.dll (meson -Dmwadc=enabled; no mwadc.exe) with the
     MSYS2 UCRT64 toolchain.

.PARAMETER MisrcGui
  MISRC-GUI checkout. Default: MISRC-GUI next to this repository.

.PARAMETER BuildDir
  Meson build directory, relative to MisrcGui. Configured with MesonArgs if it does not exist yet.

.PARAMETER MesonArgs
  Extra `meson setup` options for a new build directory (-Dmwadc=enabled is always added).

.PARAMETER Msys2
  MSYS2 root (its ucrt64\bin has meson, ninja and gcc).

.PARAMETER Force
  Copy even over files that were changed in MISRC-GUI (their changes are lost).

.PARAMETER SkipCli / SkipSync / SkipGui
  Leave out a step.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build-all.ps1
  powershell -ExecutionPolicy Bypass -File scripts\build-all.ps1 -SkipGui
#>
param(
    [string]$MisrcGui = "",
    [string]$BuildDir = ".deps\build-gui",
    [string[]]$MesonArgs = @("-Drtlsdr=disabled", "-Dmodern_monitor_codecs=disabled"),
    [string]$Msys2 = "C:\msys64",
    [switch]$Force,
    [switch]$SkipCli,
    [switch]$SkipSync,
    [switch]$SkipGui
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $MisrcGui) { $MisrcGui = Join-Path (Split-Path -Parent $root) "MISRC-GUI" }
$MisrcGui = (Resolve-Path $MisrcGui).Path
$mirrorRel = "third_party/mwadc"
$mirror = Join-Path $MisrcGui $mirrorRel
$guiPrefix = "misrc-gui/"                  # in this repository
$guiTarget = "misrc_tools/misrc_gui/"      # in MISRC-GUI
$stateFile = Join-Path $root ".misrc-gui-sync.json"
$upstreamUrl = "https://github.com/JonasCz/mwadc"

function Invoke-Checked([string]$exe, [string[]]$argv) {
    Write-Host "> $exe $($argv -join ' ')"
    & $exe @argv
    if ($LASTEXITCODE -ne 0) { throw "$exe failed (exit $LASTEXITCODE)" }
}

# git ls-files as a list of repo-relative paths with '/' separators
function Get-GitFiles([string]$repo, [string[]]$pathspec) {
    $out = & git -C $repo -c core.quotepath=off ls-files --cached --others --exclude-standard -z -- @pathspec
    if ($LASTEXITCODE -ne 0) { throw "git ls-files failed in $repo" }
    # deleted-but-tracked files are still listed by --cached; keep only what exists
    @(($out -join "") -split "`0" | Where-Object { $_ -and (Test-Path -LiteralPath (Join-Path $repo $_) -PathType Leaf) })
}

function Get-Hash([string]$path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }

# ---- 1. CLI -------------------------------------------------------------------------------------
if (-not $SkipCli) {
    Write-Host "== mwadc CLI"
    & (Join-Path $PSScriptRoot "build.ps1")
}

# ---- 2. copy into MISRC-GUI ---------------------------------------------------------------------
if (-not $SkipSync) {
    Write-Host "== copy $root -> $MisrcGui"
    if (-not (Test-Path (Join-Path $MisrcGui "misrc_tools\meson.build"))) {
        throw "$MisrcGui does not look like a MISRC-GUI checkout (no misrc_tools\meson.build)"
    }
    $generatedNames = @("AGENTS.md", "CLAUDE.md")
    $generated = @{}
    foreach ($g in $generatedNames) { $generated["$mirrorRel/$g"] = $true }

    # MISRC-GUI path (relative, '/') -> source file in this repository
    $pairs = [ordered]@{}
    foreach ($f in (Get-GitFiles $root @("."))) {
        if ($generatedNames -contains $f) { throw "$f exists in mwadc; it is generated in the mirror only" }
        if ($f.StartsWith("docs/")) { continue }   # docs live in mwadc only
        if ($f.StartsWith($guiPrefix)) { $dst = $guiTarget + $f.Substring($guiPrefix.Length) }
        else { $dst = "$mirrorRel/$f" }
        $pairs[$dst] = Join-Path $root $f
    }

    # hashes as last copied into this checkout
    $allState = @{}
    if (Test-Path $stateFile) {
        $json = Get-Content -Raw $stateFile | ConvertFrom-Json
        foreach ($co in $json.PSObject.Properties) {
            $h = @{}
            foreach ($p in $co.Value.PSObject.Properties) { $h[$p.Name] = $p.Value }
            $allState[$co.Name] = $h
        }
    }
    $stateKey = $MisrcGui.ToLowerInvariant()
    $state = if ($allState.ContainsKey($stateKey)) { $allState[$stateKey] } else { @{} }

    # files to delete: in the mirror but no longer here, or copied before and no longer here
    $deletes = @{}
    if (Test-Path $mirror) {
        foreach ($f in (Get-GitFiles $MisrcGui @($mirrorRel))) {
            if (-not $pairs.Contains($f) -and -not $generated.ContainsKey($f)) { $deletes[$f] = $true }
        }
    }
    foreach ($f in $state.Keys) {
        if (-not $pairs.Contains($f) -and (Test-Path -LiteralPath (Join-Path $MisrcGui $f))) { $deletes[$f] = $true }
    }

    # check everything before writing anything
    $copies = @(); $conflicts = @()
    foreach ($dst in $pairs.Keys) {
        $src = $pairs[$dst]
        $to = Join-Path $MisrcGui $dst
        if (-not (Test-Path -LiteralPath $to)) { $copies += $dst; continue }
        $hd = Get-Hash $to
        if ($hd -eq (Get-Hash $src)) { continue }
        if ($state[$dst] -eq $hd -or $Force) { $copies += $dst }
        else { $conflicts += [pscustomobject]@{ Path = $dst; Src = $src; Kind = "changed in MISRC-GUI" } }
    }
    foreach ($dst in @($deletes.Keys | Sort-Object)) {
        $hd = Get-Hash (Join-Path $MisrcGui $dst)
        if (-not ($state[$dst] -eq $hd -or $Force)) {
            $kind = if ($state.ContainsKey($dst)) { "changed in MISRC-GUI, removed in mwadc" } else { "only in MISRC-GUI" }
            $conflicts += [pscustomobject]@{ Path = $dst; Src = $null; Kind = $kind }
            $deletes.Remove($dst)
        }
    }
    if ($conflicts.Count -gt 0) {
        Write-Host ""
        Write-Host "These files were changed in MISRC-GUI since the last copy (or never came from mwadc):" -ForegroundColor Red
        foreach ($c in $conflicts) {
            Write-Host "  $($c.Path)  ($($c.Kind))" -ForegroundColor Red
            if ($c.Src) {
                # - this repository, + MISRC-GUI
                & git --no-pager diff --no-index --stat --patch -- $c.Src (Join-Path $MisrcGui $c.Path) | Out-Host
            }
        }
        throw ("$($conflicts.Count) conflict(s), nothing copied. Bring the MISRC-GUI changes into mwadc " +
               "(diffs above: - mwadc, + MISRC-GUI) and run again, or pass -Force to overwrite them.")
    }

    # copy (byte for byte, so LF line endings stay LF) and delete
    foreach ($dst in $copies) {
        $to = Join-Path $MisrcGui $dst
        $dir = Split-Path -Parent $to
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
        Copy-Item -LiteralPath $pairs[$dst] -Destination $to -Force
    }
    foreach ($dst in $deletes.Keys) { Remove-Item -LiteralPath (Join-Path $MisrcGui $dst) -Force }
    if (Test-Path $mirror) {
        # drop directories left empty in the mirror (deepest first)
        Get-ChildItem $mirror -Recurse -Directory | Sort-Object { $_.FullName.Length } -Descending |
            Where-Object { -not (Get-ChildItem -LiteralPath $_.FullName -Force) } |
            ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force }
    }

    $rev = (& git -C $root rev-parse --short HEAD).Trim()
    $dirty = if (& git -C $root status --porcelain) { " plus uncommitted changes" } else { "" }
    $note = @"
# third_party/mwadc is a mirror: do not edit it here

Everything in this directory is copied from the standalone mwadc repository,
$upstreamUrl (commit $rev$dirty), by that repository's
``scripts/build-all.ps1``. So are the GUI's Magewell files, from its ``misrc-gui/`` folder:
``misrc_tools/misrc_gui/input/gui_mwadc.c``, ``gui_mwadc.h`` (the capture backend) and
``misrc_tools/misrc_gui/ui/gui_ui_mwadc.c``, ``gui_ui_mwadc.h`` (the settings panel and its texts).

**Coding agents and contributors:** make changes to these files in $upstreamUrl,
then run its ``scripts/build-all.ps1`` to bring them here. The script refuses to copy over a file
that was changed here, so an edit made here has to be ported to mwadc first. If you only have this
checkout, describe the needed change instead of editing these files.

The rest of the integration belongs to MISRC-GUI and is edited here as usual: the ``ENABLE_MWADC``
hooks in other GUI files and the ``mwadc`` parts of ``misrc_tools/meson.build`` / ``meson_options.txt``.
"@
    $note = $note -replace "`r`n", "`n"
    $utf8 = New-Object System.Text.UTF8Encoding $false
    foreach ($g in $generatedNames) {
        [IO.File]::WriteAllText((Join-Path $mirror $g), $note + "`n", $utf8)
    }

    $newState = @{}
    foreach ($dst in $pairs.Keys) { $newState[$dst] = Get-Hash $pairs[$dst] }
    $allState[$stateKey] = $newState
    [IO.File]::WriteAllText($stateFile, ($allState | ConvertTo-Json -Depth 4) + "`n", $utf8)
    Write-Host ("copied $($pairs.Count) files ($($copies.Count) updated, $($deletes.Count) removed); " +
                "MISRC-GUI changes are not committed")
}

# ---- 3. MISRC-GUI (GUI + mwadc_ffi.dll, no mwadc CLI) -------------------------------------------
if (-not $SkipGui) {
    Write-Host "== MISRC-GUI"
    $ucrt = Join-Path $Msys2 "ucrt64\bin"
    if (-not (Test-Path (Join-Path $ucrt "meson.exe"))) {
        throw "meson not found in $ucrt (install MSYS2 UCRT64 with mingw-w64-ucrt-x86_64-meson, or pass -Msys2)"
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo not found; MISRC-GUI's meson build needs it for mwadc_ffi.dll"
    }
    $env:PATH = "$ucrt;$env:PATH"
    if (-not $env:HOME) { $env:HOME = $env:USERPROFILE }   # meson under MSYS2 python wants it
    Push-Location $MisrcGui
    try {
        if (-not (Test-Path (Join-Path $BuildDir "build.ninja"))) {
            Invoke-Checked "meson" (@("setup", $BuildDir, "misrc_tools", "-Dbuildtype=release", "-Dmwadc=enabled") + $MesonArgs)
        } else {
            Invoke-Checked "meson" @("configure", $BuildDir, "-Dmwadc=enabled")
        }
        # ninja rather than `meson compile <target>`: ninja regenerates build.ninja first when meson.build
        # changed, while meson compile resolves target names from the previous configuration.
        Invoke-Checked "ninja" @("-C", $BuildDir, "misrc_gui.exe", "mwadc_ffi.dll")
        $bin = (Resolve-Path $BuildDir).Path
        Write-Host ""
        Write-Host "built $bin\misrc_gui.exe and $bin\mwadc_ffi.dll (run them from the same folder)"
    } finally {
        Pop-Location
    }
}
