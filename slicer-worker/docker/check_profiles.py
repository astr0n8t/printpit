"""Check that every packed vendor cache (<Vendor>.opc) from the AppImage has a JSON index of the same
version in the profile tree the worker ships, so that JSON is what this Orca release was built with.

The .opc header (PresetCacheFormat.cpp, CACHE_VERSION 2): a 20-byte frame (magic "ZCRO" as bytes,
format version, body size, CRC-32), then a cereal binary body starting with the format version
(u32) and two length-prefixed (u64) strings, the vendor name and its profile version.

usage: check_profiles.py <dir with the .opc caches> <dir with the JSON profiles>
(Only the modules python3-minimal has: no json, so the index's version is read with a regex.)
"""

import pathlib
import re
import struct
import sys
import zlib


def stamp(path):
    data = path.read_bytes()
    magic, version, size, crc = struct.unpack_from("<IIQI", data)
    body = data[20:]
    if magic != 0x4F52435A or version != 2 or size != len(body) or zlib.crc32(body) != crc:
        sys.exit(f"{path.name}: not a version-2 Orca preset cache (or damaged)")
    pos = 4
    out = []
    for _ in range(2):
        (n,) = struct.unpack_from("<Q", body, pos)
        out.append(body[pos + 8 : pos + 8 + n].decode())
        pos += 8 + n
    return out


def semver(v):
    # Orca stamps a three-part version as four ("03.00.13" -> "3.0.0.13"), so zero parts don't count.
    return tuple(n for n in (int(p) for p in v.split(".")) if n)


caches_dir, json_dir = (pathlib.Path(a) for a in sys.argv[1:3])
caches = sorted(caches_dir.glob("*.opc"))
if not caches:
    sys.exit(f"no .opc caches in {caches_dir}")
for cache in caches:
    name, version = stamp(cache)
    index = json_dir / f"{cache.stem}.json"
    if name != cache.stem or not index.is_file():
        sys.exit(f"{cache.name}: no {index.name} beside it (vendor {name!r})")
    found = re.search(r'"version"\s*:\s*"([0-9.]+)"', index.read_text(encoding="utf-8"))
    if not found:
        sys.exit(f"{index.name}: no version")
    want = found.group(1)
    if semver(want) != semver(version):
        sys.exit(f"{cache.name} is version {version}, but {index.name} is {want}: wrong source checkout?")
print(f"profiles: {len(caches)} vendors, the JSON matches the packed caches")
