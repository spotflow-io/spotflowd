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

The Azure runner and its VM setup/build helpers live in [`azure/`](azure/).
See the [Azure test instructions](azure/README.md) for provisioning, running the
test matrix, and managing the persistent build cache.
