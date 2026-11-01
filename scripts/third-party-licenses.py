#!/usr/bin/env python3
"""Write the licence texts of every third-party crate in the build to one file.

    scripts/third-party-licenses.py <out-file> [cargo metadata args...]

Apache-2.0 and MIT both require their notices to travel with binaries. This reads
`cargo metadata` (offline, no network), takes each non-workspace package of the given
targets, and copies its LICENSE*/COPYING*/NOTICE* files in. A crate shipping none is
listed with its SPDX expression so nothing is silently left out.
"""
import json, os, subprocess, sys

out = sys.argv[1]
extra = sys.argv[2:]
meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--offline", *extra]))
members = set(meta["workspace_members"])
nodes = {n["id"] for n in meta["resolve"]["nodes"]} if meta.get("resolve") else None
pkgs = [p for p in meta["packages"] if p["id"] not in members and (nodes is None or p["id"] in nodes)]
pkgs.sort(key=lambda p: (p["name"], p["version"]))
missing = []
with open(out, "w", encoding="utf-8") as f:
    f.write("Third-party software in mouse-rs\n" + "=" * 32 + "\n\n")
    f.write(f"{len(pkgs)} crates. Each is listed with its licence, followed by the licence\n"
            "texts it ships.\n\n")
    for p in pkgs:
        d = os.path.dirname(p["manifest_path"])
        files = sorted(x for x in os.listdir(d)
                       if x.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")))
        f.write("-" * 78 + "\n")
        f.write(f"{p['name']} {p['version']}  ({p.get('license') or 'see files'})\n")
        if p.get("repository"):
            f.write(f"{p['repository']}\n")
        f.write("\n")
        if not files:
            missing.append(p["name"])
            f.write(f"(no licence file in the crate; licence: {p.get('license')})\n\n")
        for x in files:
            path = os.path.join(d, x)
            if os.path.isfile(path):
                with open(path, encoding="utf-8", errors="replace") as lf:
                    f.write(f"--- {x} ---\n{lf.read().rstrip()}\n\n")
print(f"wrote {out}: {len(pkgs)} crates, {len(missing)} without a licence file")
