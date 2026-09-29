"""Exercise download installers with real packaged binaries and local transport fixtures."""
import hashlib
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parents[1]
version = "v" + re.search(r'^version = "([^"]+)"$', (root / "Cargo.toml").read_text(), re.M)[1]
windows = sys.platform == "win32"
arch = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
target = "x86_64-pc-windows-msvc" if windows else arch + ("-apple-darwin" if sys.platform == "darwin" else "-unknown-linux-gnu")
asset = f"mutlink-{version}-{target}." + ("zip" if windows else "tar.gz")
archive = root / "dist" / asset
if not archive.exists():
    raise SystemExit("Run scripts/package.py for the native platform first")

with tempfile.TemporaryDirectory() as temporary:
    work = Path(temporary)
    fixtures, commands, installed = (work / name for name in ("fixtures", "commands", "installed"))
    fixtures.mkdir()
    commands.mkdir()
    installed.mkdir()
    shutil.copy2(archive, fixtures / asset)
    sums = fixtures / "SHA256SUMS"
    correct_sum = f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {asset}\n"
    sums.write_text(correct_sum)
    transport = commands / "transport.py"
    transport.write_text("""
import glob, os, pathlib, shutil, sys
args = sys.argv[2:]
base = pathlib.Path(os.environ["MUTLINK_FIXTURES"])
if sys.argv[1] == "gh":
    if args[:2] == ["release", "view"]:
        print(os.environ["MUTLINK_FIXTURE_VERSION"])
    elif args[:2] == ["release", "download"]:
        destination = pathlib.Path(args[args.index("--dir") + 1])
        for i, arg in enumerate(args):
            if arg == "--pattern":
                matches = list(base.glob(args[i + 1]))
                if not matches:
                    raise SystemExit("Missing fixture")
                for path in matches:
                    shutil.copy2(path, destination / path.name)
    else:
        raise SystemExit("Unexpected gh arguments")
elif sys.argv[1] == "curl":
    url = next(arg for arg in args if arg.startswith("https://"))
    if url.endswith("/latest"):
        print("https://github.com/cdcRyan/mutlink/releases/tag/" + os.environ["MUTLINK_FIXTURE_VERSION"], end="")
    else:
        shutil.copy2(base / url.rsplit("/", 1)[1], args[args.index("-o") + 1])
""")
    for command in ("gh", "curl"):
        wrapper = commands / (command + (".cmd" if windows else ""))
        if windows:
            wrapper.write_text(f'@"{sys.executable}" "{transport}" {command} %*\n')
        else:
            import shlex
            wrapper.write_text(f"#!/bin/sh\nexec {shlex.quote(sys.executable)} {shlex.quote(str(transport))} {command} \"$@\"\n")
            wrapper.chmod(0o755)
    env = {
        **os.environ,
        "PATH": str(commands) + os.pathsep + os.environ["PATH"],
        "MUTLINK_USE_GH": "1",
        "MUTLINK_VERSION": "latest",
        "MUTLINK_REPO": "cdcRyan/mutlink",
        "MUTLINK_INSTALL_DIR": str(installed),
        "MUTLINK_FIXTURES": str(fixtures),
        "MUTLINK_FIXTURE_VERSION": version,
    }
    invocation = ["pwsh", "-NoProfile", "-File", str(root / "install.ps1")] if windows else ["sh", str(root / "install.sh")]
    binary = installed / ("mutlink.exe" if windows else "mutlink")

    def run(success=True, message=None):
        result = subprocess.run(invocation, env=env, text=True, capture_output=True)
        assert (result.returncode == 0) == success, result.stdout + result.stderr
        if message:
            assert message in result.stdout + result.stderr, result.stdout + result.stderr

    run()
    output = subprocess.check_output([str(binary), "--version"], text=True)
    assert version[1:] in output, output
    print("PASS: authenticated latest install and real binary startup")
    binary.write_bytes(b"existing installation")
    sums.write_text("0" * 64 + f"  {asset}\n")
    run(False, "Checksum mismatch")
    assert binary.read_bytes() == b"existing installation"
    print("PASS: checksum failure preserves existing installation")
    sums.write_text("0" * 64 + "  unrelated.tar.gz\n")
    run(False, "Missing or invalid SHA256")
    assert binary.read_bytes() == b"existing installation"
    print("PASS: missing checksum preserves existing installation")
    sums.write_text(correct_sum)
    env["MUTLINK_VERSION"] = version
    run()
    assert binary.with_name(binary.name + ".previous").read_bytes() == b"existing installation"
    assert version[1:] in subprocess.check_output([str(binary), "--version"], text=True)
    print("PASS: pinned version upgrade keeps backup")
    if not windows:
        env["MUTLINK_USE_GH"] = "0"
        env["MUTLINK_VERSION"] = "latest"
        run()
        print("PASS: public curl latest download")
        uname = commands / "uname"
        uname.write_text('#!/bin/sh\nprintf "unsupported\\n"\n')
        uname.chmod(0o755)
        run(False, "Unsupported OS")
        print("PASS: unsupported platform rejected")
