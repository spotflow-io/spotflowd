# Automated Azure test

This directory contains the Azure-specific Yocto test tooling:

- `test.py`: local controller for provisioning, uploading sources, monitoring,
  collecting results, and deallocating the VM.
- `setup.sh`: host dependency installation and persistent-disk setup, run as root.
- `build.sh`: remote build/test matrix runner, run as the `spotflow` user.
- `cache-prune.py`: sstate cache cleanup between remote builds.

The runner uses the shared image builder at [`../ci-image.sh`](../ci-image.sh).
See the [Yocto integration guide](../README.md) for layer setup and local checks.

With Python 3.11+, Azure CLI, OpenSSH and rsync installed, and Azure CLI signed in
(`az login` once), run from the repository root. Replace `YOUR_SUBSCRIPTION_ID`
with your Azure subscription ID; these are the recommended starting values:

```sh
python3 yocto/azure/test.py \
  --subscription YOUR_SUBSCRIPTION_ID \
  --group spotflowd-yocto-test \
  --location westeurope \
  --size Standard_E4as_v5 \
  --threads 4 \
  --disk-size-gb 256 \
  --disk-sku StandardSSD_LRS \
  --cache-max-gb 64 \
  --release scarthgap \
  --init both
```

On NixOS, the tooling can be provided in one command:

```sh
nix shell nixpkgs#azure-cli nixpkgs#openssh nixpkgs#rsync nixpkgs#python3 \
  -c python3 yocto/azure/test.py \
    --subscription YOUR_SUBSCRIPTION_ID \
    --group spotflowd-yocto-test \
    --location westeurope \
    --size Standard_E4as_v5 \
    --threads 4 \
    --disk-size-gb 256 \
    --disk-sku StandardSSD_LRS \
    --cache-max-gb 64 \
    --release scarthgap \
    --init both
```

The runner automatically:

1. Creates or reuses the dedicated `spotflowd-yocto-test` resource group in West
   Europe, an Ubuntu 22.04 `Standard_E4as_v5` VM (4 vCPUs, 32 GiB RAM), a **256 GiB Standard SSD managed
   data disk**, and a **32 GiB Standard SSD OS disk**.
   The VM uses Trusted Launch with Secure Boot and vTPM; the Generation 2 Ubuntu
   image supports it without the opt-in `UseStandardSecurityType` feature.
2. Installs host dependencies and mounts the data disk at `/srv/spotflowd`.
3. Uploads the current source and Yocto files, including uncommitted changes.
4. Builds and boots Scarthgap ARM64 images for **systemd and SysVinit**.
5. Streams build output and downloads logs/results to `yocto/results/<run-id>/`.
6. Deallocates the VM after a completed test, including a failed test. The command
   returns a nonzero exit status if a test fails.

No VM creation, SSH setup, source publishing, or Spotflow credentials are needed
manually. The active Azure subscription is printed before provisioning; use
`--subscription` to select another subscription. The subscription must permit
creating the VM, networking, and managed disk.

Before creating a new VM or disk, the runner checks the selected VM SKU, its
family vCPU quota and total regional vCPU quota. SKU availability alone does not
guarantee sufficient quota. A zero family quota requires an increase or another
VM family; the runner does not silently change the selected VM or region.

To check capacity for a new VM without creating any resources:

```sh
python3 yocto/azure/test.py --subscription YOUR_SUBSCRIPTION_ID \
  --location westeurope --size Standard_E4as_v5 --check-capacity
```

If quota is insufficient, open **Azure Portal → Quotas → Microsoft.Compute**,
select the same subscription and region, and request the family limit reported
by the runner. For example, an unused `Standard_E4as_v5` needs at least **4 vCPUs**
of **Standard EASv5 Family** quota, plus four available total regional vCPUs.
Alternatively, select a quota-enabled family with `--size`; `Standard_E4ads_v5`
also has 4 vCPUs and 32 GiB RAM but uses a separate **Standard EADSv5 Family**
quota. Its price differs. Check it with `--check-capacity` before retrying.

A failed VM deployment may leave the managed cache disk in place. Retrying with
another size in the same resource group/region reuses that disk. Changing only
`--location` does not move an existing disk to another region.

## Cache and reruns

Run the same command again to restart the VM and reuse its downloads, sstate,
Poky checkouts and build configuration. Every run explicitly boot-tests the image,
even when compilation is fully cached. Both init systems/releases share the
download and sstate caches; BitBake decides which artifacts are compatible.

The runner uses Yocto's public sstate mirror and matching hash-equivalence server
to reuse prebuilt tools where available. Cache misses are built locally. Use
`--no-sstate-mirror` if the public service is inaccessible. For a direct
`ci-image.sh` run, set `YOCTO_SSTATE_MIRROR=none` to disable it or supply another
mirror base URL. For the public hash server, `ci-image.sh` creates an isolated
Python environment at `$YOCTO_CACHE/host-venv` and installs the pinned dependencies
in [`../host-requirements.txt`](../host-requirements.txt). Build hosts need
`python3-venv`; Azure setup and CI install it automatically. The environment also
uses the distro's Python modules.
Ubuntu 22.04's `python3-websockets` alone is too old for BitBake's websocket API.

To keep storage costs down:

- `rm_work` removes completed recipes' expanded source/object trees during builds.
- Successful images' `tmp/` trees are removed after their logs have been copied.
  Their cached outputs remain in sstate; future builds rehydrate them. Failed
  builds retain temporary files for diagnosis/retry. Use `--keep-work` to preserve
  successful builds' temporary files as well.
- Between builds, the oldest sstate artifacts are evicted above **64 GiB**,
  preserving recently used entries and their signatures. Downloads are kept.
  Adjust with `--cache-max-gb`; `0` disables this limit. The cap is enforced
  between builds, not while tasks are using the cache.
- Shallow Git downloads reduce source-cache size, and test images skip generated
  distribution SBOMs. Disk-space monitoring stops new tasks below 10 GiB free.

Each run includes `disk-usage.txt` with storage usage before cleanup, so disk
sizing can be based on the actual build rather than the final image size.

All build data is on the **managed disk**, not Azure's temporary disk. VM
deallocation and reboot preserve it, and the VM's data-disk deletion policy is
`Detach`, so deleting just the VM also preserves the cache. Storage continues to
be billed while the VM is deallocated. Deleting the cache disk or its resource
group removes the cache; the runner never deletes either.

The build runs under systemd independently of SSH. If monitoring is interrupted,
re-run the same command to resume and collect the result. Keep the local state
directory (default `~/.local/state/spotflowd/azure/`, or `$XDG_STATE_HOME`) for
the SSH key and pending-run ID. An interrupted controller leaves the VM allocated
until monitoring resumes and the completed result is collected.

Options:

```sh
# Both releases and both init systems, using the same persistent cache.
python3 yocto/azure/test.py --release all

# A single image, on an explicitly selected subscription/region.
python3 yocto/azure/test.py --subscription SUBSCRIPTION_ID \
  --location northeurope --init systemd

# Leave the VM running for investigation after the test.
python3 yocto/azure/test.py --keep-running

# Choose capacity/tier for a new disk and the retained sstate budget.
python3 yocto/azure/test.py --disk-size-gb 256 \
  --disk-sku StandardSSD_LRS --cache-max-gb 64
```

`--group`, `--size`, `--threads`, and `--state-dir` are also configurable. Region,
VM size and disk settings apply when resources are first created; existing
resources are reused and their cache disk's actual size/tier is printed. Azure
cannot shrink an existing disk. To use a smaller disk, create an independent
runner with a different `--group`; do not delete a disk containing wanted caches.
Existing resource groups without the runner's ownership tag are rejected. Initial builds take substantially
longer than reruns. Poky checkouts retain the revision cloned on the first run.

## Why the build disk is larger than the embedded image

An embedded root filesystem may be only tens or hundreds of megabytes. The build
also needs host tools, a cross-compiler, Rust/LLVM sources and objects, package
sysroots, downloads and reusable build artifacts. These can occupy many tens of
gigabytes even for a small final image. The 256 GiB default allows temporary build
space alongside the cache; it is not a measured minimum. Use `disk-usage.txt` and
the disk-space monitor to evaluate a smaller disk for your workload.

At West Europe retail USD rates, the default persistent resources have a base
cost of approximately **$25.25/month**: $19.20 for the 256 GiB Standard SSD, $2.40
for the 32 GiB Standard SSD OS disk, and $3.65 for the public IP. VM compute remains
approximately **$0.274/hour** while allocated. Standard SSD I/O transactions,
bandwidth, taxes and subscription-specific discounts are additional. Prices are
estimates, not a billing guarantee.
