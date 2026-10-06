"""Exercise provisioning/resume lifecycle and the real remote matrix runner without Azure."""

import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import Mock


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("azure_test", ROOT / "yocto/azure-test.py")
azure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(azure)
prune_spec = importlib.util.spec_from_file_location("cache_prune", ROOT / "yocto/cache-prune.py")
cache_prune = importlib.util.module_from_spec(prune_spec)
prune_spec.loader.exec_module(cache_prune)


class AzureLifecycle(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name)
        self.args = azure.parse_args(["--state-dir", str(self.state)])
        self.runner = azure.AzureRunner(self.args)

    def provision(self, *, existing=False, quota_limit=10, existing_nic=False):
        key_dir = self.state / "subscription-id" / self.args.group
        key_dir.mkdir(parents=True, exist_ok=True)
        (key_dir / "id_ed25519").write_text("test-private-key")
        (key_dir / "id_ed25519.pub").write_text("ssh-ed25519 AAAA test")
        disk = {"name": azure.DISK, "id": "/disks/persistent-cache", "diskSizeGB": 256}
        group = {"location": "westeurope", "tags": {azure.TAG: "true"}}
        vm = {"name": azure.VM, "storageProfile": {"dataDisks": [
            {"lun": 0, "managedDisk": {"id": disk["id"]}, "deleteOption": "Detach"},
        ]}}

        def response(*args):
            if args[:3] == ("network", "nic", "list"):
                return [{"name": f"{azure.VM}VMNic", "id": "/nics/existing"}] if existing_nic else []
            command = args[:2]
            if command == ("account", "show"):
                return {"id": "subscription-id", "name": "Test"}
            if command == ("group", "exists"):
                return existing
            if command in (("group", "create"), ("group", "show")):
                return group
            if command == ("disk", "list"):
                return [disk] if existing else []
            if command == ("disk", "create"):
                return disk
            if command == ("vm", "list"):
                return [vm] if existing else []
            if command == ("vm", "list-skus"):
                return [{"name": self.args.size, "family": "standardEASv5Family", "capabilities": [
                    {"name": "vCPUs", "value": "4"}, {"name": "MemoryGB", "value": "32"},
                ]}]
            if command == ("vm", "list-usage"):
                return [
                    {"name": {"value": "standardEASv5Family"}, "currentValue": "0", "limit": str(quota_limit)},
                    {"name": {"value": "cores"}, "currentValue": "0", "limit": "14"},
                ]
            if command == ("vm", "run-command"):
                return {"value": [{"message": "SPOTFLOWD_HOST_KEY ssh-ed25519 AAAA host"}]}
            if command == ("vm", "show"):
                return {"publicIps": "192.0.2.1"}
            return None

        self.runner.az = Mock(side_effect=response)
        self.runner.ssh = Mock(return_value=subprocess.CompletedProcess([], 0, "", ""))
        self.runner.provision()
        return [call.args for call in self.runner.az.call_args_list]

    def test_first_run_attaches_separately_created_disk_with_detach_policy(self):
        calls = self.provision()
        create = next(call for call in calls if call[:2] == ("vm", "create"))
        self.assertEqual(create[create.index("--attach-data-disks") + 1], "/disks/persistent-cache")
        self.assertEqual(create[create.index("--data-disk-delete-option") + 1], "Detach")
        self.assertTrue(any(call[:2] == ("disk", "create") for call in calls))
        self.assertIn("StrictHostKeyChecking=yes", self.runner.ssh_options)
        disk = next(call for call in calls if call[:2] == ("disk", "create"))
        self.assertEqual(disk[disk.index("--size-gb") + 1], "256")
        self.assertEqual(disk[disk.index("--sku") + 1], "StandardSSD_LRS")
        self.assertEqual(create[create.index("--os-disk-size-gb") + 1], "32")
        self.assertEqual(create[create.index("--storage-sku") + 1], "os=StandardSSD_LRS")
        self.assertEqual(create[create.index("--security-type") + 1], "TrustedLaunch")
        self.assertEqual(create[create.index("--enable-secure-boot") + 1], "true")
        self.assertEqual(create[create.index("--enable-vtpm") + 1], "true")
        self.assertEqual(create[create.index("--disk-controller-type") + 1], "SCSI")

    def test_failed_deployment_network_is_reused_on_retry(self):
        calls = self.provision(existing_nic=True)
        create = next(call for call in calls if call[:2] == ("vm", "create"))
        self.assertEqual(create[create.index("--nics") + 1], "/nics/existing")
        self.assertNotIn("--public-ip-sku", create)

    def test_custom_disk_capacity_and_tier(self):
        self.args.disk_size_gb = 128
        self.args.disk_sku = "Premium_LRS"
        calls = self.provision()
        disk = next(call for call in calls if call[:2] == ("disk", "create"))
        self.assertEqual(disk[disk.index("--size-gb") + 1], "128")
        self.assertEqual(disk[disk.index("--sku") + 1], "Premium_LRS")

    def test_rerun_reuses_disk_and_vm(self):
        calls = self.provision(existing=True)
        self.assertFalse(any(call[:2] in (("disk", "create"), ("vm", "create")) for call in calls))
        self.assertTrue(any(call[:2] == ("vm", "start") for call in calls))
        self.assertFalse(any(call[:2] == ("vm", "list-usage") for call in calls))

    def test_quota_failure_does_not_create_group_disk_or_vm(self):
        with self.assertRaisesRegex(RuntimeError, "Insufficient quota"):
            self.provision(quota_limit=0)
        calls = [call.args[:2] for call in self.runner.az.call_args_list]
        self.assertNotIn(("group", "create"), calls)
        self.assertNotIn(("disk", "create"), calls)
        self.assertNotIn(("vm", "create"), calls)

    def test_active_run_resumes_without_overwriting_source(self):
        self.runner.state = self.state
        (self.state / "pending-run").write_text("previous-run")
        self.runner.active = Mock(return_value=True)
        self.runner.transfer = Mock()
        self.assertEqual(self.runner.start(), "previous-run")
        self.runner.transfer.assert_not_called()

    def test_completed_run_is_collected_after_monitoring_interruption(self):
        self.runner.state = self.state
        (self.state / "pending-run").write_text("previous-run")
        self.runner.active = Mock(return_value=False)
        self.runner.ssh = Mock(return_value=subprocess.CompletedProcess([], 0, "", ""))
        self.runner.transfer = Mock()
        self.assertEqual(self.runner.start(), "previous-run")
        self.runner.transfer.assert_not_called()

    def test_failed_build_collects_results_and_deallocates_without_deleting_disk(self):
        self.runner.provision = Mock()
        self.runner.start = Mock(return_value="test-run")
        self.runner.wait = Mock(return_value=1)
        self.runner.finish = Mock()
        self.runner.az = Mock()
        self.assertEqual(self.runner.execute(), 1)
        self.runner.finish.assert_called_once_with("test-run")
        self.assertEqual(self.runner.az.call_args.args[:2], ("vm", "deallocate"))

    def test_lost_monitoring_connection_keeps_vm_running(self):
        self.runner.provision = Mock()
        self.runner.start = Mock(return_value="test-run")
        self.runner.wait = Mock(side_effect=RuntimeError("SSH connection lost"))
        self.runner.finish = Mock()
        self.runner.deallocate = Mock()
        with self.assertRaises(RuntimeError):
            self.runner.execute()
        self.runner.deallocate.assert_not_called()
        self.runner.finish.assert_not_called()


class CapacityChecks(unittest.TestCase):
    def setUp(self):
        self.runner = azure.AzureRunner(azure.parse_args([]))
        self.sku = {"name": "Standard_E4as_v5", "family": "standardEASv5Family", "restrictions": [],
                    "capabilities": [{"name": "vCPUs", "value": "4"}, {"name": "MemoryGB", "value": "32"}]}
        self.usage = [
            {"name": {"value": "standardEASv5Family", "localizedValue": "Standard EASv5 Family vCPUs"},
             "currentValue": 0, "limit": 10},
            {"name": {"value": "cores", "localizedValue": "Total Regional vCPUs"}, "currentValue": 0, "limit": 14},
        ]

        def response(*args):
            if args[:2] == ("account", "show"):
                return {"id": "test-subscription", "name": "Test"}
            return [self.sku] if args[:2] == ("vm", "list-skus") else self.usage

        self.runner.az = Mock(side_effect=response)

    def test_checks_used_family_quota(self):
        self.usage[0]["currentValue"] = 7
        with self.assertRaisesRegex(RuntimeError, "usage 7, limit 10, 4 additional"):
            self.runner.check_vm_capacity("westeurope")

    def test_checks_total_regional_quota(self):
        self.usage[1]["currentValue"] = 12
        with self.assertRaisesRegex(RuntimeError, "Total Regional vCPUs"):
            self.runner.check_vm_capacity("westeurope")

    def test_zone_only_restriction_allows_non_zonal_vm(self):
        self.sku["restrictions"] = [{"type": "Zone", "reasonCode": "NotAvailableForSubscription"}]
        self.runner.check_vm_capacity("westeurope")

    def test_location_restriction_is_rejected(self):
        self.sku["restrictions"] = [{"type": "Location", "reasonCode": "NotAvailableForSubscription"}]
        with self.assertRaisesRegex(RuntimeError, "restricted"):
            self.runner.check_vm_capacity("westeurope")

    def test_vm_without_trusted_launch_support_is_rejected(self):
        self.sku["capabilities"].append({"name": "TrustedLaunchDisabled", "value": "True"})
        with self.assertRaisesRegex(RuntimeError, "does not support Trusted Launch"):
            self.runner.check_vm_capacity("westeurope")

    def test_read_only_check_does_not_provision(self):
        self.assertEqual(self.runner.check_capacity_only(), 0)
        self.assertIsNone(self.runner.state)
        self.assertEqual([call.args[:2] for call in self.runner.az.call_args_list], [
            ("account", "show"), ("vm", "list-skus"), ("vm", "list-usage"),
        ])

    def test_extracts_quota_cause_from_azure_cli_secondary_traceback(self):
        stderr = (
            "ERROR: The content for this response was already consumed\n"
            "(QuotaExceeded) Operation could not be completed as it results in exceeding approved "
            "standardEASv5Family Cores quota. Additional details - Deployment Model: Resource Manager, "
            "Location: westeurope, Current Limit: 0, Current Usage: 0, Additional Required: 4, "
            "(Minimum) New Limit Required: 4.\nRuntimeError: The content for this response was already consumed"
        )
        message = azure.azure_error(subprocess.CalledProcessError(1, ["az", "vm", "create"], stderr=stderr))
        self.assertIn("standardEASv5Family in westeurope", message)
        self.assertIn("usage 0, limit 0", message)
        self.assertNotIn("response was already consumed", message)

    def test_nested_deployment_error_reports_cause_without_region_price_hint(self):
        stderr = 'Selecting "uksouth" may reduce your costs.\nERROR: ' + azure.json.dumps({
            "status": "Failed", "error": {"code": "DeploymentFailed", "message": "Generic deployment failure",
                                          "details": [{"code": "BadRequest", "message": "Unsupported securityType"}]},
        })
        message = azure.azure_error(subprocess.CalledProcessError(1, ["az", "vm", "create"], stderr=stderr))
        self.assertEqual(message, "BadRequest: Unsupported securityType")


class HostSetup(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        tools = root / "bin"
        tools.mkdir()
        self.calls = root / "calls"
        self.marker = root / "host-ready"
        self.env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                        FAKE_CALLS=str(self.calls), FAKE_CLOUD_READY=str(root / "cloud-ready"))
        for name, script in {
            "cloud-init": (
                '[[ "$*" == "status --wait" ]] || exit 1\n'
                'echo CLOUD_INIT_WAIT >> "$FAKE_CALLS"\n'
                '[[ "${FAKE_CLOUD_FAIL:-0}" == 0 ]] || exit 1\n'
                'touch "$FAKE_CLOUD_READY"\n'
            ),
            "apt-get": (
                # Simulate cloud-init making the original mirror's indexes unusable.
                '[[ -e "$FAKE_CLOUD_READY" ]] || { echo "Mirror configuration is not ready" >&2; exit 1; }\n'
                'printf "APT %s\\n" "$*" >> "$FAKE_CALLS"\n'
            ),
            "locale-gen": 'echo LOCALE >> "$FAKE_CALLS"\n',
        }.items():
            tool = tools / name
            tool.write_text(f"#!{shutil.which('bash')}\nset -eu\n" + script)
            tool.chmod(0o755)
        # Execute the real package-bootstrap stage, without touching host disks.
        self.script = (ROOT / "yocto/azure-setup.sh").read_text().split("# LUN 0", 1)[0]
        self.script = self.script.replace("/var/lib/spotflowd-yocto-host-v3", str(self.marker))

    def test_first_boot_waits_for_repository_setup_and_rerun_skips_packages(self):
        for _ in range(2):
            subprocess.run(["bash", "-c", self.script], env=self.env,
                           capture_output=True, text=True, check=True)
        calls = self.calls.read_text().splitlines()
        self.assertEqual(calls[0], "CLOUD_INIT_WAIT")
        self.assertEqual(calls.count("CLOUD_INIT_WAIT"), 2)
        self.assertEqual(calls.count("APT update"), 1)
        self.assertEqual(sum(call.startswith("APT install ") for call in calls), 1)
        self.assertTrue(self.marker.exists())

    def test_failed_cloud_init_does_not_install_packages_or_mark_host_ready(self):
        self.env["FAKE_CLOUD_FAIL"] = "1"
        result = subprocess.run(["bash", "-c", self.script], env=self.env,
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls.read_text().splitlines(), ["CLOUD_INIT_WAIT"])
        self.assertFalse(self.marker.exists())


class HostPython(unittest.TestCase):
    def test_isolated_python_is_installed_and_selected_and_reused(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            tools = root / "bin"
            tools.mkdir()
            venv = root / "host-venv"
            calls = root / "calls"
            # Fake only the expensive venv/pip operations; run the real helper.
            python = tools / "python3"
            python.write_text(
                f"#!{shutil.which('bash')}\nset -eu\n"
                'printf "CREATE %s\\n" "$*" >> "$FAKE_CALLS"\n'
                '[[ "$1 $2 $3" == "-m venv --system-site-packages" ]]\n'
                'mkdir -p "$4/bin"\n'
                'cp "$FAKE_VENV_PYTHON" "$4/bin/python3"\n'
            )
            python.chmod(0o755)
            venv_python = root / "venv-python"
            venv_python.write_text(
                f"#!{shutil.which('bash')}\nset -eu\n"
                'printf "VENV %s\\n" "$*" >> "$FAKE_CALLS"\n'
                '[[ "${FAKE_PIP_FAIL:-0}" == 0 ]]\n'
            )
            venv_python.chmod(0o755)
            env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                       FAKE_CALLS=str(calls), FAKE_VENV_PYTHON=str(venv_python))
            command = ['bash', '-c', 'source "$1" "$2"; python3 SELECTED',
                       'test', str(ROOT / "yocto/host-python.sh"), str(venv)]
            for _ in range(2):
                subprocess.run(command, env=env, capture_output=True, text=True, check=True)
            lines = calls.read_text().splitlines()
            self.assertEqual(sum(line.startswith("CREATE ") for line in lines), 1)
            self.assertEqual(lines.count("VENV SELECTED"), 2)
            self.assertEqual(sum("--requirement" in line and "host-requirements.txt" in line for line in lines), 2)
            env["FAKE_PIP_FAIL"] = "1"
            result = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls.read_text().splitlines().count("VENV SELECTED"), 2)


class RemoteBuild(unittest.TestCase):
    def test_cached_image_is_boot_tested_again_without_readding_layer(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            poky = root / "poky"
            (poky / ".git").mkdir(parents=True)
            (poky / "oe-init-build-env").write_text(
                'mkdir -p "$1/conf"\n'
                'touch "$1/conf/local.conf"\n'
                'cd "$1" || return 1\n'
            )
            tools = root / "bin"
            tools.mkdir()
            venv_bin = root / "cache/host-venv/bin"
            venv_bin.mkdir(parents=True)
            python = venv_bin / "python3"
            python.write_text(f"#!{shutil.which('bash')}\nexit 0\n")
            python.chmod(0o755)
            for name, script in {
                "bitbake": (
                    'if [[ "${YOCTO_SSTATE_MIRROR:-}" != none ]]; then\n'
                    '[[ "$(command -v python3)" == "$YOCTO_CACHE/host-venv/bin/python3" ]] || exit 1\n'
                    'fi\n'
                    'printf "%s\\n" "$*" >> "$FAKE_CALLS"\n'
                ),
                "bitbake-layers": (
                    'case "$1" in\n'
                    'show-layers) [[ ! -f conf/layer ]] || cat conf/layer ;;\n'
                    'add-layer) printf "%s\\n" "$2" > conf/layer; echo ADD_LAYER >> "$FAKE_CALLS" ;;\n'
                    'esac\n'
                ),
            }.items():
                tool = tools / name
                tool.write_text(f"#!{shutil.which('bash')}\n" + script)
                tool.chmod(0o755)
            calls = root / "calls"
            env = dict(os.environ, POKY_DIR=str(poky), YOCTO_BUILD_ROOT=str(root / "build"),
                       YOCTO_CACHE=str(root / "cache"), YOCTO_RELEASE="scarthgap", YOCTO_INIT="systemd",
                       PATH=str(tools) + os.pathsep + os.environ["PATH"], FAKE_CALLS=str(calls))
            for _ in range(2):
                subprocess.run(["bash", str(ROOT / "yocto/ci-image.sh")], env=env,
                               capture_output=True, text=True, check=True)
            commands = calls.read_text().splitlines()
            self.assertEqual(commands.count("ADD_LAYER"), 1)
            self.assertEqual(commands.count("core-image-minimal"), 2)
            self.assertEqual(commands.count("-c testimage core-image-minimal"), 2)
            conf = (root / "build/conf/spotflowd-test.conf").read_text()
            self.assertIn(f'DL_DIR = "{root}/cache/downloads"', conf)
            self.assertIn('QEMU_USE_KVM = ""', conf)
            self.assertIn('INHERIT:remove = "create-spdx"', conf)
            self.assertIn('https://sstate.yoctoproject.org/all/PATH', conf)
            self.assertIn('BB_HASHSERVE_UPSTREAM = "wss://hashserv.yoctoproject.org/ws"', conf)
            env["YOCTO_SSTATE_MIRROR"] = "none"
            subprocess.run(["bash", str(ROOT / "yocto/ci-image.sh")], env=env,
                           capture_output=True, text=True, check=True)
            self.assertNotIn("BB_HASHSERVE_UPSTREAM", (root / "build/conf/spotflowd-test.conf").read_text())

    def test_matrix_continues_after_failure_and_reuses_persistent_cache(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "source/yocto"
            source.mkdir(parents=True)
            shutil.copy2(ROOT / "yocto/cache-prune.py", source)
            # Stand-in for the expensive image builder; use the real Azure matrix
            # orchestration and inspect its persistent cache and captured logs.
            (source / "ci-image.sh").write_text(
                '#!/bin/bash\nset -eu\n'
                'mkdir -p "$YOCTO_CACHE/downloads" "$YOCTO_CACHE/sstate" "$YOCTO_BUILD_ROOT/tmp/log"\n'
                'if [[ -e "$YOCTO_CACHE/downloads/retained" ]]; then echo CACHE_HIT; fi\n'
                'touch "$YOCTO_CACHE/downloads/retained" "$YOCTO_CACHE/sstate/retained"\n'
                'echo "test result" > "$YOCTO_BUILD_ROOT/tmp/log/runtime.log"\n'
                'mkdir -p "$YOCTO_BUILD_ROOT/tmp/work/qemuarm64-poky-linux/core-image-minimal/1.0/testimage"\n'
                'echo "guest boot" > "$YOCTO_BUILD_ROOT/tmp/work/qemuarm64-poky-linux/core-image-minimal/1.0/testimage/qemu_boot_log"\n'
                '[[ "$YOCTO_INIT" != systemd ]]\n'
            )
            env = dict(os.environ, SPOTFLOWD_AZURE_ROOT=temp)
            for run_id in ("first-run", "second-run"):
                result = subprocess.run(
                    ["bash", str(ROOT / "yocto/azure-build.sh"), "all", "both", "4", run_id],
                    env=env, capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                results = root / "runs" / run_id
                self.assertEqual((results / "exit-code").read_text().strip(), "1")
                self.assertEqual((results / "summary.txt").read_text().splitlines(), [
                    "scarthgap-systemd FAIL (exit 1)", "scarthgap-sysvinit PASS",
                    "styhead-systemd FAIL (exit 1)", "styhead-sysvinit PASS",
                ])
                self.assertTrue((results / "styhead-sysvinit/log/runtime.log").exists())
            self.assertIn("CACHE_HIT", (root / "runs/second-run/build.log").read_text())
            self.assertTrue((root / "cache/sstate/retained").exists())
            self.assertFalse((root / "build-styhead-sysvinit/tmp").exists())
            self.assertTrue((root / "build-styhead-systemd/tmp").exists())
            self.assertTrue((root / "runs/second-run/disk-usage.txt").exists())
            self.assertEqual((root / "runs/second-run/styhead-sysvinit/testimage/qemu_boot_log").read_text(), "guest boot\n")
            subprocess.run(
                ["bash", str(ROOT / "yocto/azure-build.sh"), "scarthgap", "sysvinit", "4",
                 "keep-work-run", "64", "1", "0"], env=env, capture_output=True, text=True, check=True,
            )
            self.assertTrue((root / "build-scarthgap-sysvinit/tmp").exists())


class CachePruning(unittest.TestCase):
    def test_evicts_old_archives_and_sidecars_but_keeps_recent_hits(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            for name, modified in (("old", 10), ("recent", 20)):
                archive = cache / f"{name}.tar.zst"
                for suffix in ("", ".siginfo", ".sig"):
                    path = Path(str(archive) + suffix)
                    path.write_bytes(b"1234")
                    os.utime(path, (modified, modified))
            unrelated = cache / "other-state"
            unrelated.write_bytes(b"keep this")
            outside = cache.parent / f"outside-{cache.name}.tar.zst"
            outside.write_bytes(b"keep this too")
            try:
                (cache / "linked.tar.zst").symlink_to(outside)
                self.assertEqual(cache_prune.prune(cache, 12), 12)
                self.assertFalse((cache / "old.tar.zst.siginfo").exists())
                self.assertTrue((cache / "recent.tar.zst").exists())
                self.assertTrue(unrelated.exists())
                self.assertTrue(outside.exists())
                self.assertEqual(cache_prune.prune(cache, 12), 0)
            finally:
                outside.unlink()

    def test_pruning_can_be_disabled_and_a_missing_cache_is_ok(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            archive = cache / "old.tar.zst"
            archive.write_bytes(b"data")
            self.assertEqual(cache_prune.prune(cache, 0), 0)
            self.assertTrue(archive.exists())
            self.assertEqual(cache_prune.prune(cache / "not-created", 1), 0)


if __name__ == "__main__":
    unittest.main()
