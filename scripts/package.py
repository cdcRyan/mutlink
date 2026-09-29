"""Package a locally built release binary; no publishing or signing.

Usage: python3 scripts/package.py aarch64-apple-darwin
"""
import hashlib
from pathlib import Path
import sys
import tarfile

root = Path(__file__).resolve().parents[1]
target = sys.argv[1]
if not target or any(c not in "abcdefghijklmnopqrstuvwxyz0123456789_-" for c in target):
    raise SystemExit("Invalid target triple")
exe = "mutlink.exe" if "windows" in target else "mutlink"
binary = root / "target" / "release" / exe
if not binary.is_file():
    raise SystemExit("Build the release binary first")
out = root / "dist"
out.mkdir(exist_ok=True)
archive = out / f"mutlink-v0.1.0-{target}.tar.gz"
with tarfile.open(archive, "w:gz") as tar:
    for path, name in [
        (binary, exe), (root / "README.md", "README.md"),
        (root / "LICENSE", "LICENSE"),
        (root / "web/vendor/TWEETNACL-LICENSE", "TWEETNACL-LICENSE"),
        (root / "Cargo.lock", "Cargo.lock"),
        (root / "scripts/install.sh", "scripts/install.sh"),
        (root / "scripts/install.ps1", "scripts/install.ps1"),
    ]:
        tar.add(path, arcname=name)
checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
(out / "SHA256SUMS").write_text(f"{checksum}  {archive.name}\n")
print(f"Packaged {archive.name}: {archive.stat().st_size:,} bytes")
