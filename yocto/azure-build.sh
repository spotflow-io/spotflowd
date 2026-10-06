#!/usr/bin/env bash
# Runs under systemd on the Azure VM, independently of the controlling SSH client.
set -euo pipefail

release="${1:?release required}"
init="${2:?init required}"
threads="${3:?thread count required}"
run_id="${4:?run ID required}"
cache_max_gb="${5:-64}"
keep_work="${6:-0}"
use_mirror="${7:-1}"
case "$release" in scarthgap|styhead|all) ;; *) exit 2 ;; esac
case "$init" in systemd|sysvinit|both) ;; *) exit 2 ;; esac
[[ "$threads" =~ ^[1-9][0-9]*$ && "$run_id" =~ ^[a-zA-Z0-9_-]+$ ]] || exit 2
[[ "$cache_max_gb" =~ ^[0-9]+$ && "$keep_work" =~ ^[01]$ && "$use_mirror" =~ ^[01]$ ]] || exit 2

root="${SPOTFLOWD_AZURE_ROOT:-/srv/spotflowd}"
results="$root/runs/$run_id"
mkdir -p "$results"
exec >> "$results/build.log" 2>&1
trap 'code=$?; printf "%s\n" "$code" > "$results/exit-code"' EXIT
export LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8
export YOCTO_CACHE="$root/cache" YOCTO_THREADS="$threads"
[[ "$use_mirror" == 1 ]] || export YOCTO_SSTATE_MIRROR=none

# Prune only while BitBake is not running; retain downloads for future builds.
prune_cache() {
    python3 "$root/source/yocto/cache-prune.py" "$YOCTO_CACHE/sstate" --max-gb "$cache_max_gb"
}
prune_cache

releases=("$release")
inits=("$init")
[[ "$release" != all ]] || releases=(scarthgap styhead)
[[ "$init" != both ]] || inits=(systemd sysvinit)

failed=0
for version in "${releases[@]}"; do
    for manager in "${inits[@]}"; do
        name="$version-$manager"
        export YOCTO_RELEASE="$version" YOCTO_INIT="$manager"
        export POKY_DIR="$root/poky-$version"
        export YOCTO_BUILD_ROOT="$root/build-$name"
        echo "=== Building and boot-testing $name ==="
        if bash "$root/source/yocto/ci-image.sh"; then
            printf '%s PASS\n' "$name" >> "$results/summary.txt"
            passed=1
        else
            code=$?
            printf '%s FAIL (exit %s)\n' "$name" "$code" >> "$results/summary.txt"
            failed=1
            passed=0
        fi

        mkdir -p "$results/$name"
        for log in "$YOCTO_BUILD_ROOT/bitbake-cookerdaemon.log" "$YOCTO_BUILD_ROOT/tmp/log"; do
            [[ ! -e "$log" ]] || cp -a "$log" "$results/$name/"
        done
        # QemuRunner requires these logs alongside the image recipe's sysroot.
        # Collect them before successful tmp/ cleanup instead of relocating them.
        for log in "$YOCTO_BUILD_ROOT"/tmp/work/*/core-image-minimal/*/testimage; do
            [[ ! -d "$log" ]] || cp -a "$log" "$results/$name/"
        done
        # Preserve spotflowd task diagnostics even if another recipe failed.
        if [[ -d "$YOCTO_BUILD_ROOT/tmp/work" ]]; then
            while IFS= read -r -d '' log; do
                relative="${log#"$YOCTO_BUILD_ROOT"/}"
                mkdir -p "$results/$name/$(dirname "$relative")"
                cp "$log" "$results/$name/$relative"
            done < <(find "$YOCTO_BUILD_ROOT/tmp/work" \( -path '*/spotflowd/*/temp/log.*' \
                -o -path '*/core-image-minimal/*/temp/log.do_testimage*' \) -type f -print0)
        fi
        {
            echo "=== $name, before cleanup ==="
            df -h "$root"
            for path in "$YOCTO_CACHE/downloads" "$YOCTO_CACHE/sstate" "$YOCTO_BUILD_ROOT/tmp"; do
                [[ ! -d "$path" ]] || du -sh "$path"
            done
        } >> "$results/disk-usage.txt"
        if [[ "$passed" == 1 && "$keep_work" == 0 ]]; then
            # Successful outputs are in sstate. Do not retain duplicate expanded
            # sysroots/package trees for every init system and release.
            rm -rf -- "$YOCTO_BUILD_ROOT/tmp"
        fi
        prune_cache
    done
done
cat "$results/summary.txt"
cat "$results/disk-usage.txt"
exit "$failed"
