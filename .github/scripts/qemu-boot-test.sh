#!/usr/bin/env bash
# End-to-end boot test: builds an initramfs with the just-built initrz/mkinitrz binaries,
# packs a minimal ext4 root filesystem (busybox as /sbin/init), and boots both under QEMU
# using the host's own running kernel. Verifies that initrz reaches switch_root and that
# the new root's /sbin/init actually runs, by looking for a marker string on the serial
# console. This is the only way to exercise initrz's real PID-1 path (device detection via
# uevents, blkid probing, mount_root, switch_root) - none of that runs under `cargo test`.
set -euo pipefail

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
KVER="$(uname -r)"
MARKER="INITRZ_QEMU_BOOT_OK"
BOOT_LOG="$WORKDIR/boot.log"

echo "== kernel version: $KVER =="

KERNEL_IMG=""
for candidate in "/boot/vmlinuz-$KVER" "/boot/vmlinuz"; do
    if [ -f "$candidate" ]; then
        KERNEL_IMG="$candidate"
        break
    fi
done
if [ -z "$KERNEL_IMG" ]; then
    echo "no bootable kernel image found for $KVER under /boot" >&2
    exit 1
fi
echo "== using kernel image: $KERNEL_IMG =="

if [ ! -d "/lib/modules/$KVER" ]; then
    echo "no /lib/modules/$KVER - can't build a matching initramfs" >&2
    exit 1
fi

# --- build the initramfs with the freshly built initrz binary -----------------------------
INITRZ_BIN="$REPO_ROOT/target/release/initrz"
MKINITRZ_BIN="$REPO_ROOT/target/release/mkinitrz"
[ -x "$INITRZ_BIN" ] || { echo "missing $INITRZ_BIN - build with 'cargo build --release -p initrz -p mkinitrz' first" >&2; exit 1; }
[ -x "$MKINITRZ_BIN" ] || { echo "missing $MKINITRZ_BIN" >&2; exit 1; }

INITRAMFS_IMG="$WORKDIR/initramfs.img"
INITRZ="$INITRZ_BIN" "$MKINITRZ_BIN" -k "$KVER" -o "$INITRAMFS_IMG"
echo "== initramfs built: $(du -h "$INITRAMFS_IMG" | cut -f1) =="

# --- build a minimal ext4 root filesystem: busybox as /sbin/init --------------------------
ROOTFS_IMG="$WORKDIR/rootfs.img"
truncate -s 64M "$ROOTFS_IMG"
mke2fs -t ext4 -F -q "$ROOTFS_IMG"

BUSYBOX_BIN="$(command -v busybox)"
[ -n "$BUSYBOX_BIN" ] || { echo "busybox not found on PATH" >&2; exit 1; }

STAGE="$WORKDIR/stage"
mkdir -p "$STAGE/lib"
cp "$BUSYBOX_BIN" "$STAGE/busybox"

# Resolve busybox's own shared library deps (if it isn't a static build) so they can be
# copied into the image too - busybox-static packages need none of this, but fall back to
# handling a dynamically linked busybox for portability across distros.
if ldd "$BUSYBOX_BIN" >/dev/null 2>&1; then
    while read -r lib; do
        [ -f "$lib" ] && cp -n "$lib" "$STAGE/lib/"
    done < <(ldd "$BUSYBOX_BIN" | grep -oE '/[^ ]+\.so[^ ]*' | sort -u)
fi

cat > "$STAGE/init" <<EOF
#!/sbin/busybox sh
/sbin/busybox echo "$MARKER"
/sbin/busybox poweroff -f
EOF
chmod 755 "$STAGE/busybox" "$STAGE/init"

DEBUGFS_CMDS="$WORKDIR/debugfs.cmds"
{
    echo "mkdir /sbin"
    echo "mkdir /lib64"
    echo "mkdir /dev"
    echo "mkdir /proc"
    echo "mkdir /sys"
    echo "cd /sbin"
    echo "write $STAGE/busybox busybox"
    echo "write $STAGE/init init"
    if [ -n "$(ls -A "$STAGE/lib" 2>/dev/null)" ]; then
        echo "cd /lib64"
        for f in "$STAGE"/lib/*; do
            echo "write $f $(basename "$f")"
        done
    fi
} > "$DEBUGFS_CMDS"
debugfs -w -f "$DEBUGFS_CMDS" "$ROOTFS_IMG" >/dev/null

# --- boot it --------------------------------------------------------------------------------
ACCEL="tcg"
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL="kvm"
fi
echo "== booting under QEMU (accel=$ACCEL) =="

set +e
timeout 90 qemu-system-x86_64 \
    -M pc -accel "$ACCEL" -cpu max -m 512 \
    -kernel "$KERNEL_IMG" \
    -initrd "$INITRAMFS_IMG" \
    -drive "file=$ROOTFS_IMG,format=raw,if=virtio" \
    -append "root=/dev/vda console=ttyS0 rw" \
    -nographic -no-reboot \
    > "$BOOT_LOG" 2>&1
QEMU_EXIT=$?
set -e

if grep -q "$MARKER" "$BOOT_LOG"; then
    echo "== boot test passed: reached switch_root and ran the new root's /sbin/init =="
    exit 0
fi

echo "== boot test FAILED: marker '$MARKER' not found in serial console output (qemu exit $QEMU_EXIT) ==" >&2
echo "---- full console log ----" >&2
cat "$BOOT_LOG" >&2
exit 1
