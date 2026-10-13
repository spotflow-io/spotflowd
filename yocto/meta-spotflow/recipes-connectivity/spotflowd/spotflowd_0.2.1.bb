SUMMARY = "Spotflow observability daemon"
DESCRIPTION = "Collects logs (syslog, journald) and OS metrics from embedded \
Linux devices and streams them to the Spotflow platform over MQTT/TLS."
HOMEPAGE = "https://spotflow.io"
LICENSE = "BUSL-1.1"
LIC_FILES_CHKSUM = "file://LICENSE.MD;md5=60a3a1d0dbcd92a0a15d68aadc344e38"

# Stage only build inputs, not the checkout's .git or target directories.
# BitBake tracks local file contents, including the entire src directory.
FILESEXTRAPATHS:prepend := "${SPOTFLOWD_SOURCE_DIR}:"
SRC_URI = " \
    file://Cargo.toml;subdir=spotflowd \
    file://Cargo.lock;subdir=spotflowd \
    file://LICENSE.MD;subdir=spotflowd \
    file://src;subdir=spotflowd \
    file://spotflowd.toml \
    file://spotflowd.init \
    file://spotflowd.default \
    file://spotflowd.service \
"
# Scarthgap uses WORKDIR; Styhead separates unpacked inputs into UNPACKDIR.
SPOTFLOWD_UNPACKDIR = "${@d.getVar('UNPACKDIR') or d.getVar('WORKDIR')}"
S = "${SPOTFLOWD_UNPACKDIR}/spotflowd"

inherit cargo pkgconfig useradd update-rc.d systemd
require spotflowd-crates.inc

PACKAGECONFIG ??= "${@bb.utils.contains('DISTRO_FEATURES', 'systemd', 'journald', '', d)}"
PACKAGECONFIG[journald] = ",,systemd"
PACKAGECONFIG[crashdump] = ",,,"

# Explicit Cargo features, without configure-style PACKAGECONFIG arguments.
CARGO_BUILD_FLAGS += "--no-default-features ${@bb.utils.contains('PACKAGECONFIG', 'journald', '--features journald', '', d)}"
# Let do_package split debug symbols and strip the target binary. Cargo's
# desktop release profile otherwise triggers the already-stripped QA check.
export CARGO_PROFILE_RELEASE_STRIP = "false"
# Scarthgap's cargo-built libstd includes a panic_abort runtime with unwind
# metadata. The checkout's panic=abort profile fails when rustc selects it.
# Follow the distro's target strategy rather than rebuilding its Rust sysroot.
export CARGO_PROFILE_RELEASE_PANIC = "${RUST_PANIC_STRATEGY}"

# Syslog/pstore access is normally root-only. Journald-only builds use the
# service account; do not change a distribution's logger permissions or groups.
SPOTFLOWD_USER ?= "${@'spotflow' if 'journald' in d.getVar('PACKAGECONFIG').split() and 'crashdump' not in d.getVar('PACKAGECONFIG').split() else 'root'}"
SPOTFLOWD_GROUP ?= "${@'spotflow' if d.getVar('SPOTFLOWD_USER') == 'spotflow' else 'root'}"
SPOTFLOWD_SYSLOG_PATH ?= "/var/log/messages"
SPOTFLOWD_CONFIG ?= "${SPOTFLOWD_UNPACKDIR}/spotflowd.toml"
SPOTFLOWD_JOURNALD = "${@bb.utils.contains('PACKAGECONFIG', 'journald', 'true', 'false', d)}"
SPOTFLOWD_SYSLOG = "${@bb.utils.contains('PACKAGECONFIG', 'journald', 'false', 'true', d)}"
SPOTFLOWD_CRASHDUMP = "${@bb.utils.contains('PACKAGECONFIG', 'crashdump', 'true', 'false', d)}"
SPOTFLOWD_JOURNAL_GROUP = "${@bb.utils.contains('PACKAGECONFIG', 'journald', 'SupplementaryGroups=systemd-journal', '', d)}"
SPOTFLOWD_PSTORE_WRITE = "${@bb.utils.contains('PACKAGECONFIG', 'crashdump', 'ReadWritePaths=-/sys/fs/pstore -/var/lib/systemd/pstore', '', d)}"

USERADD_PACKAGES = "${PN}"
GROUPADD_PARAM:${PN} = "--system spotflow"
USERADD_PARAM:${PN} = "--system --gid spotflow --no-create-home --home-dir ${localstatedir}/lib/spotflow --shell /bin/false spotflow"

INITSCRIPT_NAME = "spotflowd"
INITSCRIPT_PARAMS = "defaults 85 15"
SYSTEMD_SERVICE:${PN} = "spotflowd.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

# Pull in the selected logger when file collection is used and an explicit
# start-stop-daemon provider on SysV images, including those without BusyBox.
RDEPENDS:${PN} += "ca-certificates \
    ${@bb.utils.contains('PACKAGECONFIG', 'journald', '', d.getVar('VIRTUAL-RUNTIME_base-utils-syslog') or 'busybox-syslog', d)} \
    ${@bb.utils.contains('DISTRO_FEATURES', 'sysvinit', 'dpkg-start-stop', '', d)} \
"

python __anonymous() {
    if 'journald' in (d.getVar('PACKAGECONFIG') or '').split() and 'systemd' not in (d.getVar('DISTRO_FEATURES') or '').split():
        bb.fatal('spotflowd: PACKAGECONFIG journald requires systemd in DISTRO_FEATURES')
    if 'crashdump' in (d.getVar('PACKAGECONFIG') or '').split() and d.getVar('SPOTFLOWD_USER') != 'root':
        bb.fatal('spotflowd: PACKAGECONFIG crashdump requires SPOTFLOWD_USER = "root"')
}

do_install() {
    install -d ${D}${sbindir} ${D}${sysconfdir}/spotflow
    install -m 0755 ${B}/target/${CARGO_TARGET_SUBDIR}/spotflowd ${D}${sbindir}/spotflowd

    # Render a dedicated template instead of editing comments in the example.
    sed -e 's|@JOURNALD@|${SPOTFLOWD_JOURNALD}|g' \
        -e 's|@SYSLOG@|${SPOTFLOWD_SYSLOG}|g' \
        -e 's|@SYSLOG_PATH@|${SPOTFLOWD_SYSLOG_PATH}|g' \
        -e 's|@CRASHDUMP@|${SPOTFLOWD_CRASHDUMP}|g' \
        -e 's|@LOCALSTATEDIR@|${localstatedir}|g' \
        ${SPOTFLOWD_CONFIG} > ${D}${sysconfdir}/spotflow/spotflowd.toml
    chmod 0640 ${D}${sysconfdir}/spotflow/spotflowd.toml
    chown root:${SPOTFLOWD_GROUP} ${D}${sysconfdir}/spotflow/spotflowd.toml

    install -d -m 0750 ${D}${localstatedir}/lib/spotflow
    chown ${SPOTFLOWD_USER}:${SPOTFLOWD_GROUP} ${D}${localstatedir}/lib/spotflow

    if ${@bb.utils.contains('DISTRO_FEATURES', 'systemd', 'true', 'false', d)}; then
        install -d ${D}${systemd_system_unitdir}
        sed -e 's|@SBINDIR@|${sbindir}|g' \
            -e 's|@SYSCONFDIR@|${sysconfdir}|g' \
            -e 's|@LOCALSTATEDIR@|${localstatedir}|g' \
            -e 's|@USER@|${SPOTFLOWD_USER}|g' \
            -e 's|@GROUP@|${SPOTFLOWD_GROUP}|g' \
            -e 's|@JOURNAL_GROUP@|${SPOTFLOWD_JOURNAL_GROUP}|g' \
            -e 's|@PSTORE_WRITE@|${SPOTFLOWD_PSTORE_WRITE}|g' \
            ${SPOTFLOWD_UNPACKDIR}/spotflowd.service > ${D}${systemd_system_unitdir}/spotflowd.service
        chmod 0644 ${D}${systemd_system_unitdir}/spotflowd.service
    fi

    if ${@bb.utils.contains('DISTRO_FEATURES', 'sysvinit', 'true', 'false', d)}; then
        install -d ${D}${sysconfdir}/init.d ${D}${sysconfdir}/default
        sed -e 's|@SBINDIR@|${sbindir}|g' \
            -e 's|@SYSCONFDIR@|${sysconfdir}|g' \
            ${SPOTFLOWD_UNPACKDIR}/spotflowd.init > ${D}${sysconfdir}/init.d/spotflowd
        chmod 0755 ${D}${sysconfdir}/init.d/spotflowd
        sed -e 's|@USER@|${SPOTFLOWD_USER}|g' \
            -e 's|@GROUP@|${SPOTFLOWD_GROUP}|g' \
            ${SPOTFLOWD_UNPACKDIR}/spotflowd.default > ${D}${sysconfdir}/default/spotflowd
        chmod 0644 ${D}${sysconfdir}/default/spotflowd
    fi
}

# Runtime directories are created at service start, never shipped under /run.
CONFFILES:${PN} = "${sysconfdir}/spotflow/spotflowd.toml ${@bb.utils.contains('DISTRO_FEATURES', 'sysvinit', '${sysconfdir}/default/spotflowd', '', d)}"
FILES:${PN} += "${sysconfdir}/spotflow ${systemd_system_unitdir}/spotflowd.service ${localstatedir}/lib/spotflow"
