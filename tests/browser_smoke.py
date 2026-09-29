"""Real browser -> NaCl packet -> relay -> Rust agent -> mock HTTP model roundtrip.

Run after cargo build: python3 tests/browser_smoke.py
Requires Python Playwright and its Chromium browser. No cloud credentials used.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from playwright.sync_api import sync_playwright, expect

PROJECT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("MUTLINK_TEST_BINARY", PROJECT / "target/debug/mutlink"))
ARTIFACTS = PROJECT / ".artifacts"
ARTIFACTS.mkdir(exist_ok=True)


class Model(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["stream"] is True
        data = 'data: ' + json.dumps({"choices": [{"delta": {"content": "Browser linked. Encrypted task completed."}, "finish_reason": "stop"}]}) + '\n\ndata: [DONE]\n\n'
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.wfile.write(data.encode())

    def log_message(self, *_):
        pass


def wait_http(url):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            urllib.request.urlopen(url, timeout=1).close()
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("Server did not start")


def main():
    with tempfile.TemporaryDirectory(prefix="browser-", dir=ARTIFACTS) as tmp:
        root = Path(tmp)
        home = root / "desktop"

        def cli(*args):
            return subprocess.check_output([str(BINARY), "--home", str(home), *args], text=True, env=env)

        env = {**os.environ, "MUTLINK_TEST_PASSWORD": "browser-test-password-12345"}
        with socket.socket() as lease:
            lease.bind(("127.0.0.1", 0))
            port = lease.getsockname()[1]
        relay_url = f"http://127.0.0.1:{port}"
        log = open(root / "relay.log", "w")
        relay = subprocess.Popen([str(BINARY), "relay", "--listen", f"127.0.0.1:{port}", "--database", str(root / "relay.db"), "--allow-registration"], stdout=log, stderr=log)
        model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
        threading.Thread(target=model.serve_forever, daemon=True).start()
        try:
            wait_http(relay_url)
            cli("status")
            cli("model", "set", "--model", "integration-test", "--base-url", f"http://127.0.0.1:{model.server_port}")
            cli("workspace", str(root))
            cli("device", "--name", "Studio Mac")
            with sync_playwright() as p:
                browser = p.chromium.launch(headless=True)
                page = browser.new_page(viewport={"width": 390, "height": 844}, is_mobile=True, has_touch=True)
                errors = []
                page.on("pageerror", lambda e: errors.append(str(e)))
                page.goto(relay_url)
                page.wait_for_load_state("networkidle")
                expect(page.get_by_role("heading", name="从这里，连接你的 Agent。")).to_be_visible()
                page.screenshot(path=str(ARTIFACTS / "mobile-login.png"), full_page=True)
                page.locator("#account").fill("browser-test")
                page.locator("#password").fill(env["MUTLINK_TEST_PASSWORD"])
                page.locator("#register").check()
                page.get_by_role("button", name="连接设备").click()
                expect(page.locator("#workspace")).to_be_visible()
                cli("login", "--relay", relay_url, "--account", "browser-test", "--password-env", "MUTLINK_TEST_PASSWORD")
                phone = page.evaluate("JSON.parse(localStorage.getItem('mutlink.identity')).device")
                phone_file = root / "phone-public.json"
                phone_file.write_text(json.dumps(phone))
                cli("pair", "trust", str(phone_file), "--fingerprint", phone["id"], "--allow-agent")
                desktop = json.loads(cli("pair", "export"))
                desktop_file = root / "desktop-public.json"
                desktop_file.write_text(json.dumps(desktop))
                page.get_by_text("设备配对", exact=True).click()
                page.locator("#peer-file").set_input_files(desktop_file)
                page.locator("#peer-fingerprint").fill(desktop["id"])
                page.get_by_role("button", name="核对并信任电脑").click()
                expect(page.locator("#target")).to_have_value(desktop["id"], timeout=10000)
                expect(page.locator(f'#target option[value="{desktop["id"]}"]')).to_have_text("● Studio Mac · 已配对", timeout=15000)
                page.get_by_text("设备配对", exact=True).click()
                page.locator("#plan").check()
                page.locator("#prompt").fill("Say hello through the encrypted device connection.")
                page.locator("#send").click()
                try:
                    expect(page.locator("#conversation")).to_contain_text("Browser linked. Encrypted task completed.", timeout=30000)
                except AssertionError:
                    page.screenshot(path=str(ARTIFACTS / "browser-failure.png"), full_page=True)
                    print("Browser notice:", page.locator("#notice").inner_text(), flush=True)
                    print("Browser errors:", errors, flush=True)
                    print("Desktop status:", cli("status"), flush=True)
                    print("Desktop tasks:", cli("tasks"), flush=True)
                    raise
                expect(page.locator("#task-state")).to_contain_text("succeeded", timeout=15000)
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "mobile overflow"
                page.screenshot(path=str(ARTIFACTS / "mobile-session.png"), full_page=True)
                # Reload preserves identity/task and replays durable events from cursor zero.
                page.reload()
                expect(page.locator("#conversation")).to_contain_text("Browser linked.", timeout=20000)
                page.set_viewport_size({"width": 1440, "height": 960})
                page.emulate_media(reduced_motion="reduce")
                page.screenshot(path=str(ARTIFACTS / "desktop-controller.png"), full_page=True)
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "desktop overflow"
                assert not errors, errors
                browser.close()
            print("PASS: browser/Rust E2EE, relay login, pairing, task execution, reload recovery, 390px/1440px layouts")
        finally:
            try:
                cli("stop")
            except subprocess.CalledProcessError:
                pass
            relay.terminate()
            relay.wait(timeout=10)
            model.shutdown()
            log.close()


if __name__ == "__main__":
    main()
