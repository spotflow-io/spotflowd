#!/usr/bin/env bash
# Executed as root by Azure Run Command. The build itself runs as spotflow.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

# The Azure agent can accept Run Command before cloud-init finishes first boot.
# Wait for it to rewrite the APT sources/index names and finish package setup;
# otherwise update can use the old mirror and install cannot find its indexes.
cloud-init status --wait

if [[ ! -e /var/lib/spotflowd-yocto-host-v3 ]]; then
    apt-get update
    apt-get install -y gawk wget git diffstat unzip texinfo gcc build-essential \
        chrpath socat cpio python3 python3-pip python3-venv python3-pexpect xz-utils debianutils \
        iputils-ping iproute2 python3-git python3-jinja2 python3-subunit zstd \
        liblz4-tool file locales rsync
    locale-gen en_US.UTF-8
    touch /var/lib/spotflowd-yocto-host-v3
fi

# LUN 0 is the dedicated managed data disk, not Azure's temporary /mnt disk.
disk=/dev/disk/azure/scsi1/lun0
for _ in {1..60}; do
    [[ -b "$disk" ]] && break
    sleep 2
done
[[ -b "$disk" ]] || { echo "Managed cache disk at LUN 0 is missing" >&2; exit 1; }
filesystem="$(blkid -s TYPE -o value "$disk" || true)"
if [[ -z "$filesystem" ]]; then
    # Only initialize an empty whole disk. Never overwrite a partitioned disk.
    [[ -z "$(blkid -s PTTYPE -o value "$disk" || true)" ]]
    [[ "$(lsblk -n -o TYPE "$disk" | wc -l)" -eq 1 ]]
    mkfs.ext4 -L spotflowd-yocto "$disk"
elif [[ "$filesystem" != ext4 ]]; then
    echo "Expected ext4 on the cache disk, got $filesystem" >&2
    exit 1
fi

uuid="$(blkid -s UUID -o value "$disk")"
mkdir -p /srv/spotflowd
if ! grep -Fq "UUID=$uuid /srv/spotflowd " /etc/fstab; then
    printf 'UUID=%s /srv/spotflowd ext4 defaults,nofail 0 2\n' "$uuid" >> /etc/fstab
fi
if ! mountpoint -q /srv/spotflowd; then
    mount /srv/spotflowd
fi
[[ "$(findmnt -n -o UUID /srv/spotflowd)" == "$uuid" ]]
chown spotflow:spotflow /srv/spotflowd

# Pass the host key back through the authenticated Azure control plane.
printf '\nSPOTFLOWD_HOST_KEY '
cat /etc/ssh/ssh_host_ed25519_key.pub
