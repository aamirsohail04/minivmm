#!/usr/bin/env bash
# check-host.sh: is this Linux machine ready to build and run minivmm?
#
# Usage: ./scripts/check-host.sh
# Exit status: 0 if every required check passes, 1 otherwise.

set -u

fails=0
pass() { printf '  [ OK ]  %s\n' "$1"; }
fail() { printf '  [FAIL]  %s\n' "$1"; [ -n "${2:-}" ] && printf '          fix: %s\n' "$2"; fails=$((fails + 1)); }
note() { printf '  [NOTE]  %s\n' "$1"; }

echo "minivmm host check"
echo

echo "Virtualization"
if grep -qwE 'svm|vmx' /proc/cpuinfo; then
    flag=$(grep -owE 'svm|vmx' /proc/cpuinfo | head -n1)
    pass "CPU exposes hardware virtualization ($flag)"
else
    fail "CPU does not expose svm or vmx" \
         "enable AMD-V or VT-x in firmware; in VMware enable 'Virtualize Intel VT-x/EPT or AMD-V/RVI' for this VM"
fi

if [ -d /sys/module/kvm_amd ] || [ -d /sys/module/kvm_intel ]; then
    pass "KVM kernel module loaded ($( [ -d /sys/module/kvm_amd ] && echo kvm_amd || echo kvm_intel ))"
else
    fail "kvm_amd / kvm_intel module not loaded" "sudo modprobe kvm_amd (AMD) or sudo modprobe kvm_intel (Intel)"
fi

if [ -e /dev/kvm ]; then
    pass "/dev/kvm exists"
else
    fail "/dev/kvm is missing" "see the two checks above"
fi

echo
echo "Permissions"
in_session=no
id -nG | tr ' ' '\n' | grep -qx kvm && in_session=yes
in_group_db=no
getent group kvm | cut -d: -f4 | tr ',' '\n' | grep -qx "$(id -un)" && in_group_db=yes

if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    pass "this shell can read and write /dev/kvm"
    if [ "$in_session" = no ]; then
        note "access comes from a device ACL (desktop login), not the kvm group;"
        note "SSH sessions and services may not get it"
        if [ "$in_group_db" = yes ]; then
            note "you are in the kvm group, but this session started before that: log out and back in"
        else
            note "for access in every session: sudo usermod -aG kvm \$USER, then log out and back in"
        fi
    fi
else
    if [ "$in_group_db" = yes ] && [ "$in_session" = no ]; then
        fail "no access to /dev/kvm: you were added to the kvm group after this session started" \
             "log out and back in (or reboot), then run: id"
    else
        fail "no access to /dev/kvm" "sudo usermod -aG kvm \$USER, then log out and back in"
    fi
fi

echo
echo "Toolchain"
if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
    ver=$(rustc --version | awk '{print $2}')
    major=${ver%%.*}
    rest=${ver#*.}
    minor=${rest%%.*}
    if [ "$major" -gt 1 ] || { [ "$major" -eq 1 ] && [ "$minor" -ge 85 ]; }; then
        pass "Rust $ver (1.85 or newer needed)"
    else
        fail "Rust $ver is too old (1.85 or newer needed)" "rustup update stable"
    fi
else
    fail "Rust (cargo, rustc) not found" \
         "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh, then open a new terminal"
fi

if command -v cc >/dev/null 2>&1; then
    pass "C toolchain present (Rust uses it to link)"
else
    fail "no C compiler/linker (cc)" "sudo apt install build-essential"
fi

if command -v nasm >/dev/null 2>&1; then
    pass "nasm present (optional: assembles guest/*.asm)"
else
    note "nasm not installed (optional): sudo apt install nasm"
fi

echo
echo "Linux guest (milestone M2, optional)"
bb=$(command -v busybox || true)
if [ -n "$bb" ] && ! ldd "$bb" >/dev/null 2>&1 && command -v cpio >/dev/null 2>&1; then
    pass "static busybox and cpio present (for scripts/make-initramfs.sh)"
else
    note "to build the initramfs: sudo apt install busybox-static cpio"
fi
here=$(cd "$(dirname "$0")/.." && pwd)
if [ -r "$here/guest/bzImage" ]; then
    pass "guest kernel guest/bzImage present"
else
    note "no guest/bzImage yet: sudo cp /boot/vmlinuz-\$(uname -r) guest/bzImage && sudo chown \$USER: guest/bzImage"
fi
if [ -r "$here/guest/initramfs.cpio.gz" ]; then
    pass "guest initramfs guest/initramfs.cpio.gz present"
else
    note "no guest/initramfs.cpio.gz yet: ./scripts/make-initramfs.sh"
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "READY: run 'cargo test' and 'cargo run' in the minivmm folder."
    echo "       Boot Linux: see \"Boot Linux (M2)\" in README.md."
    exit 0
fi
echo "NOT READY: $fails check(s) failed. Fix them and run this script again."
exit 1
