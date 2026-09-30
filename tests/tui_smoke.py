"""Launch the real CLI in a macOS/Linux PTY and exercise basic keyboard input."""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time

PROJECT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("MUTLINK_TEST_BINARY", PROJECT / "target/debug/mutlink"))
ARTIFACTS = PROJECT / ".artifacts"
ARTIFACTS.mkdir(exist_ok=True)

with tempfile.TemporaryDirectory(prefix="tui-", dir=ARTIFACTS) as root:
    catalog_path = Path(root) / "models.json"
    catalog_path.write_text(json.dumps({
        "default": "first",
        "models": {
            name: {
                "model": name + "-model",
                "base_url": "http://127.0.0.1:1234/v1",
                "api_key_env": "MUTLINK_SMOKE_KEY",
            } for name in ("first", "second")
        },
    }))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
    process = subprocess.Popen([str(BINARY), "--home", root], stdin=slave, stdout=slave, stderr=slave, env={**os.environ, "TERM": "xterm-256color"})
    os.close(slave)
    output = bytearray()

    def drain(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                try:
                    output.extend(os.read(master, 65536))
                except OSError:
                    return

    def cli(*args):
        return json.loads(subprocess.check_output([str(BINARY), "--home", root, *args]))

    try:
        drain(2)
        assert b"mutlink" in output and b"BUILD" in output, output[-1000:]
        os.write(master, b"\x1b[Z")  # Shift+Tab -> Plan
        drain(0.3)
        assert b"PLAN" in output
        os.write(master, b"/help\r")
        drain(0.3)
        # Ratatui emits cursor moves between unchanged spaces in a differential frame.
        assert b"history" in output and b"/resume" in output
        os.write(master, b"/model\r")
        drain(0.5)
        assert b"Select model" in output and b"models.json" in output
        os.write(master, b"\x1b[B\r")  # Down, Enter -> second model
        drain(0.5)
        assert cli("status")["model"] == "second-model"
        assert json.loads(catalog_path.read_text())["default"] == "second"
        os.write(master, b"/model first\r")
        drain(0.5)
        assert cli("status")["model"] == "first-model"
        catalog = json.loads(catalog_path.read_text())
        catalog["models"]["third"] = {**catalog["models"]["first"], "model": "third-model"}
        catalog_path.write_text(json.dumps(catalog))
        os.write(master, b"/model third\r")
        drain(0.5)
        assert cli("status")["model"] == "third-model"
        os.write(master, b"/model missing\r")
        drain(0.5)
        assert cli("status")["model"] == "third-model"
        os.write(master, b"\x03")
        process.wait(timeout=5)
        drain(0.1)
        assert process.returncode == 0
        (ARTIFACTS / "tui.ansi").write_bytes(output)
        cli("stop")
        for _ in range(50):
            if not (Path(root) / "daemon.json").exists():
                break
            time.sleep(0.1)
        assert not (Path(root) / "daemon.json").exists()
        assert cli("model", "list")["selected"] == "third"
        print("PASS: bare startup, Plan toggle, model picker, named switch, live config reload, persistence, clean exit")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        os.close(master)
        subprocess.run([str(BINARY), "--home", root, "stop"], check=False, stdout=subprocess.DEVNULL)
