#!/bin/sh
# Turn a verified OrcaSlicer AppImage into the slim tree the worker image ships (PLAN.md §15.10):
#   <dest>/bin/orca-slicer        stripped CLI binary
#   <dest>/lib/orca-runtime/      the few libraries Orca bundles (zlib, lzma, expat, mspack)
#   <dest>/stubs/                 generated stand-ins for the GUI libraries (gui_stubs.py)
#   <dest>/resources/profiles/    the system profile JSON only (no images, fonts, web UI)
# Runs on the build machine with the target's binutils and gcc, so nothing is executed for the
# target architecture.
#
# usage: prepare-orca.sh <AppImage> <target triple> <dest>
set -eu
img=$1
triple=$2
dest=$3
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The squashfs image starts right after the AppImage's ELF runtime: the end of its section headers.
field() { "$triple-readelf" -h "$img" | sed -n "s/^ *$1: *\([0-9]*\).*/\1/p"; }
offset=$(( $(field "Start of section headers") + $(field "Size of section headers") * $(field "Number of section headers") ))
magic=$(dd if="$img" bs=1 skip="$offset" count=4 2>/dev/null)
[ "$magic" = "hsqs" ] || { echo "no squashfs at offset $offset (got '$magic')" >&2; exit 1; }
unsquashfs -q -n -o "$offset" -d "$work/root" "$img" >/dev/null

mkdir -p "$dest/bin" "$dest/lib" "$dest/resources"
"$triple-strip" -o "$dest/bin/orca-slicer" "$work/root/bin/orca-slicer"
cp -a "$work/root/lib/orca-runtime" "$dest/lib/"
(cd "$work/root/resources" && find profiles -name '*.json' -exec cp --parents -t "$dest/resources/" {} +)
python3 "$here/gui_stubs.py" "$dest/bin/orca-slicer" "$dest/lib/orca-runtime" "$dest/stubs" "$triple"
chmod -R a+rX,go-w "$dest"
echo "orca tree: $(du -sh "$dest" | cut -f1) ($(find "$dest/resources/profiles" -name '*.json' | wc -l) profiles)"
