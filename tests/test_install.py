import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[1]
WINDOWS = os.name == "nt"
WINGET_PYTHON = "winget install --id Python.Python.3.12 -e"
WINGET_FFMPEG = "winget install --id Gyan.FFmpeg.Shared -e"
FFMPEG_PAGE = "https://ffmpeg.org/download.html"


def path_without(*names):
    """PATH with every folder dropped that holds one of these programs."""
    keep = [p for p in os.environ["PATH"].split(os.pathsep) if p and not any((Path(p) / n).exists() for n in names)]
    return os.pathsep.join(keep)


def bash_works():
    bash = shutil.which("bash")
    if not bash:
        return False
    try:
        return subprocess.run([bash, "-c", "exit 0"], capture_output=True, timeout=30).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


@unittest.skipUnless(WINDOWS, "install.cmd is for Windows")
class InstallCmdTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        (self.dir / "brand").mkdir()
        shutil.copy(ROOT / "install.cmd", self.dir)
        shutil.copy(ROOT / "brand" / "icon.ico", self.dir / "brand")
        if os.environ.get("DEAD_AIR_INSTALL_REAL") == "1":  # opt in: really download from the package index
            shutil.copy(ROOT / "requirements.txt", self.dir)
        else:
            (self.dir / "requirements.txt").write_text("# no requirements for this test\n", encoding="ascii")
        self.shortcuts = self.dir / "shortcuts"

    def run_install(self, *args, path=None, timeout=60):
        env = dict(os.environ, INSTALL_SHORTCUT_DIR=str(self.shortcuts))
        if path is not None:
            env["PATH"] = path
        result = subprocess.run([str(self.dir / "install.cmd"), *args], cwd=self.dir, env=env,
                                capture_output=True, text=True, errors="replace", timeout=timeout)
        return result, result.stdout + result.stderr

    def test_dry_run_prints_the_steps_and_changes_nothing(self):
        before = sorted(os.listdir(self.dir))
        result, output = self.run_install("/dry-run")
        self.assertEqual(result.returncode, 0, output)
        for text in ("[dry-run]", "Step 1", "pip install --disable-pip-version-check -r requirements.txt",
                     "Dead Air.lnk", "INSTALL_SHORTCUT_DIR"):
            self.assertIn(text, output)
        self.assertEqual(sorted(os.listdir(self.dir)), before)
        self.assertFalse((self.dir / ".venv").exists())

    def test_the_shortcut_passes_watch_only_when_the_config_already_has_a_stream(self):
        _, output = self.run_install("/dry-run")
        self.assertIn("running: dead_air.py gui --config dead-air.toml", output)
        self.assertNotIn("--watch", output)
        (self.dir / "dead-air.toml").write_text("[[streams]]\nid = \"a\"\n", encoding="ascii")
        _, output = self.run_install("/dry-run")
        self.assertIn("running: dead_air.py gui --config dead-air.toml --watch", output)

    def test_missing_ffmpeg_prints_the_official_page_and_the_winget_line_and_continues(self):
        path = path_without("ffmpeg.exe", "ffprobe.exe")
        if not shutil.which("py", path=path) and not shutil.which("python", path=path):
            self.skipTest("Python shares a folder with ffmpeg on this machine")
        result, output = self.run_install("/dry-run", path=path)
        self.assertEqual(result.returncode, 0, output)
        self.assertIn(FFMPEG_PAGE, output)
        self.assertIn(WINGET_FFMPEG, output.splitlines())
        self.assertIn("Done. Nothing was changed.", output)

    def test_no_python_prints_the_winget_line_and_exits_1(self):
        system = os.environ.get("SystemRoot", r"C:\Windows")
        path = os.pathsep.join([str(Path(system) / "System32"), system])
        if shutil.which("py", path=path) or shutil.which("python", path=path):
            self.skipTest("a Python launcher lives in the Windows folder on this machine")
        result, output = self.run_install("/dry-run", path=path)
        self.assertEqual(result.returncode, 1, output)
        self.assertIn(WINGET_PYTHON, output.splitlines())

    def test_real_install_creates_the_venv_and_both_shortcuts(self):
        try:
            result, output = self.run_install(timeout=180)
        except subprocess.TimeoutExpired:
            self.skipTest("install took longer than 3 minutes on this machine")
        self.assertEqual(result.returncode, 0, output)
        for name in ("python.exe", "pythonw.exe"):
            self.assertTrue((self.dir / ".venv" / "Scripts" / name).is_file(), name)
        links = [self.shortcuts / "Start Menu" / "Dead Air.lnk", self.shortcuts / "Desktop" / "Dead Air.lnk"]
        for link in links:
            self.assertTrue(link.is_file(), link)
        read = ("$s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:LNK); "
                "$s.TargetPath; $s.Arguments; $s.WorkingDirectory; $s.IconLocation")
        shown = subprocess.run(["powershell", "-NoProfile", "-Command", read], capture_output=True, text=True,
                               timeout=60, env=dict(os.environ, LNK=str(links[0]))).stdout.splitlines()
        self.assertTrue(os.path.samefile(shown[0], self.dir / ".venv" / "Scripts" / "pythonw.exe"))
        self.assertEqual(shown[1], "dead_air.py gui --config dead-air.toml")
        self.assertTrue(os.path.samefile(shown[2], self.dir))
        self.assertTrue(shown[3].lower().endswith(r"brand\icon.ico,0"))


FAKE_PYTHON = """#!/bin/sh
# Stands in for python3 and for .venv/bin/python: real interpreter for -c, a stub venv and a log for the rest.
case "$1" in
  -c) exec "{real}" "$@" ;;
  -m) if [ "$2" = venv ]; then mkdir -p "$3/bin" && cp "$0" "$3/bin/python" && chmod +x "$3/bin/python"; exit 0; fi
      echo "$@" >> "{log}"; exit 0 ;;
esac
exit 0
"""


@unittest.skipUnless(bash_works(), "needs a working bash")
class InstallShTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name) / "repo copy"  # a space, to prove the launcher quotes its paths
        self.dir.mkdir()
        shutil.copy(ROOT / "install.sh", self.dir)
        (self.dir / "requirements.txt").write_text("# none\n", encoding="ascii")
        self.bin = Path(self.temp.name) / "bin"
        self.bin.mkdir()
        self.log = Path(self.temp.name) / "pip.log"
        self.write_tool("python3", FAKE_PYTHON.format(real=Path(sys.executable).as_posix(), log=self.log.as_posix()))
        self.shortcuts = Path(self.temp.name) / "shortcuts"

    def write_tool(self, name, text):
        tool = self.bin / name
        tool.write_text(text, encoding="ascii", newline="\n")
        tool.chmod(tool.stat().st_mode | stat.S_IXUSR)

    def run_install(self, *args, system="Linux"):
        self.write_tool("uname", f"#!/bin/sh\necho {system}\n")
        env = dict(os.environ, INSTALL_SHORTCUT_DIR=str(self.shortcuts),
                   PATH=str(self.bin) + os.pathsep + os.environ["PATH"])
        result = subprocess.run([shutil.which("bash"), "install.sh", *args], cwd=self.dir, env=env,
                                capture_output=True, text=True, errors="replace", timeout=120)
        return result, result.stdout + result.stderr

    def test_syntax(self):
        result = subprocess.run([shutil.which("bash"), "-n", "install.sh"], cwd=ROOT, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_dry_run_prints_the_steps_and_changes_nothing(self):
        result, output = self.run_install("--dry-run")
        self.assertEqual(result.returncode, 0, output)
        self.assertIn("[dry-run] python3 -m venv .venv", output)
        self.assertIn("-m pip install --disable-pip-version-check -r requirements.txt", output)
        self.assertIn("Done. Nothing was changed.", output)
        self.assertEqual(sorted(p.name for p in self.dir.iterdir()), ["install.sh", "requirements.txt"])
        self.assertFalse(self.shortcuts.exists())

    def test_linux_install_makes_the_venv_installs_requirements_and_writes_the_desktop_file(self):
        result, output = self.run_install()
        self.assertEqual(result.returncode, 0, output)
        self.assertTrue((self.dir / ".venv" / "bin" / "python").is_file())
        self.assertIn("-m pip install --disable-pip-version-check -r requirements.txt", self.log.read_text())
        entry = (self.shortcuts / "dead-air.desktop").read_text()
        self.assertRegex(entry, r'(?m)^Exec="[^"]*/repo copy/\.venv/bin/python" "[^"]*/repo copy/dead_air\.py" '
                                r'gui --config "[^"]*/repo copy/dead-air\.toml"$')
        self.assertRegex(entry, r"(?m)^Path=.*/repo copy$")
        self.assertIn("Name=Dead Air", entry)
        self.assertIn("Terminal=false", entry)

    def test_launchers_pass_watch_only_when_the_config_already_has_a_stream(self):
        (self.dir / "dead-air.toml").write_text("[[streams]]\nid = \"a\"\n", encoding="ascii")
        self.assertEqual(self.run_install()[0].returncode, 0)
        self.assertRegex((self.shortcuts / "dead-air.desktop").read_text(), r'(?m)^Exec=.*dead-air\.toml" --watch$')
        self.assertEqual(self.run_install(system="Darwin")[0].returncode, 0)
        self.assertRegex((self.shortcuts / "Dead Air.command").read_text(), r"(?m)^exec .*dead-air\.toml --watch$")

    def test_macos_install_writes_a_command_file_that_starts_the_gui_in_the_repo(self):
        result, output = self.run_install(system="Darwin")
        self.assertEqual(result.returncode, 0, output)
        script = (self.shortcuts / "Dead Air.command").read_text()
        self.assertTrue(script.startswith("#!/bin/bash\n"))
        self.assertRegex(script, r'(?m)^cd "[^"]*/repo copy" \|\| exit 1$')
        self.assertRegex(script, r'(?m)^exec "[^"]*/repo copy/\.venv/bin/python" dead_air\.py gui --config dead-air\.toml$')
        if not WINDOWS:
            self.assertTrue(os.access(self.shortcuts / "Dead Air.command", os.X_OK))

    def test_old_python_gets_the_install_lines_and_exit_1(self):
        self.write_tool("python3", "#!/bin/sh\nexit 1\n")
        self.write_tool("python", "#!/bin/sh\nexit 1\n")
        env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"])
        result = subprocess.run([shutil.which("bash"), "install.sh", "--dry-run"], cwd=self.dir, env=env,
                                capture_output=True, text=True, errors="replace", timeout=60)
        self.assertEqual(result.returncode, 1)
        self.assertIn("Python 3.11 or newer was not found", result.stdout)
        self.assertIn("brew install python", result.stdout)


if __name__ == "__main__":
    unittest.main()
