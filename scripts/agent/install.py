#!/usr/bin/env python3
"""Generate the pi extension for ograph. Writes the file to --out (default: print the path you
would install to). It does not touch ~/.pi unless you pass --out there yourself.

Usage: install.py --bin /path/to/ograph --root DIR_WITH_.ograph [--out FILE]"""
import argparse, json, pathlib, subprocess
ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--root", required=True); ap.add_argument("--out")
a = ap.parse_args()
here = pathlib.Path(__file__).resolve().parent
req = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n'
out = subprocess.run([str(pathlib.Path(a.bin).resolve()), "serve", "--mcp"], input=req, capture_output=True, text=True, cwd=a.root).stdout.splitlines()
tools = json.loads(out[1])["result"]["tools"]
skill = (here / "SKILL.md").read_text().split("---", 2)[2].strip()
src = (here / "pi-extension.ts").read_text()
src = src.replace("__BIN__", json.dumps(str(pathlib.Path(a.bin).resolve()))).replace("__TOOLS__", json.dumps(tools)).replace("__INSTRUCTIONS__", json.dumps(skill.replace("`ograph_", "`ograph_")))
dest = pathlib.Path(a.out) if a.out else pathlib.Path.home() / ".pi" / "agent" / "extensions" / "ograph.ts"
if a.out:
    dest.write_text(src); print("wrote", dest)
else:
    print("would install to", dest, "(pass --out to write it)")
