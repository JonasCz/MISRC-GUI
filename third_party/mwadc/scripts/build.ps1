<#
.SYNOPSIS
  Build the mwadc CLI on Windows and stage it with the Magewell SDK DLL in dist\.

.DESCRIPTION
  Needs the Rust MSVC toolchain (rustup default stable-x86_64-pc-windows-msvc, which needs the
  "Desktop development with C++" build tools). Nothing from the Magewell SDK is needed at build
  time: LibMWCapture.dll is loaded at run time. This script only copies it next to mwadc.exe
  if it can find one.

.PARAMETER SdkDll
  Path to LibMWCapture.dll (x64). Default: search the Magewell SDK/runtime install locations.

.PARAMETER Debug
  Build the debug profile instead of release.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build.ps1
  powershell -ExecutionPolicy Bypass -File scripts\build.ps1 -SdkDll "C:\path\to\LibMWCapture.dll"
#>
param(
    [string]$SdkDll = "",
    [switch]$Debug
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo not found. Install Rust from https://rustup.rs (MSVC toolchain)."
}

$profileDir = if ($Debug) { "debug" } else { "release" }
# The CLI only. mwadc_ffi.dll (C API) is built by MISRC-GUI's meson build (scripts\meson-cargo.py).
$cargoArgs = @("build", "--locked", "--package", "mwadc", "--target", "x86_64-pc-windows-msvc")
if (-not $Debug) { $cargoArgs += "--release" }
Write-Host "cargo $($cargoArgs -join ' ')"
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$exe = Join-Path $root "target\x86_64-pc-windows-msvc\$profileDir\mwadc.exe"
Copy-Item $exe $dist -Force
$pdb = [IO.Path]::ChangeExtension($exe, ".pdb")
if (Test-Path $pdb) { Copy-Item $pdb $dist -Force }

# Find the release SDK DLL. The runtime installer (MWCaptureRT.exe) puts it in System32;
# the SDK ships a copy with its Python example; the SDK installer itself only installs the
# debug LibMWCaptured.dll.
if (-not $SdkDll) {
    $candidates = @(
        "$env:WINDIR\System32\LibMWCapture.dll",
        "${env:ProgramFiles(x86)}\Magewell\*\SDKv3\Examples\Python\AVCapturePY\mwcapture\bin\x64\LibMWCapture.dll",
        "$env:ProgramFiles\Magewell\*\SDKv3\Examples\Python\AVCapturePY\mwcapture\bin\x64\LibMWCapture.dll"
    )
    foreach ($c in $candidates) {
        $hit = Get-ChildItem -Path $c -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($hit) { $SdkDll = $hit.FullName; break }
    }
    if (-not $SdkDll) {
        foreach ($base in @("${env:ProgramFiles(x86)}\Magewell", "$env:ProgramFiles\Magewell")) {
            $hit = Get-ChildItem -Path $base -Recurse -Filter LibMWCapture.dll -ErrorAction SilentlyContinue |
                Where-Object { $_.FullName -match "x64" } | Select-Object -First 1
            if ($hit) { $SdkDll = $hit.FullName; break }
        }
    }
}
if ($SdkDll -and (Test-Path $SdkDll)) {
    Copy-Item $SdkDll $dist -Force
    Write-Host "staged SDK DLL: $SdkDll"
} else {
    Write-Warning ("LibMWCapture.dll not found. Download and install MWCaptureRT.exe from https://github.com/jonasCz/mwadc/releases, " +
        "pass -SdkDll, or run mwadc with --dll <path>.")
}

Write-Host ""
Write-Host "built $dist\mwadc.exe"
Write-Host "first run:  $dist\mwadc.exe list"
Write-Host "then:       $dist\mwadc.exe -b 0 capture   (see docs\12-mwadc.md)"
