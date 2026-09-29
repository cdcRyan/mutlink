"""Assemble SHA256SUMS and a Homebrew formula from the four Unix release assets."""
import argparse
import hashlib
from pathlib import Path
import re

parser = argparse.ArgumentParser()
parser.add_argument("--version", required=True)
parser.add_argument("--repo", default="cdcRyan/mutlink")
parser.add_argument("--dist", type=Path, default=Path("dist"))
args = parser.parse_args()
if not re.fullmatch(r"v\d+\.\d+\.\d+(?:[.-][A-Za-z0-9.-]+)?", args.version):
    raise SystemExit("Invalid version")
if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
    raise SystemExit("Invalid repository")

targets = {
    "on_macos": {"on_arm": "aarch64-apple-darwin", "on_intel": "x86_64-apple-darwin"},
    "on_linux": {"on_arm": "aarch64-unknown-linux-gnu", "on_intel": "x86_64-unknown-linux-gnu"},
}
expected = [f"mutlink-{args.version}-{target}.tar.gz" for arches in targets.values() for target in arches.values()]
expected.append(f"mutlink-{args.version}-x86_64-pc-windows-msvc.zip")
hashes = {}
for name in expected:
    path = args.dist / name
    if not path.is_file():
        raise SystemExit(f"Missing release asset: {path}")
    hashes[name] = hashlib.sha256(path.read_bytes()).hexdigest()

lines = [
    "class Mutlink < Formula",
    '  desc "Terminal agent with encrypted multi-device collaboration"',
    f'  homepage "https://github.com/{args.repo}"',
    f'  version "{args.version[1:]}"',
    '  license "MIT"',
]
for os_name, arches in targets.items():
    lines += ["", f"  {os_name} do"]
    for arch, target in arches.items():
        name = f"mutlink-{args.version}-{target}.tar.gz"
        lines += [
            f"    {arch} do",
            f'      url "https://github.com/{args.repo}/releases/download/{args.version}/{name}"',
            f'      sha256 "{hashes[name]}"',
            "    end",
        ]
    lines += ["  end"]
lines += [
    "",
    "  def install",
    '    bin.install "mutlink"',
    '    prefix.install "LICENSE", "TWEETNACL-LICENSE", "Cargo.lock"',
    "  end",
    "",
    "  test do",
    '    assert_match version.to_s, shell_output("#{bin}/mutlink --version")',
    "  end",
    "end",
    "",
]
(args.dist / "mutlink.rb").write_text("\n".join(lines))
# Include installers and formula, so users can verify any release asset.
for name in ("install.sh", "install.ps1", "mutlink.rb"):
    path = args.dist / name
    if path.is_file():
        hashes[name] = hashlib.sha256(path.read_bytes()).hexdigest()
(args.dist / "SHA256SUMS").write_text("".join(f"{hashes[name]}  {name}\n" for name in sorted(hashes)))
print(f"Created SHA256SUMS ({len(hashes)} assets) and mutlink.rb for {args.version}")
