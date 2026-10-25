#!/usr/bin/env bash
# make-initramfs.sh: build the small busybox initramfs for minivmm's Linux guest.
#
# Usage: ./scripts/make-initramfs.sh [OUTPUT]
#        (default OUTPUT: guest/initramfs.cpio.gz)
#
# Needs: busybox-static, cpio, gzip   (sudo apt install busybox-static cpio)
#
# The guest's /init mounts /proc, /sys and /dev, prints a banner and starts
# a shell on the serial console. If the kernel command line contains
# "minivmm.selftest", it prints MINIVMM-BOOT-OK and reboots instead, which
# is what tests/boot.rs looks for.

set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$repo/guest/initramfs.cpio.gz}

die() { printf 'make-initramfs: %s\n' "$1" >&2; exit 1; }

busybox=$(command -v busybox || true)
[ -n "$busybox" ] || die "busybox not found (sudo apt install busybox-static)"
# A dynamically linked busybox would need its libraries in the initramfs too.
if ldd "$busybox" >/dev/null 2>&1; then
    die "$busybox is dynamically linked; install the static one: sudo apt install busybox-static"
fi
command -v cpio >/dev/null || die "cpio not found (sudo apt install cpio)"
command -v gzip >/dev/null || die "gzip not found"

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

mkdir -p "$root"/{bin,sbin,usr/bin,usr/sbin,dev,proc,sys,tmp,root,etc}
cp "$busybox" "$root/bin/busybox"
ln -s busybox "$root/bin/sh"

cat > "$root/init" <<'INIT'
#!/bin/sh
# /init for the minivmm guest: PID 1, started by the kernel from the initramfs.
/bin/busybox --install -s
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
exec 0</dev/console 1>/dev/console 2>&1
hostname minivmm

echo "minivmm guest: $(uname -sr), $(grep -c ^processor /proc/cpuinfo) vCPU, $(awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo) MiB RAM"

if grep -qw minivmm.selftest /proc/cmdline; then
    echo MINIVMM-BOOT-OK
    reboot -f
fi

echo "Type 'reboot -f' to stop the VM, or press Ctrl-A x."
export PS1='minivmm:\w# '
export HOME=/root
cd /root
# cttyhack opens the real console (ttyS0) as the controlling terminal, so
# Ctrl-C and job control work in the shell.
while true; do
    setsid cttyhack sh
done
INIT
chmod 755 "$root/init"

mkdir -p "$(dirname "$out")"
(cd "$root" && find . -print0 | cpio --null --create --format=newc --owner=0:0 --quiet) \
    | gzip -9 > "$out"
printf 'make-initramfs: wrote %s (%s bytes)\n' "$out" "$(stat -c %s "$out")"
