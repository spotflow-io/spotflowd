"""Verify provisioning, offline collection and restart on both init systems."""

import json
import time

from oeqa.runtime.case import OERuntimeTestCase
from oeqa.core.decorator.depends import OETestDepends


class SpotflowdTest(OERuntimeTestCase):
    def command(self, command):
        status, output = self.target.run(command)
        self.assertEqual(status, 0, output)
        return output

    def wait_for_status(self, predicate):
        deadline = time.monotonic() + 30
        last = "no status response"
        while time.monotonic() < deadline:
            status, output = self.target.run("spotflowd status --json")
            last = output
            if status == 0:
                snapshot = json.loads(output)
                if predicate(snapshot):
                    return snapshot
            time.sleep(1)
        self.fail(last)

    @OETestDepends(["ssh.SSHTest.test_ssh"])
    def test_offline_collection_and_restart(self):
        systemd = self.target.run("test -d /run/systemd/system")[0] == 0
        restart = "systemctl restart spotflowd" if systemd else "/etc/init.d/spotflowd restart"
        stop = "systemctl stop spotflowd" if systemd else "/etc/init.d/spotflowd stop"
        self.assertNotEqual(self.target.run("spotflowd status --json")[0], 0)
        # Use a deliberately unreachable local broker: no credentials or cloud
        # access are needed, and a device must still collect logs offline.
        self.command(
            "sed -i -e 's/^id = \"\"/id = \"yocto-test\"/' "
            "-e 's/^ingest_key = \"\"/ingest_key = \"test\"/' "
            # Focus on live collection: replaying hundreds of boot records into
            # a one-entry fsync-backed buffer can delay the marker past 30s.
            "-e '/^\\[logs\\]$/a startup_max_entries = 0' "
            "/etc/spotflow/spotflowd.toml; "
            "printf '\\n[mqtt]\\nbroker = \"127.0.0.1\"\\nport = 1\\n"
            "\\n[logs.buffer]\\nmemory_max_entries = 1\\n"
            "\\n[metrics.os]\\nenabled = false\\n' >> /etc/spotflow/spotflowd.toml"
        )
        self.command(restart)
        # Exercise the default account as well as collection. Systemd images
        # must work as the unprivileged journal reader, not just as root.
        if systemd:
            self.assertEqual(self.command("systemctl show -p User --value spotflowd").strip(), "spotflow")
        snapshot = self.wait_for_status(
            lambda value: value["connection"]["state"] == "disconnected"
            and any(value["sources"][source]["health"] == "healthy" for source in ("journald", "syslog"))
        )
        baseline = snapshot["queue"]["records"]
        # push() spills the existing full buffer before adding the next record.
        # A second entry guarantees the first leaves our one-entry memory ring;
        # do not rely on unrelated journal/syslog activity to flush it to disk.
        if systemd:
            # Use the journal's native transport, independently of /dev/log and
            # a distribution's choice of syslog logger/forwarding configuration.
            self.command(
                "printf 'offline log collection works\\noffline spool flush sentinel\\n' | "
                "systemd-cat -t spotflowd-yocto-test"
            )
        else:
            self.command(
                "logger -t spotflowd-yocto-test 'offline log collection works' && "
                "logger -t spotflowd-yocto-test 'offline spool flush sentinel'"
            )
        self.wait_for_status(lambda value: value["queue"]["records"] > baseline)
        # Check the actual log payload so daemon noise cannot satisfy the test.
        payload_check = "grep -a -l -F 'offline log collection works' /var/lib/spotflow/spool/[0-9]*.cbor"
        deadline = time.monotonic() + 30
        while self.target.run(payload_check)[0] != 0:
            if time.monotonic() >= deadline:
                diagnostics = self.target.run(
                    "spotflowd status --json; du -sh /var/lib/spotflow/spool; "
                    "ls -l /dev/log; "
                    "if test -d /run/systemd/system; then "
                    "journalctl --no-pager -n 10 -t spotflowd-yocto-test; "
                    "journalctl --no-pager -n 20 -u spotflowd; "
                    "else tail -n 20 /var/log/messages; fi"
                )[1]
                self.fail("the injected log did not reach the offline disk spool:\n" + diagnostics)
            time.sleep(1)
        self.command(stop)
        self.assertNotEqual(self.target.run("spotflowd status --json")[0], 0)
        self.command(restart)
        self.wait_for_status(lambda value: value["queue"]["records"] > 0)
        self.command(payload_check)
