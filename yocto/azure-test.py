#!/usr/bin/env python3
"""Provision/reuse an Azure VM, run Yocto QEMU tests, and retain the build cache."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]
REMOTE = "/srv/spotflowd"
VM = "spotflowd-yocto"
DISK = "spotflowd-yocto-cache"
USER = "spotflow"
UNIT = "spotflowd-yocto-build.service"
TAG = "spotflowd-yocto"


def run(command, **kwargs):
    return subprocess.run(command, check=True, **kwargs)


def azure_error(error):
    detail = getattr(error, "stderr", None) or str(error)
    # Some Azure CLI versions obscure deployment validation failures with a
    # second "response already consumed" traceback. Keep the actionable cause.
    if "QuotaExceeded" in detail:
        fields = {}
        for name, pattern in {
            "family": r"exceeding approved (\w+) Cores quota",
            "location": r"Location: ([^,\n]+)",
            "limit": r"Current Limit: (\d+)",
            "usage": r"Current Usage: (\d+)",
            "required": r"Additional Required: (\d+)",
        }.items():
            match = re.search(pattern, detail)
            if match:
                fields[name] = match[1]
        if len(fields) == 5:
            minimum = int(fields["usage"]) + int(fields["required"])
            return (f"Azure quota exceeded for {fields['family']} in {fields['location']}: "
                    f"usage {fields['usage']}, limit {fields['limit']}, "
                    f"{fields['required']} additional vCPUs required. "
                    f"Request a family quota of at least {minimum} in Azure Portal → Quotas → "
                    "Microsoft.Compute, or choose a VM size with available quota.")
    # ARM deployment failures wrap the useful message in nested details, often
    # preceded by an unrelated Azure CLI region-price recommendation.
    match = re.search(r"ERROR:\s*(\{)", detail)
    if match:
        try:
            payload, _ = json.JSONDecoder().raw_decode(detail[match.start(1):])

            def messages(value):
                if "error" in value:
                    return messages(value["error"])
                if value.get("details"):
                    return [message for child in value["details"] for message in messages(child)]
                return [f"{value.get('code', 'AzureError')}: {value['message']}"] if "message" in value else []

            causes = messages(payload)
            if causes:
                return "\n".join(causes)
        except (json.JSONDecodeError, TypeError, KeyError):
            pass
    return detail


class AzureRunner:
    def __init__(self, args):
        self.args = args
        self.subscription = args.subscription
        self.state = None
        self.ssh_options = []
        self.destination = None

    def az(self, *arguments):
        command = ["az", *arguments, "--only-show-errors", "--output", "json"]
        if self.subscription:
            command += ["--subscription", self.subscription]
        result = run(command, capture_output=True, text=True)
        return json.loads(result.stdout) if result.stdout.strip() else None

    def ssh(self, command, *, check=True, text=True):
        return subprocess.run(
            ["ssh", *self.ssh_options, self.destination, command], check=check,
            capture_output=True, text=text, timeout=90,
        )

    def active(self):
        return self.ssh(f"systemctl is-active --quiet {UNIT}", check=False).returncode == 0

    def check_vm_capacity(self, location):
        size = self.args.size
        skus = self.az("vm", "list-skus", "--location", location, "--resource-type", "virtualMachines",
                       "--size", size, "--all")
        sku = next((sku for sku in skus if sku["name"] == size), None)
        if sku is None:
            raise RuntimeError(f"VM size {size} was not found in {location}")
        # A zone-only restriction does not block this non-zonal deployment.
        if any(restriction.get("type") == "Location" for restriction in sku.get("restrictions", [])):
            raise RuntimeError(f"VM size {size} is restricted for this subscription in {location}; "
                               "choose another --size or region")
        capabilities = {entry["name"]: entry["value"] for entry in sku["capabilities"]}
        if capabilities.get("CpuArchitectureType", "x64") != "x64":
            raise RuntimeError(f"{size} is not x86-64; this runner uses an x86-64 Ubuntu host image")
        if capabilities.get("TrustedLaunchDisabled", "false").lower() == "true":
            raise RuntimeError(f"{size} does not support Trusted Launch; choose a compatible --size")
        if "HyperVGenerations" in capabilities and "V2" not in capabilities["HyperVGenerations"].split(","):
            raise RuntimeError(f"{size} does not support the required Generation 2 Ubuntu image")
        if "DiskControllerTypes" in capabilities and "SCSI" not in capabilities["DiskControllerTypes"].split(","):
            raise RuntimeError(f"{size} does not support the SCSI controller used by the cache-disk setup")
        required = int(capabilities["vCPUs"])
        usages = self.az("vm", "list-usage", "--location", location)
        quotas = {entry["name"]["value"].lower(): entry for entry in usages}
        for key in (sku["family"].lower(), "cores"):
            quota = quotas.get(key)
            if quota is None:
                raise RuntimeError(f"Cannot determine Azure vCPU quota {key} in {location}")
            limit = int(quota["limit"])
            usage = int(quota["currentValue"])
            label = quota["name"].get("localizedValue", key)
            if limit - usage < required:
                raise RuntimeError(
                    f"Insufficient quota for {size} in {location}: {label} ({key}), "
                    f"usage {usage}, limit {limit}, {required} additional vCPUs required. "
                    f"Request a quota of at least {usage + required} in Azure Portal → Quotas → "
                    "Microsoft.Compute, or choose another --size with available quota. "
                    "No new resources were created."
                )
        print(f"Capacity preflight passed: {size}, {required} vCPUs, "
              f"{capabilities.get('MemoryGB', '?')} GiB RAM, {location}. "
              "Deployment still depends on Azure availability and policy.", flush=True)

    def check_capacity_only(self):
        account = self.az("account", "show")
        self.subscription = account["id"]
        print(f"Azure subscription: {account['name']} ({self.subscription})", flush=True)
        self.check_vm_capacity(self.args.location)
        return 0

    def provision(self):
        account = self.az("account", "show")
        self.subscription = account["id"]
        print(f"Azure subscription: {account['name']} ({self.subscription})", flush=True)
        base = Path(self.args.state_dir).expanduser()
        self.state = base / self.subscription / self.args.group
        self.state.mkdir(parents=True, exist_ok=True, mode=0o700)
        key = self.state / "id_ed25519"
        if not key.exists():
            run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)])

        group_args = ["--resource-group", self.args.group]
        group_exists = self.az("group", "exists", "--name", self.args.group)
        if group_exists:
            group = self.az("group", "show", "--name", self.args.group)
            if group.get("tags", {}).get(TAG) != "true":
                raise RuntimeError(f"Resource group {self.args.group} is not owned by this runner")
            machines = self.az("vm", "list", *group_args)
            vm = next((machine for machine in machines if machine["name"] == VM), None)
        else:
            group = {"location": self.args.location}
            vm = None
        # Check before creating either a resource group or a cache disk. An
        # existing VM is reused as-is and must not be counted as new quota usage.
        if vm is None:
            self.check_vm_capacity(group["location"])
        if not group_exists:
            group = self.az("group", "create", "--name", self.args.group,
                            "--location", self.args.location, "--tags", f"{TAG}=true")

        disks = self.az("disk", "list", *group_args)
        disk = next((disk for disk in disks if disk["name"] == DISK), None)
        if disk is None:
            print(f"Creating persistent {self.args.disk_size_gb} GiB {self.args.disk_sku} build/cache disk…", flush=True)
            disk = self.az("disk", "create", *group_args, "--name", DISK,
                           "--location", group["location"], "--size-gb", str(self.args.disk_size_gb),
                           "--sku", self.args.disk_sku, "--tags", f"{TAG}=true")
        else:
            print(f"Reusing cache disk: {disk.get('diskSizeGB', disk.get('diskSizeGb', 'unknown'))} GiB, "
                  f"{disk.get('sku', {}).get('name', 'unknown')}. Existing disks are not resized.", flush=True)

        if vm is None:
            # A failed ARM deployment may have created networking before failing
            # the VM. Reuse the existing NIC (and its public IP/NSG) on retry.
            nics = self.az("network", "nic", "list", *group_args)
            nic = next((nic for nic in nics if nic["name"] == f"{VM}VMNic"), None)
            network = ["--nics", nic["id"]] if nic else ["--public-ip-sku", "Standard"]
            print(f"Creating Ubuntu 22.04 VM ({self.args.size})…", flush=True)
            self.az("vm", "create", *group_args, "--name", VM,
                    "--location", group["location"], "--size", self.args.size,
                    "--image", "Canonical:0001-com-ubuntu-server-jammy:22_04-lts-gen2:latest",
                    "--admin-username", USER, "--ssh-key-values", key.with_suffix(".pub").read_text().strip(),
                    "--attach-data-disks", disk["id"], "--data-disk-delete-option", "Detach",
                    "--os-disk-delete-option", "Delete", "--os-disk-size-gb", "32",
                    "--storage-sku", "os=StandardSSD_LRS",
                    "--security-type", "TrustedLaunch", "--enable-secure-boot", "true", "--enable-vtpm", "true",
                    "--disk-controller-type", "SCSI", *network,
                    "--tags", f"{TAG}=true")
        else:
            data_disks = vm["storageProfile"]["dataDisks"]
            if not any(entry["managedDisk"]["id"].lower() == disk["id"].lower()
                       and entry["lun"] == 0 and entry.get("deleteOption") == "Detach"
                       for entry in data_disks):
                raise RuntimeError("VM must have the retained cache disk attached at LUN 0 with deleteOption=Detach")
            self.az("vm", "start", *group_args, "--name", VM)
            # A second workstation may have a different local SSH key.
            self.az("vm", "user", "update", *group_args, "--name", VM,
                    "--username", USER, "--ssh-key-value", key.with_suffix(".pub").read_text().strip())

        print("Preparing build host and mounting the persistent disk…", flush=True)
        setup = self.az("vm", "run-command", "invoke", *group_args, "--name", VM,
                        "--command-id", "RunShellScript", "--scripts",
                        "bash <<'SPOTFLOWD_SETUP'\n" + (ROOT / "yocto/azure-setup.sh").read_text()
                        + "\nSPOTFLOWD_SETUP")
        messages = "\n".join(value.get("message", "") for value in setup["value"])
        match = re.search(r"SPOTFLOWD_HOST_KEY (ssh-ed25519 [A-Za-z0-9+/=]+)", messages)
        if not match:
            raise RuntimeError(f"VM setup failed:\n{messages}")
        alias = f"{self.args.group}-{VM}"
        known_hosts = self.state / "known_hosts"
        known_hosts.write_text(f"{alias} {match[1]}\n")
        details = self.az("vm", "show", *group_args, "--name", VM, "--show-details")
        self.destination = f"{USER}@{details['publicIps']}"
        self.ssh_options = ["-i", str(key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
                            "-o", "ConnectTimeout=15", "-o", "StrictHostKeyChecking=yes",
                            "-o", f"HostKeyAlias={alias}", "-o", f"UserKnownHostsFile={known_hosts}"]
        for _ in range(30):
            if self.ssh("true", check=False).returncode == 0:
                return
            time.sleep(10)
        raise RuntimeError("VM is running, but SSH did not become available")

    def transfer(self, source, destination):
        run(["rsync", "-a", "--delete", "-e", shlex.join(["ssh", *self.ssh_options]),
             str(source), str(destination)])

    def start(self):
        pending = self.state / "pending-run"
        if pending.exists():
            run_id = pending.read_text().strip()
            if not re.fullmatch(r"[a-zA-Z0-9_-]+", run_id):
                raise RuntimeError("Invalid pending run ID")
            if self.active() or self.ssh(f"test -f {REMOTE}/runs/{run_id}/exit-code", check=False).returncode == 0:
                print(f"Resuming run {run_id}", flush=True)
                return run_id
        if self.active():
            raise RuntimeError("A build is already running; resume it using the original local state directory")

        # Upload current build inputs, including uncommitted files. Git history,
        # root-level local config, build output and prior results are excluded.
        self.ssh(f"mkdir -p {REMOTE}/source/yocto")
        for name in ("Cargo.toml", "Cargo.lock", "LICENSE.MD"):
            self.transfer(ROOT / name, f"{self.destination}:{REMOTE}/source/{name}")
        self.transfer(f"{ROOT}/src/", f"{self.destination}:{REMOTE}/source/src/")
        run(["rsync", "-a", "--delete", "--exclude", "__pycache__/", "--exclude", "results/",
             "-e", shlex.join(["ssh", *self.ssh_options]), f"{ROOT}/yocto/",
             f"{self.destination}:{REMOTE}/source/yocto/"])
        run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8]
        self.ssh(f"mkdir -p {REMOTE}/runs/{run_id}")
        self.ssh(f"sudo systemctl reset-failed {UNIT}", check=False)
        command = ["sudo", "systemd-run", f"--unit={UNIT}", f"--uid={USER}", f"--gid={USER}",
                   f"--property=WorkingDirectory={REMOTE}/source", "bash",
                   f"{REMOTE}/source/yocto/azure-build.sh", self.args.release, self.args.init,
                   str(self.args.threads), run_id, str(self.args.cache_max_gb),
                   "1" if self.args.keep_work else "0", "0" if self.args.no_sstate_mirror else "1"]
        # Persist before launch: an SSH drop after systemd accepts the job must
        # not lose the ID needed to resume it.
        pending.write_text(run_id)
        self.ssh(shlex.join(command))
        print(f"Started {run_id}. Re-run this command to resume after an interruption.", flush=True)
        return run_id

    def wait(self, run_id):
        offset = 0
        failures = 0
        directory = f"{REMOTE}/runs/{run_id}"
        while True:
            try:
                reader = (
                    "from pathlib import Path; import sys; "
                    f"p=Path('{directory}/build.log'); "
                    "f=p.open('rb') if p.exists() else None; "
                    f"f.seek({offset}) if f else None; "
                    "sys.stdout.buffer.write(f.read(262144) if f else b'')"
                )
                result = self.ssh("python3 -c " + shlex.quote(reader), text=False)
                offset += len(result.stdout)
                print(result.stdout.decode("utf-8", errors="replace"), end="", flush=True)
                completion = self.ssh(f"cat {directory}/exit-code", check=False)
                if completion.returncode == 0:
                    return int(completion.stdout.strip())
                if not self.active():
                    raise RuntimeError(f"Build service stopped without a result; inspect journalctl -u {UNIT}")
                failures = 0
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
                failures += 1
                if failures >= 5:
                    raise RuntimeError("Lost SSH connection; the build continues. Re-run to resume.") from error
            time.sleep(15)

    def finish(self, run_id):
        results = ROOT / "yocto/results" / run_id
        results.mkdir(parents=True, exist_ok=True)
        self.transfer(f"{self.destination}:{REMOTE}/runs/{run_id}/", f"{results}/")
        print(f"\nResults: {results}", flush=True)
        summary = results / "summary.txt"
        if summary.exists():
            print(summary.read_text(), end="", flush=True)
        (self.state / "pending-run").unlink(missing_ok=True)

    def deallocate(self):
        print("Deallocating VM; managed disk, build directories and caches are retained.", flush=True)
        self.az("vm", "deallocate", "--resource-group", self.args.group, "--name", VM)

    def execute(self):
        self.provision()
        run_id = self.start()
        code = self.wait(run_id)
        self.finish(run_id)
        if not self.args.keep_running:
            self.deallocate()
        return code


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--subscription", help="Azure subscription ID or name (defaults to the active account)")
    parser.add_argument("--group", default="spotflowd-yocto-test", help="dedicated resource group to create/reuse")
    parser.add_argument("--location", default="westeurope", help="region for a new resource group")
    parser.add_argument("--size", default="Standard_E4as_v5", help="VM size for a new VM (default: Standard_E4as_v5)")
    parser.add_argument("--release", choices=("scarthgap", "styhead", "all"), default="scarthgap")
    parser.add_argument("--init", choices=("systemd", "sysvinit", "both"), default="both")
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--disk-size-gb", type=int, default=256, help="capacity of a new cache disk (default: 256 GiB)")
    parser.add_argument("--disk-sku", choices=("StandardSSD_LRS", "Premium_LRS"), default="StandardSSD_LRS",
                        help="storage tier of a new cache disk (default: StandardSSD_LRS)")
    parser.add_argument("--cache-max-gb", type=int, default=64, help="retained sstate limit in GiB; 0 disables pruning")
    parser.add_argument("--keep-work", action="store_true", help="retain successful builds' temporary directories")
    parser.add_argument("--no-sstate-mirror", action="store_true", help="build without the Yocto public sstate mirror")
    parser.add_argument("--check-capacity", action="store_true", help="check VM SKU and vCPU quotas without provisioning")
    parser.add_argument("--keep-running", action="store_true", help="leave VM allocated after testing")
    parser.add_argument("--state-dir", default=str(Path(os.environ.get("XDG_STATE_HOME", "~/.local/state"))
                                                / "spotflowd/azure"), help="local SSH keys and resume state")
    args = parser.parse_args(argv)
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", args.group) or args.threads < 1:
        parser.error("group must contain only letters, numbers, underscores and hyphens; threads must be positive")
    if args.disk_size_gb < 64 or args.cache_max_gb < 0:
        parser.error("disk-size-gb must be at least 64; cache-max-gb must be nonnegative")
    if args.cache_max_gb >= args.disk_size_gb:
        parser.error("cache-max-gb must leave room on the disk for downloads and build intermediates")
    return args


def main():
    args = parse_args()
    tools = ("az",) if args.check_capacity else ("az", "ssh", "ssh-keygen", "rsync")
    for executable in tools:
        if shutil.which(executable) is None:
            sys.exit(f"Missing {executable}; install Azure CLI, OpenSSH and rsync first")
    # Catch an outdated crate list before allocating the VM.
    if not args.check_capacity:
        run([sys.executable, str(ROOT / "yocto/update-cargo.py"), "--check"])
    try:
        runner = AzureRunner(args)
        return runner.check_capacity_only() if args.check_capacity else runner.execute()
    except KeyboardInterrupt:
        print("\nMonitoring interrupted. VM/cache are retained; re-run to resume.", file=sys.stderr)
        return 130
    except (RuntimeError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        print(f"Error: {azure_error(error)}\nExisting resources and cache are retained.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
