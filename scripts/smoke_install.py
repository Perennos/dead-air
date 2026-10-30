"""Exercise the installed distribution outside the source tree, without network."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def main():
    command = shutil.which("dead-air")
    if command is None:
        raise RuntimeError("installed dead-air command not found on PATH")
    with tempfile.TemporaryDirectory(prefix="dead-air-installed-") as directory:
        root = Path(directory)
        environment = dict(os.environ)
        environment.pop("PYTHONPATH", None)

        def run(*arguments, expected=0):
            result = subprocess.run([command, *arguments], cwd=root, env=environment,
                                    capture_output=True, text=True, timeout=10)
            if result.returncode != expected:
                raise RuntimeError(f"installed {arguments[0]} returned {result.returncode}: {result.stderr}")
            return result.stdout

        assert "0.2.0" in run("--version")
        assert json.loads(run("init"))["created"]
        assert json.loads(run("validate", "--config", "dead-air.toml"))["ok"]
        status = json.loads(run("status", "--json", expected=2))
        assert status["status"] == "UNKNOWN" and not status["network_contacted"]
        assert not (root / "state").exists(), "read-only status created state"
        run("init", expected=3)
        imported = subprocess.run(
            [sys.executable, "-c",
             "import deadair.persistence, deadair.status, dead_air; "
             "d=dead_air.DestinationConfig('fixture','webhook',endpoint_env='UNSET_DEAD_AIR_FIXTURE'); "
             "n=dead_air.NetworkConfig((),3,1024); "
             "r=dead_air._send_destination_bounded(d,{'event_id':'fixture','transition':'open',"
             "'stream_id':'fixture','condition':'off_air'},n,timeout_s=3,environment={}); "
             "assert r[1]=='ENDPOINT_UNAVAILABLE',r; print(dead_air.VERSION)"],
            cwd=root, env=environment, capture_output=True, text=True, timeout=10,
        )
        assert imported.returncode == 0 and imported.stdout.strip() == "0.2.0", imported.stderr
        gui_probe = """
import http.client
import threading
from pathlib import Path
import dead_air_gui

server = dead_air_gui.make_server(Path('missing.toml'), 0)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    connection = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=3)
    connection.request('GET', '/')
    page = connection.getresponse()
    assert page.status == 200 and b'__TOKEN__' not in page.read()
    connection.close()
    connection = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=3)
    connection.request('GET', '/brand/mark.svg')
    logo = connection.getresponse()
    assert logo.status == 200 and b'<svg' in logo.read()
    connection.close()
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=3)
"""
        gui = subprocess.run([sys.executable, "-c", gui_probe], cwd=root, env=environment,
                             capture_output=True, text=True, timeout=10)
        assert gui.returncode == 0, gui.stderr
    print("Installed CLI, config/status, compatibility imports, and GUI resources passed.")


if __name__ == "__main__":
    main()
