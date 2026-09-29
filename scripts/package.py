"""Package a release binary. Usage: package.py TARGET [BINARY_PATH]."""
import hashlib
from pathlib import Path
import re
import sys
import tarfile
import zipfile

root = Path(__file__).resolve().parents[1]
target = sys.argv[1]
if not target or any(c not in "abcdefghijklmnopqrstuvwxyz0123456789_-" for c in target):
    raise SystemExit("Invalid target triple")
version = re.search(r'^version = "([^"]+)"$', (root / "Cargo.toml").read_text(), re.M)[1]
exe = "mutlink.exe" if "windows" in target else "mutlink"
binary = Path(sys.argv[2]) if len(sys.argv) > 2 else root / "target" / "release" / exe
if not binary.is_file():
    raise SystemExit("Build the release binary first")
out = root / "dist"
out.mkdir(exist_ok=True)
extension = "zip" if "windows" in target else "tar.gz"
archive = out / f"mutlink-v{version}-{target}.{extension}"
files = [
    (binary, exe), (root / "README.md", "README.md"),
    (root / "LICENSE", "LICENSE"),
    (root / "web/vendor/TWEETNACL-LICENSE", "TWEETNACL-LICENSE"),
    (root / "Cargo.lock", "Cargo.lock"),
    (root / "scripts/install.sh", "scripts/install.sh"),
    (root / "scripts/install.ps1", "scripts/install.ps1"),
]
if extension == "zip":
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
        for path, name in files:
            bundle.write(path, name)
else:
    with tarfile.open(archive, "w:gz") as bundle:
        for path, name in files:
            bundle.add(path, arcname=name)
checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
(out / f"{archive.name}.sha256").write_text(f"{checksum}  {archive.name}\n")
print(f"Packaged {archive.name}: {archive.stat().st_size:,} bytes")
