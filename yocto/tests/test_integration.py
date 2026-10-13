"""Yocto integration checks; Python 3.11+ is required.

Set POKY_DIR to also parse the real OpenEmbedded classes and execute do_install.
Set SPOTFLOWD_BINARY and run as root for SysV tests.
"""

import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[2]
RECIPE_DIR = ROOT / "yocto/meta-spotflow/recipes-connectivity/spotflowd"
FILES = RECIPE_DIR / "files"


class CargoPackaging(unittest.TestCase):
    def test_generated_dependencies_are_current(self):
        subprocess.run(["python3", str(ROOT / "yocto/update-cargo.py"), "--check"], check=True)

    def test_runtime_configurations_are_valid_toml(self):
        for journald in (True, False):
            for crashdump in (True, False):
                with self.subTest(journald=journald, crashdump=crashdump):
                    text = (FILES / "spotflowd.toml").read_text()
                    for token, value in {
                        "JOURNALD": str(journald).lower(), "SYSLOG": str(not journald).lower(),
                        "CRASHDUMP": str(crashdump).lower(), "LOCALSTATEDIR": "/var",
                        "SYSLOG_PATH": "/var/log/messages",
                    }.items():
                        text = text.replace(f"@{token}@", value)
                    config = tomllib.loads(text)
                    self.assertEqual(config["logs"]["journald"], journald)
                    self.assertEqual(config["logs"]["syslog"], not journald)
                    self.assertEqual(config["crashdump"]["enabled"], crashdump)
                    self.assertEqual(config["device"]["ingest_key"], "")
                    self.assertEqual(config["storage"]["state_dir"], "/var/lib/spotflow")
                    self.assertNotIn("buffer", config["logs"])


@unittest.skipUnless(os.environ.get("POKY_DIR"), "set POKY_DIR for real BitBake metadata checks")
class OpenEmbeddedPackaging(unittest.TestCase):
    def test_metadata_and_install_matrix(self):
        # Isolate the BitBake server, cache and configuration from a user's build.
        poky = Path(os.environ["POKY_DIR"]).resolve()
        with tempfile.TemporaryDirectory() as temp:
            build = Path(temp)
            conf = build / "conf"
            conf.mkdir()
            (conf / "bblayers.conf").write_text(
                'LCONF_VERSION = "7"\nBBPATH = "${TOPDIR}"\nBBFILES ?= ""\n'
                f'BBLAYERS = "{poky}/meta {poky}/meta-poky {ROOT}/yocto/meta-spotflow"\n'
            )
            (conf / "local.conf").write_text(
                'MACHINE = "qemuarm64"\nDISTRO = "poky"\nPACKAGE_CLASSES = "package_ipk"\n'
                'CONF_VERSION = "2"\nINIT_MANAGER = "systemd"\nBB_NUMBER_THREADS = "2"\n'
                # Only parsing/installing; host distribution sanity isn't relevant.
                'INHERIT:remove = "sanity"\nHOSTTOOLS = "git sh bash uname python3 cp mkdir rm xargs wc"\n'
                # One test server deliberately parses different feature sets.
                'BB_HASH_IGNORE_MISMATCH = "1"\n'
            )
            env = os.environ.copy()
            env["PATH"] = str(poky / "bitbake/bin") + os.pathsep + env["PATH"]
            env["PYTHONPATH"] = str(poky / "bitbake/lib")
            result = subprocess.run(
                ["python3", str(ROOT / "yocto/tests/check_bitbake.py")],
                cwd=build, env=env, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            print("\n".join(line for line in result.stdout.splitlines() if line.startswith("validated ")))


@unittest.skipUnless(os.geteuid() == 0 and os.environ.get("SPOTFLOWD_BINARY"),
                     "run as root with SPOTFLOWD_BINARY for SysV lifecycle checks")
class SysVLifecycle(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name)
        self.bin = self.path / "sbin"
        self.etc = self.path / "etc"
        self.bin.mkdir()
        (self.etc / "default").mkdir(parents=True)
        (self.etc / "spotflow").mkdir()
        shutil.copy2(os.environ["SPOTFLOWD_BINARY"], self.bin / "spotflowd")
        ssd = shutil.which("start-stop-daemon")
        self.assertIsNotNone(ssd, "install start-stop-daemon")
        os.symlink(ssd, self.bin / "start-stop-daemon")
        self.run_dir = self.path / "run"
        self.pidfile = self.run_dir / "spotflowd.pid"
        (self.etc / "default/spotflowd").write_text(
            f'DAEMON_USER="root"\nDAEMON_GROUP="root"\nRUNDIR="{self.run_dir}"\nSTOP_TIMEOUT=2\n'
        )
        self.messages = self.path / "messages"
        self.messages.touch()
        self.config = self.etc / "spotflow/spotflowd.toml"
        self.valid_config = (
            '[device]\nid="test"\ningest_key="test"\n'
            '[mqtt]\nbroker="127.0.0.1"\nport=1\n'
            f'[logs]\njournald=false\nsyslog=true\nsyslog_path="{self.messages}"\n'
            f'[storage]\nstate_dir="{self.path}/state"\n'
            f'[status]\nsocket_path="{self.run_dir}/status.sock"\n'
        )
        self.config.write_text(self.valid_config)
        self.script = self.path / "spotflowd.init"
        self.script.write_text((FILES / "spotflowd.init").read_text()
                               .replace("@SBINDIR@", str(self.bin))
                               .replace("@SYSCONFDIR@", str(self.etc)))
        self.addCleanup(self.call, "stop")

    def call(self, command):
        return subprocess.run(["sh", str(self.script), command], capture_output=True, text=True, timeout=10)

    def test_start_status_duplicate_start_restart_and_stop(self):
        started = self.call("start")
        self.assertEqual(started.returncode, 0, started.stderr)
        first_pid = self.pidfile.read_text()
        self.assertEqual(self.call("status").returncode, 0)
        self.assertEqual(self.call("start").returncode, 0)
        self.assertEqual(self.pidfile.read_text(), first_pid)
        self.assertEqual(self.call("restart").returncode, 0)
        self.assertNotEqual(self.pidfile.read_text(), first_pid)
        self.assertEqual(self.call("stop").returncode, 0)
        self.assertFalse(self.pidfile.exists())
        self.assertEqual(self.call("status").returncode, 1)
        self.assertEqual(self.call("stop").returncode, 0)

    def test_invalid_configuration_fails_without_starting(self):
        self.config.write_text('[device]\nid=""\ningest_key=""\n')
        self.assertEqual(self.call("start").returncode, 1)
        self.assertFalse(self.pidfile.exists())

    def test_start_reports_failure_after_successful_preflight(self):
        self.run_dir.mkdir()
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(listener.close)
        listener.bind(str(self.run_dir / "status.sock"))
        listener.listen(10)
        # config-check can access the socket; run rejects another live instance.
        result = self.call("start")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertFalse(self.pidfile.exists())

    def test_stale_pid_cannot_stop_an_unrelated_process(self):
        self.run_dir.mkdir()
        unrelated = subprocess.Popen(["sleep", "30"])
        self.addCleanup(unrelated.wait)
        self.addCleanup(unrelated.terminate)
        self.pidfile.write_text(str(unrelated.pid))
        self.assertEqual(self.call("status").returncode, 1)
        self.assertEqual(self.call("stop").returncode, 0)
        self.assertIsNone(unrelated.poll())


if __name__ == "__main__":
    unittest.main()
