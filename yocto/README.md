# Yocto integration

`meta-spotflow` supports Poky/OpenEmbedded **Scarthgap (5.0)** and **Styhead
(5.1)**, with systemd or SysVinit. It depends only on OE-Core.

The layer builds the source checkout it belongs to. Keep the full `spotflowd`
repository together; copying just `meta-spotflow` is not supported. Pin the
repository revision in your image's source manifest for reproducible builds.
BitBake stages `Cargo.toml`, `Cargo.lock`, `LICENSE.MD`, and `src/`, tracks changes
to these files, and builds outside the source tree. It never copies `.git` or
`target`. No Git fetch or source compatibility patch is involved.

Cargo dependencies are versioned and checksummed in `spotflowd-crates.inc`.
BitBake downloads them in `do_fetch`; compilation uses the distribution's Rust
toolchain with `--frozen` and no network access. Scarthgap requires Rust/Cargo
1.75-compatible dependencies and lockfile format 3.

The recipe lets Yocto strip/package the binary and follows its
`RUST_PANIC_STRATEGY` (normally `unwind`) for the Cargo release profile. This
avoids Scarthgap's `panic_abort` runtime incompatibility with the checkout's
`panic = "abort"` setting. Other builds continue to use the checkout's profile.

## Add to an image

In an initialized Yocto build environment:

```sh
bitbake-layers add-layer /path/to/spotflowd/yocto/meta-spotflow
```

Add to `local.conf` or your image recipe:

```bitbake
IMAGE_INSTALL:append = " spotflowd"
```

Build your image, then provision the existing `/etc/spotflow/spotflowd.toml`
on the device:

```toml
[device]
id = "your-device-id"
ingest_key = "your-ingest-key"
```

Keep the installed log/storage settings. The config is root-owned and readable
by the service account (0640), and package upgrades preserve it. Credentials are
empty by default. Systemd skips startup until provisioning succeeds; SysVinit
reports a configuration-check failure.

```sh
# systemd
systemctl restart spotflowd

# SysVinit
/etc/init.d/spotflowd restart

# Either init system; works offline
spotflowd status
```

## Log sources and accounts

| Image / option | Log source | Account |
|---|---|---|
| systemd default | journald | `spotflow`, with journal access |
| SysVinit default | `/var/log/messages` | root |
| `crashdump` enabled | selected log source + pstore | root |

Compilation and runtime configuration follow the same `PACKAGECONFIG`.
Journald and syslog are not enabled together by default. Syslog uses the image's
selected logger (`VIRTUAL-RUNTIME_base-utils-syslog`, normally BusyBox syslog).
The logger must write the configured file; BusyBox's IPC-only `-C` mode does not.
The layer does not change logger permissions or assume a Debian-specific group.

Optional settings in `local.conf` or a `spotflowd_%.bbappend`:

```bitbake
# Use syslog instead of journald on a systemd image.
PACKAGECONFIG:pn-spotflowd = ""
SPOTFLOWD_SYSLOG_PATH:pn-spotflowd = "/var/log/messages"

# If your image permits unprivileged access to that file:
SPOTFLOWD_USER:pn-spotflowd = "spotflow"
SPOTFLOWD_GROUP:pn-spotflowd = "spotflow"
```

The layer creates the `spotflow` account. Custom accounts must be supplied by
your image. SysVinit account settings live in `/etc/default/spotflowd`; both
preflight and execution use the selected account.

## Kernel crash dumps

```bitbake
PACKAGECONFIG:append:pn-spotflowd = " crashdump"
```

This selects root, enables collection from `/sys/fs/pstore` and the
systemd-pstore archive, and grants the systemd service write access to those
directories. Missing pstore directories do not prevent startup. Explicitly
selecting an unprivileged account with `crashdump` is rejected.

Your BSP must enable pstore and its backend, usually `CONFIG_PSTORE` and
`CONFIG_PSTORE_RAM`, with a board-specific ramoops region. The layer does not
configure the kernel or reserve RAM.

## Image configuration and storage

To install your own config from an image layer:

```bitbake
FILESEXTRAPATHS:prepend := "${THISDIR}/files:"
SRC_URI:append = " file://device-spotflowd.toml"
SPOTFLOWD_CONFIG = "${SPOTFLOWD_UNPACKDIR}/device-spotflowd.toml"
```

The file can be ordinary TOML or use the default template's placeholders:
`@JOURNALD@`, `@SYSLOG@`, `@SYSLOG_PATH@`, `@CRASHDUMP@`, and `@LOCALSTATEDIR@`.
Provision device-specific credentials rather than baking a shared key into an
image.

`[storage].state_dir` defaults to `/var/lib/spotflow`. The daemon creates `spool/`
for log chunks, `metrics/` for metric sequence numbers, and `crashdump/` for
crash-dump deduplication state beneath it. Log spool limits apply only to log
chunks, not the other state. Runtime sockets are created under `/run` when the
service starts. Collection does not wait for network connectivity.
Read-only images need a writable mount/overlay for the state root. If you choose
another `storage.state_dir`, also allow it in the systemd service's
`ReadWritePaths` and provision its ownership for the service account. Volatile
storage is supported but loses its contents on reboot.

## Maintenance and checks

After changing `Cargo.lock`, regenerate and check the crate list:

```sh
python3 yocto/update-cargo.py
python3 yocto/update-cargo.py --check
```

When bumping the application version, rename `spotflowd_<version>.bb` to match
`Cargo.toml`. The generator checks the version and lockfile format.

Run lightweight checks with Python 3.11+:

```sh
python3 -m unittest discover -s yocto/tests -v
```

Exercise real BitBake source staging, metadata, installation and systemd unit
validation:

```sh
POKY_DIR=/path/to/poky python3 -m unittest discover -s yocto/tests -v
```

Run SysV lifecycle checks with a native binary in temporary directories:

```sh
cargo build --locked --no-default-features
sudo env PATH="$PATH" SPOTFLOWD_BINARY="$PWD/target/debug/spotflowd" \
  "$(command -v python3)" -m unittest discover -s yocto/tests -v
```

The Yocto workflow checks Rust 1.75, both releases and init systems, and builds
and boot-tests QEMU images. To run an image test on a supported Yocto build host:

```sh
YOCTO_RELEASE=scarthgap YOCTO_INIT=systemd bash yocto/ci-image.sh
```

`POKY_DIR`, `YOCTO_BUILD_ROOT`, `YOCTO_CACHE`, and `YOCTO_THREADS` can be set to
reuse a local Poky checkout, build directory and caches.

## Automated Azure test

With Python 3.11+, Azure CLI, OpenSSH and rsync installed, and Azure CLI signed in
(`az login` once), run from the repository root. Replace `YOUR_SUBSCRIPTION_ID`
with your Azure subscription ID; these are the recommended starting values:

```sh
python3 yocto/azure-test.py \
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
  -c python3 yocto/azure-test.py \
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
python3 yocto/azure-test.py --subscription YOUR_SUBSCRIPTION_ID \
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

### Cache and reruns

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
in `host-requirements.txt`. Build hosts need `python3-venv`; Azure setup and CI
install it automatically. The environment also uses the distro's Python modules.
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
python3 yocto/azure-test.py --release all

# A single image, on an explicitly selected subscription/region.
python3 yocto/azure-test.py --subscription SUBSCRIPTION_ID \
  --location northeurope --init systemd

# Leave the VM running for investigation after the test.
python3 yocto/azure-test.py --keep-running

# Choose capacity/tier for a new disk and the retained sstate budget.
python3 yocto/azure-test.py --disk-size-gb 256 \
  --disk-sku StandardSSD_LRS --cache-max-gb 64
```

`--group`, `--size`, `--threads`, and `--state-dir` are also configurable. Region,
VM size and disk settings apply when resources are first created; existing
resources are reused and their cache disk's actual size/tier is printed. Azure
cannot shrink an existing disk. To use a smaller disk, create an independent
runner with a different `--group`; do not delete a disk containing wanted caches.
Existing resource groups without the runner's ownership tag are rejected. Initial builds take substantially
longer than reruns. Poky checkouts retain the revision cloned on the first run.

### Why the build disk is larger than the embedded image

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
