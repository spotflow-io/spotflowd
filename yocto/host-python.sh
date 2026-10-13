#!/usr/bin/env bash
# Source before oe-init-build-env so BitBake and its servers use this Python.
set -euo pipefail
host_venv="${1:?host virtual environment directory required}"
if [[ ! -x "$host_venv/bin/python3" ]]; then
    # Keep distro-provided git, jinja2, pexpect and subunit available.
    python3 -m venv --system-site-packages "$host_venv"
fi
"$host_venv/bin/python3" -m pip install --disable-pip-version-check \
    --requirement "$(dirname "${BASH_SOURCE[0]}")/host-requirements.txt"
export PATH="$host_venv/bin:$PATH"
