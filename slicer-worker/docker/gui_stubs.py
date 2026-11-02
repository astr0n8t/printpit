#!/usr/bin/env python3
"""Generate stand-ins for the GUI libraries the OrcaSlicer binary links but never calls when slicing.

usage: gui_stubs.py <orca-slicer binary> <bundled lib dir> <out dir> <target triple>

OrcaSlicer's Linux build links GTK3, WebKitGTK, GStreamer, Soup, libsecret, X11/Wayland, GL, glib
and dbus (~700 MB installed), but a CLI slice calls none of them (PLAN.md §15.10). Every library in
its DT_NEEDED list, except the C/C++ runtime and the ones Orca bundles, is replaced by a stub with the
same SONAME that defines every symbol the binary imports from it:

- functions trap (`__builtin_trap()`), so if a future Orca does reach GUI code, it crashes loudly
  (the golden test catches that) instead of silently doing nothing. No headers, no libc: a stub can't
  clash with a C declaration of the same name;
- data objects are zeroed buffers;
- versioned symbols (dbus, xkbcommon) are defined under their version node, so the loader finds
  the versions it expects and stays quiet.

Unversioned symbols can come from any library, so they all go into one shared stub library that
every per-library stub depends on. The binary must be lazily bound (no BIND_NOW), which is checked.
Runs on the build machine with the target's binutils and gcc (`<triple>-readelf`, `-nm`, `-gcc`).
"""

import os
import re
import subprocess
import sys

# The C and C++ runtime stays real: it's in the base image and everything uses it.
KEEP = re.compile(r"^(ld-linux.*|libc|libm|libdl|libpthread|librt|libresolv|libutil|libstdc\+\+|libgcc_s)\.so(\.\d+)*$")
SHARED_STUB = "libprintpit-gui-stub.so"
DATA_BYTES = 256


def run(*cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"{' '.join(cmd[:3])} ... failed ({r.returncode}):\n{r.stderr[-4000:]}")
    return r.stdout


def main():
    binary, bundled, out, triple = sys.argv[1:5]
    tool = lambda name: f"{triple}-{name}"

    dynamic = run(tool("readelf"), "-d", "-W", binary)
    if "BIND_NOW" in dynamic or re.search(r"FLAGS_1\).*\bNOW\b", dynamic):
        sys.exit("binary is bound immediately (BIND_NOW); stubs would need every symbol and can't be trusted")
    needed = re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", dynamic)
    bundled_libs = {f for f in os.listdir(bundled) if ".so" in f}
    stubbed = [lib for lib in needed if not KEEP.match(lib) and lib not in bundled_libs]

    # Version index -> library, from .gnu.version_r ("File: libdbus-1.so.3" then its "Name:" lines).
    version_lib = {}
    current = None
    for line in run(tool("readelf"), "-V", "-W", binary).splitlines():
        m = re.search(r"File: (\S+)", line)
        if m:
            current = m.group(1)
            continue
        m = re.search(r"Name: (\S+)\s+Flags: \S+\s+Version: \d+", line)
        if m and current:
            version_lib[m.group(1)] = current

    # What the bundled libraries really provide stays theirs.
    provided = set()
    for f in bundled_libs:
        path = os.path.join(bundled, f)
        if os.path.islink(path):
            continue
        for line in run(tool("nm"), "-D", "--defined-only", path).splitlines():
            parts = line.split()
            if len(parts) >= 3:
                provided.add(parts[2].split("@")[0])

    shared = {"func": set(), "data": set()}
    per_lib = {lib: {} for lib in stubbed}  # lib -> version -> {"func": set, "data": set}
    for line in run(tool("readelf"), "--dyn-syms", "-W", binary).splitlines():
        cols = line.split()
        if len(cols) < 8 or cols[6] != "UND" or cols[4] != "GLOBAL":
            continue
        kind = {"FUNC": "func", "OBJECT": "data", "NOTYPE": "func", "TLS": "tls"}.get(cols[3])
        name = cols[7]
        if "@" in name:
            sym, version = name.split("@", 1)
            lib = version_lib.get(version)
            if lib not in per_lib:
                continue  # the C/C++ runtime or a bundled library: real
            if kind == "tls":
                sys.exit(f"thread-local {sym}@{version} from {lib} can't be stubbed")
            per_lib[lib].setdefault(version, {"func": set(), "data": set()})[kind].add(sym)
        else:
            if name in provided or not kind:
                continue
            if kind == "tls":
                sys.exit(f"unversioned thread-local {name} can't be stubbed")
            shared[kind].add(name)

    os.makedirs(out, exist_ok=True)
    cc = tool("gcc")

    def source(funcs, datas):
        lines = []
        lines += [f"char {d}[{DATA_BYTES}] __attribute__((aligned(16)));" for d in sorted(datas)]
        lines += [f"void {f}(void) {{ __builtin_trap(); }}" for f in sorted(funcs)]
        return "\n".join(lines) + "\n"

    src = os.path.join(out, "stub.c")
    with open(src, "w") as fh:
        fh.write(source(shared["func"], shared["data"]))
    run(cc, "-shared", "-fPIC", "-O1", "-w", "-nostdlib", "-fno-builtin", f"-Wl,-soname,{SHARED_STUB}", "-o", os.path.join(out, SHARED_STUB), src)

    for lib in stubbed:
        versions = per_lib[lib]
        c = os.path.join(out, "lib.c")
        with open(c, "w") as fh:
            funcs = set().union(*(v["func"] for v in versions.values()))
            datas = set().union(*(v["data"] for v in versions.values()))
            fh.write(source(funcs, datas))
        args = [cc, "-shared", "-fPIC", "-O1", "-w", "-nostdlib", "-fno-builtin", f"-Wl,-soname,{lib}", "-Wl,--no-as-needed",
                "-o", os.path.join(out, lib), c, f"-L{out}", f"-l:{SHARED_STUB}"]
        if versions:
            script = os.path.join(out, "lib.map")
            with open(script, "w") as fh:
                for i, (version, syms) in enumerate(sorted(versions.items())):
                    names = sorted(syms["func"] | syms["data"])
                    local = " local: *;" if i == len(versions) - 1 else ""
                    fh.write(f"{version} {{ global: {' '.join(n + ';' for n in names)}{local} }};\n")
            args.append(f"-Wl,--version-script={script}")
        run(*args)

    for f in ("stub.c", "lib.c", "lib.map"):
        try:
            os.remove(os.path.join(out, f))
        except FileNotFoundError:
            pass
    versioned = sum(len(s["func"]) + len(s["data"]) for v in per_lib.values() for s in v.values())
    print(f"stubbed {len(stubbed)} libraries: {len(shared['func'])} functions, {len(shared['data'])} data "
          f"symbols unversioned, {versioned} versioned")


if __name__ == "__main__":
    main()
