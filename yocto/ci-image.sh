#!/usr/bin/env bash
# Run a real cross-build, rootfs packaging and offline QEMU smoke test.
set -euo pipefail

ROOT="$(realpath "$(dirname "${BASH_SOURCE[0]}")/..")"
RELEASE="${YOCTO_RELEASE:-scarthgap}"
INIT="${YOCTO_INIT:-systemd}"
BUILD_ROOT="${YOCTO_BUILD_ROOT:-${RUNNER_TEMP:-/tmp}/yocto-build-${RELEASE}-${INIT}}"
CACHE="${YOCTO_CACHE:-${HOME}/yocto-cache}"
POKY_DIR="${POKY_DIR:-${RUNNER_TEMP:-/tmp}/poky-${RELEASE}}"
THREADS="${YOCTO_THREADS:-4}"
SSTATE_MIRROR="${YOCTO_SSTATE_MIRROR:-https://sstate.yoctoproject.org/all}"

case "$RELEASE" in scarthgap|styhead) ;; *) exit 2 ;; esac
case "$INIT" in systemd|sysvinit) ;; *) exit 2 ;; esac

if [[ "$SSTATE_MIRROR" != none ]]; then
    # Jammy's python3-websockets is too old for BitBake's public hash server.
    # Use one isolated, retained host environment for both init systems/releases.
    # shellcheck source=/dev/null
    source "$ROOT/yocto/host-python.sh" "$CACHE/host-venv"
fi

if [[ ! -d "$POKY_DIR/.git" ]]; then
    git clone --depth 1 --branch "$RELEASE" https://github.com/yoctoproject/poky.git "$POKY_DIR"
fi
# The upstream environment scripts may reference unset optional variables.
set +u
# shellcheck source=/dev/null
source "$POKY_DIR/oe-init-build-env" "$BUILD_ROOT"
set -u
# Leave no idle BitBake server holding state when the Azure runner prunes caches
# or removes a completed build's temporary directory.
trap 'bitbake -m >/dev/null 2>&1 || true' EXIT
cat > conf/spotflowd-test.conf <<EOF
MACHINE = "qemuarm64"
INIT_MANAGER = "$INIT"
IMAGE_INSTALL:append = " spotflowd"
# Native journal injection for the systemd runtime test, not a daemon dependency.
IMAGE_INSTALL:append = " \${@bb.utils.contains('DISTRO_FEATURES', 'systemd', 'systemd-extra-utils', '', d)}"
EXTRA_IMAGE_FEATURES:append = " debug-tweaks ssh-server-dropbear"
DL_DIR = "$CACHE/downloads"
SSTATE_DIR = "$CACHE/sstate"
BB_NUMBER_THREADS = "$THREADS"
PARALLEL_MAKE = "-j $THREADS"
INHERIT += "rm_work"
# Skip distribution SBOM generation for this disposable test image.
INHERIT:remove = "create-spdx"
BB_GIT_SHALLOW = "1"
BB_GENERATE_SHALLOW_TARBALLS = "1"
BB_DISKMON_DIRS = "STOPTASKS,\${TMPDIR},10G,100K HALT,\${TMPDIR},2G,1K STOPTASKS,/tmp,100M,100K HALT,/tmp,10M,1K"
TESTIMAGE_AUTO = "0"
IMAGE_CLASSES += "testimage"
TEST_SUITES = "ping ssh spotflowd"
# Keep testimage's default WORKDIR/testimage log path: QemuRunner uses its
# parent to locate the recipe's native QMP Python module.
TEST_RUNQEMUPARAMS = "slirp nographic"
QEMU_USE_KVM = ""
PACKAGECONFIG:remove:pn-qemu-system-native = "sdl"
EOF
if [[ "$SSTATE_MIRROR" != none ]]; then
    cat >> conf/spotflowd-test.conf <<EOF
# Reuse official prebuilt tools where available; cache misses build locally.
BB_HASHSERVE = "auto"
BB_HASHSERVE_UPSTREAM = "wss://hashserv.yoctoproject.org/ws"
SSTATE_MIRRORS:append = " file://.* $SSTATE_MIRROR/PATH;downloadfilename=PATH"
EOF
fi
if ! grep -Fxq 'require conf/spotflowd-test.conf' conf/local.conf; then
    printf '\nrequire conf/spotflowd-test.conf\n' >> conf/local.conf
fi
# Machine and init selection must be read from local.conf before BitBake loads
# the machine/distro includes. A post-read (-R) config is too late for these.
# Do not rewrite bblayers.conf on every run of a persistent build directory.
if ! bitbake-layers show-layers | grep -Fq "$ROOT/yocto/meta-spotflow"; then
    bitbake-layers add-layer "$ROOT/yocto/meta-spotflow"
fi
bitbake core-image-minimal
# Always boot-test, even when the image itself is already in the build cache.
bitbake -c testimage core-image-minimal
