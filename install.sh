#!/usr/bin/env bash
#
# spotflowd installer — download and install a pre-built binary.
#
# Usage:
#   curl -sSfL https://github.com/spotflow-io/spotflowd/releases/latest/download/install.sh | sudo bash
#   curl -sSfL .../install.sh | sudo bash -s -- --version 0.1.0
#   curl -sSfL .../install.sh | sudo bash -s -- --syslog-only
#
set -euo pipefail

REPO="spotflow-io/spotflowd"
INSTALL_DIR="/usr/sbin"
CONFIG_DIR="/etc/spotflow"
STATE_DIR="/var/lib/spotflow"
SERVICE_USER="spotflow"

# ---------- defaults ----------
VERSION=""
SYSLOG_ONLY=false

# ---------- parse args ----------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="$2"
      shift 2
      ;;
    --syslog-only)
      SYSLOG_ONLY=true
      shift
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

# ---------- require root ----------
if [[ "$(id -u)" -ne 0 ]]; then
  echo "Error: this installer must be run as root (use sudo)." >&2
  exit 1
fi

# ---------- detect architecture ----------
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
  armv7l)  TARGET="armv7-unknown-linux-gnueabihf" ;;
  *)
    echo "Error: unsupported architecture: $ARCH" >&2
    echo "Supported: x86_64, aarch64, armv7l" >&2
    exit 1
    ;;
esac

SUFFIX=""
if $SYSLOG_ONLY; then
  SUFFIX="-syslog-only"
fi

# ---------- resolve version ----------
if [[ -z "$VERSION" ]]; then
  echo "Fetching latest release..."
  VERSION="$(curl -sSf "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 | sed 's/.*"v\(.*\)".*/\1/')"
  if [[ -z "$VERSION" ]]; then
    echo "Error: could not determine latest version." >&2
    exit 1
  fi
fi

ARCHIVE="spotflowd-${VERSION}-${TARGET}${SUFFIX}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"

echo "Installing spotflowd v${VERSION} (${TARGET}${SUFFIX})..."

# ---------- download and verify ----------
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading ${ARCHIVE}..."
curl -sSfL "${BASE_URL}/${ARCHIVE}" -o "${TMPDIR}/${ARCHIVE}"
curl -sSfL "${BASE_URL}/checksums-sha256.txt" -o "${TMPDIR}/checksums-sha256.txt"

echo "Verifying checksum..."
cd "$TMPDIR"
grep "${ARCHIVE}" checksums-sha256.txt | sha256sum -c - >/dev/null 2>&1
echo "Checksum OK."

# ---------- extract ----------
tar xzf "${ARCHIVE}"
EXTRACTED_DIR="spotflowd-${VERSION}-${TARGET}${SUFFIX}"

# ---------- install binary ----------
install -m 0755 "${EXTRACTED_DIR}/spotflowd" "${INSTALL_DIR}/spotflowd"
echo "Installed binary to ${INSTALL_DIR}/spotflowd"

# ---------- create system user ----------
if ! id "$SERVICE_USER" &>/dev/null; then
  useradd -r -s /usr/sbin/nologin "$SERVICE_USER"
  echo "Created system user: ${SERVICE_USER}"
fi

# ---------- install config (never overwrite) ----------
mkdir -p "$CONFIG_DIR"
if [[ ! -f "${CONFIG_DIR}/spotflowd.toml" ]]; then
  install -m 0600 -o "$SERVICE_USER" -g "$SERVICE_USER" \
    "${EXTRACTED_DIR}/spotflowd.toml.example" "${CONFIG_DIR}/spotflowd.toml"
  echo "Installed config to ${CONFIG_DIR}/spotflowd.toml"
else
  echo "Config already exists at ${CONFIG_DIR}/spotflowd.toml — skipping (not overwritten)."
fi

# ---------- create persistent state directory ----------
mkdir -p "$STATE_DIR"
chown -R "${SERVICE_USER}:${SERVICE_USER}" "$STATE_DIR"
echo "Created state directory: ${STATE_DIR}"

# ---------- systemd service ----------
if command -v systemctl &>/dev/null; then
  install -m 0644 "${EXTRACTED_DIR}/spotflowd.service" /etc/systemd/system/spotflowd.service
  systemctl daemon-reload
  systemctl enable spotflowd
  echo "Installed and enabled systemd service."
else
  echo "systemd not detected — skipping service installation."
  echo "You can start spotflowd manually: sudo ${INSTALL_DIR}/spotflowd run --config ${CONFIG_DIR}/spotflowd.toml"
fi

# ---------- done ----------
echo ""
echo "============================================"
echo "  spotflowd v${VERSION} installed!"
echo "============================================"
echo ""
echo "Next steps:"
echo "  1. Edit ${CONFIG_DIR}/spotflowd.toml"
echo "     Set device.id and device.ingest_key from your Spotflow dashboard."
echo ""
if command -v systemctl &>/dev/null; then
  echo "  2. Start the service:"
  echo "     sudo systemctl start spotflowd"
  echo ""
  echo "  3. Check status:"
  echo "     sudo spotflowd status"
  echo "     sudo systemctl status spotflowd"
  echo "     sudo journalctl -u spotflowd -f"
fi
