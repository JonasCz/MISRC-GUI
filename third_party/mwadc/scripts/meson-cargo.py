#!/usr/bin/env python3
"""Build the mwadc workspace (core, CLI, C API) for the MISRC-GUI meson build.

Called by misrc_tools/meson.build as a custom_target:
    meson-cargo.py <cargo> <workspace dir> <cargo target dir> <profile> <output>...

Runs `cargo build --locked` for the crates behind the requested outputs only
(MISRC-GUI asks for mwadc_ffi.dll, so the CLI is not built; cargo decides what
is stale) and copies each output from the cargo profile directory to the given
path, next to the GUI binary. The matching .pdb, if any, is copied too.
"""
import os
import shutil
import subprocess
import sys

# output file name -> cargo package that produces it
PACKAGES = {
    "mwadc.exe": "mwadc",
    "mwadc_ffi.dll": "mwadc-ffi",
}


def main(argv):
    if len(argv) < 6:
        sys.stderr.write(__doc__)
        return 2
    cargo, workspace, target_dir, profile = argv[1:5]
    outputs = argv[5:]

    cmd = [cargo, "build", "--locked",
           "--manifest-path", os.path.join(workspace, "Cargo.toml"),
           "--target-dir", target_dir]
    for out in outputs:
        name = os.path.basename(out)
        if name not in PACKAGES:
            sys.stderr.write("meson-cargo.py: unknown output %r (known: %s)\n"
                             % (name, ", ".join(sorted(PACKAGES))))
            return 2
        cmd += ["--package", PACKAGES[name]]
    if profile == "release":
        cmd.append("--release")
    elif profile != "debug":
        sys.stderr.write("meson-cargo.py: profile must be release or debug, got %r\n" % profile)
        return 2
    print(" ".join(cmd), flush=True)
    ret = subprocess.call(cmd)
    if ret != 0:
        return ret

    built = os.path.join(target_dir, profile)
    for out in outputs:
        src = os.path.join(built, os.path.basename(out))
        if not os.path.isfile(src):
            sys.stderr.write("meson-cargo.py: cargo did not produce %s\n" % src)
            return 1
        copy_if_changed(src, out)
        # mwadc.exe -> mwadc.pdb, mwadc_ffi.dll -> mwadc_ffi.pdb (MSVC toolchain only)
        stem = os.path.splitext(os.path.basename(src))[0].replace("-", "_")
        pdb = os.path.join(built, stem + ".pdb")
        if os.path.isfile(pdb):
            copy_if_changed(pdb, os.path.join(os.path.dirname(out), os.path.basename(pdb)))
    return 0


def copy_if_changed(src, dst):
    """Copy unless dst is already this build (same size and mtime): a running GUI keeps
    mwadc_ffi.dll locked, which only matters when there is something new to copy."""
    try:
        s, d = os.stat(src), os.stat(dst)
        if s.st_size == d.st_size and int(s.st_mtime) == int(d.st_mtime):
            return
    except OSError:
        pass
    shutil.copy2(src, dst)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
