#!/usr/bin/env bash
# Same end-to-end boot test as qemu-boot-test.sh, but with the root filesystem living on an
# LVM logical volume instead of a plain partition. This is what exercises lvm.rs's native VG
# activation (PV label/metadata parsing + device-mapper table load) against a real PV built by
# actual lvm2 tooling - the parser and device-mapper ioctl code can't be dry-run in the
# sandbox this was developed in (no losetup/root/lvm2 there), so this CI job is where that
# code path first gets proven against reality. Watch its first few runs closely.
set -euo pipefail

WORKDIR="$(mktemp -d)"
cleanup() {
    set +e
    [ -n "${LOOP_DEV:-}" ] && sudo losetup -d "$LOOP_DEV" 2>/dev/null
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
KVER="$(uname -r)"
MARKER="INITRZ_QEMU_LVM_BOOT_OK"
BOOT_LOG="$WORKDIR/boot.log"
VG_NAME="vgtest"
LV_NAME="rootlv"

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

# --- build a disk image containing a real PV/VG/LV, with an ext4 root on the LV -----------
DISK_IMG="$WORKDIR/disk.img"
truncate -s 256M "$DISK_IMG"

LOOP_DEV="$(sudo losetup -f --show "$DISK_IMG")"
echo "== backing image attached as $LOOP_DEV =="

sudo pvcreate -y "$LOOP_DEV" >/dev/null
sudo vgcreate -y "$VG_NAME" "$LOOP_DEV" >/dev/null
sudo lvcreate -y -n "$LV_NAME" -L 128M "$VG_NAME" >/dev/null
LV_DEV="/dev/$VG_NAME/$LV_NAME"

sudo mke2fs -t ext4 -F -q "$LV_DEV"
ROOT_UUID="$(sudo blkid -s UUID -o value "$LV_DEV")"
[ -n "$ROOT_UUID" ] || { echo "unable to read UUID of $LV_DEV" >&2; exit 1; }
echo "== root filesystem UUID: $ROOT_UUID =="

BUSYBOX_BIN="$(command -v busybox)"
[ -n "$BUSYBOX_BIN" ] || { echo "busybox not found on PATH" >&2; exit 1; }

STAGE="$WORKDIR/stage"
mkdir -p "$STAGE"
cat > "$STAGE/init" <<EOF
#!/sbin/busybox sh
/sbin/busybox echo "$MARKER"
/sbin/busybox poweroff -f
EOF
chmod 755 "$STAGE/init"

MOUNT_DIR="$WORKDIR/mnt"
mkdir -p "$MOUNT_DIR"
sudo mount "$LV_DEV" "$MOUNT_DIR"
sudo mkdir -p "$MOUNT_DIR/sbin" "$MOUNT_DIR/lib64" "$MOUNT_DIR/dev" "$MOUNT_DIR/proc" "$MOUNT_DIR/sys"
sudo cp "$BUSYBOX_BIN" "$MOUNT_DIR/sbin/busybox"
sudo cp "$STAGE/init" "$MOUNT_DIR/sbin/init"
if ldd "$BUSYBOX_BIN" >/dev/null 2>&1; then
    while read -r lib; do
        [ -f "$lib" ] && sudo cp -n "$lib" "$MOUNT_DIR/lib64/"
    done < <(ldd "$BUSYBOX_BIN" | grep -oE '/[^ ]+\.so[^ ]*' | sort -u)
fi
sudo umount "$MOUNT_DIR"

# Deactivate the VG and detach the loop device before booting: QEMU gets the raw backing file
# as an untouched virtio-blk drive, so initrz inside the guest has to do the PV scan, metadata
# parse, and device-mapper activation completely from scratch, same as a real boot would.
sudo vgchange -an "$VG_NAME" >/dev/null
sudo losetup -d "$LOOP_DEV"
LOOP_DEV=""

# --- boot it --------------------------------------------------------------------------------
ACCEL="tcg"
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL="kvm"
fi
echo "== booting under QEMU (accel=$ACCEL), root=UUID=$ROOT_UUID =="

set +e
timeout 90 qemu-system-x86_64 \
    -M pc -accel "$ACCEL" -cpu max -m 512 \
    -kernel "$KERNEL_IMG" \
    -initrd "$INITRAMFS_IMG" \
    -drive "file=$DISK_IMG,format=raw,if=virtio" \
    -append "root=UUID=$ROOT_UUID console=ttyS0 rw" \
    -nographic -no-reboot \
    > "$BOOT_LOG" 2>&1
QEMU_EXIT=$?
set -e

if grep -q "$MARKER" "$BOOT_LOG"; then
    echo "== LVM boot test passed: activated the VG natively, mounted the LV, and ran its /sbin/init =="
    exit 0
fi

echo "== LVM boot test FAILED: marker '$MARKER' not found in serial console output (qemu exit $QEMU_EXIT) ==" >&2
echo "---- full console log ----" >&2
cat "$BOOT_LOG" >&2
exit 1
