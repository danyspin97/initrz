# initrz

initrz is a Linux initramfs: a minimal PID-1 environment that runs after the kernel boots
and before the real root filesystem takes over. Its job is to find the root device,
unlock/assemble/mount it, and hand off to `/sbin/init` on it — the same role `dracut` or
`mkinitcpio`+busybox fill, but implemented as a single Rust binary rather than a pile of
shell scripts calling out to `blkid`, `cryptsetup`, `lvm`, `mount`, etc.

The workspace has two crates:

- **`initrz`** — the init binary that actually runs as PID 1 inside the initramfs image.
- **`mkinitrz`** — the host-side tool that builds the initramfs cpio image: it resolves ELF
  shared-library dependencies, picks which kernel modules to bundle, and packs everything
  into a `newc`-format cpio archive (optionally zstd-compressed).

## Philosophy

**Do the work natively; don't shell out to userspace tools inside the initramfs.** This is
an explicit, repeatedly-reaffirmed design constraint (see `feedback_jj_commits`/session
notes) — `initrz` talks to the kernel directly (netlink, ioctls, `/sys`, `/proc`) instead of
forking `blkid`, `cryptsetup`, `mount`, `vgchange`, `mount.cifs`, etc. Concretely:

- Device metadata comes from `libblkid-rs` and a persistent `/blkid.cache`, not the `blkid`
  binary.
- LUKS unlock goes through `libcryptsetup-rs` directly.
- LVM activation (`lvm.rs`) hand-parses the LVM2 on-disk binary label + text metadata format
  and drives `/dev/mapper/control` with raw `DM_DEV_CREATE`/`DM_TABLE_LOAD`/`DM_DEV_SUSPEND`
  ioctls (`device_mapper.rs`) — the from-scratch equivalent of `vgchange -ay` / `dmsetup
  create`. No `vgchange` process is ever spawned.
- Mounting goes through the modern `fsopen`/`fsconfig`/`fsmount`/`move_mount` syscall API
  (via the `mount-api` crate), not `mount(8)` or even `libc::mount()`.
- Networking (`ip=` static config) is done with raw `NETLINK_ROUTE` messages, not `ip(8)`.
- `/dev/mapper/<name>` and btrfs multi-device registration are handled by reacting to
  netlink uevents and issuing `mknod`/ioctls directly — there's no udev running to do it for
  you.

This constraint isn't absolute: `busybox` is still bundled as a rescue-shell fallback (see
below), and building an ELF binary's shared-library closure (`mkinitrz/src/depend.rs`)
obviously reads real ELF files rather than reimplementing `ld.so`. But the steady-state boot
path — the thing that has to work on every boot — does not fork any external process.

This constraint has been deliberately *not* extended to two dracut-parity features that were
evaluated and explicitly deferred rather than half-implemented: native mdadm RAID assembly
(reimplementing MD superblock parsing + ioctl orchestration is data-loss-risk scope, and
untestable without real degraded arrays) and fsck repair (fundamentally needs fs-specific
external tools; a detect-only "refuse to mount if dirty" policy was rejected because the
kernel already auto-replays journals on unclean shutdown for ext4/btrfs/xfs/f2fs, so it would
cause more boot failures than it prevents). See `lvm.rs`'s `is_supported_segment_type` doc
comment and the project's session notes for the reasoning; these are correctness-sensitive
enough that they're left undone rather than shipped unverified.

**A failed boot should drop to an interactive shell, never panic the kernel.** `initrz` runs
as PID 1, so an uncaught error or panic is not "the process crashed" — the kernel treats
"init died" as a fatal panic. `main()` (`initrz/src/main.rs`) wraps the whole boot sequence in
`catch_unwind` and, on any `Err` or panic, falls back to an interactive `brush` (a Rust `bash`
implementation) rescue shell instead of exiting. This makes the rescue shell a load-bearing
safety net rather than a nicety: it's what turns "root device not found" from an unrecoverable
boot failure into a debuggable prompt.

**Best-effort over all-or-nothing.** A single failed kernel module load, a wrong LUKS
passphrase on the first try, or one unsupported LVM segment type doesn't abort the whole
boot — it's logged and the rest of the boot proceeds (passphrases get `MAX_PASSPHRASE_ATTEMPTS
= 3` tries; module loads for individual `/sys` modalias files are independent; unsupported LVM
LVs are skipped, not fatal to the whole VG). Only the things that are genuinely required to
reach a working root (the root device itself, its encryption, its LVM/DM layer) are fatal.

**dracut/kernel cmdline compatibility where practical.** Rather than invent its own cmdline
vocabulary, `initrz` recognizes the same parameters dracut and the mainline kernel already
document: `root=`, `ro`/`rw`, `rootflags=`, `rootwait=`, `resume=`/`resume_offset=`/
`noresume`, `rd.luks.uuid=`/`rd.luks.name=`, `rd.live.overlay[=dir]`, `ip=`, `nameserver=`,
`quiet`/`loglevel=N`/`rd.debug`, `nfsroot=`. This means an existing dracut-based cmdline
mostly works unchanged.

## How initrz boots (`initrz/src/main.rs`)

Sequentially, `initrz()`:

1. **Mounts `/dev`, `/sys`, `/proc`** (`devtmpfs`/`sysfs`/`proc`) via the `fsopen` mount API —
   nothing else works without these, including reading the cmdline.
2. **Parses `/proc/cmdline`** and initializes the logger at a level derived from it
   (`quiet`/`loglevel=`/`rd.debug`; defaults to `Trace`). Anything logged before this point is
   silently lost — there's no buffering, by design, since the mounts themselves can't fail
   without a working shell to see the error anyway.
3. **Configures networking** from `ip=`/`nameserver=` if present (static IPv4 only — see
   Non-goals).
4. **Builds the core objects**: `ModuleLoader` (parses `modules.dep`/`modules.alias` for the
   running kernel version), `DeviceHandler` (parses `/etc/crypttab.initramfs` + `rd.luks.*` +
   `resume=` + `root=` from cmdline), `UeventListener` (binds a `NETLINK_KOBJECT_UEVENT`
   socket).
5. **Probes already-present devices** via blkid (`probe_all`/`probe_all_removable`), so
   devices that existed before `initrz` started (as opposed to hot-plugged later) are known
   immediately.
6. **Unlocks any already-present LUKS devices**, looks for the hibernation-resume device, and
   checks whether the root device is already available.
7. **Spawns the uevent-listener thread**, which streams `add`/`change` block-device events
   from the kernel for the rest of boot — this is what catches devices that appear *after*
   this point (the common case: block/USB/virtio drivers finishing probing).
8. **Walks `/sys/**/modalias`** in parallel (`rayon`) to load modules for every device already
   present at boot, matching against `modules.alias`.
9. **Blocks on the uevent channel** (`DeviceHandler::listen`, default 30s timeout,
   `rootwait=N` overridable) until the root device, all crypttab/`rd.luks.*` devices, and the
   resume device (if any) have all shown up. Each uevent is handled by `DeviceHandler::handle`,
   which recognizes: a LUKS device to unlock, the resume device, the root device itself, or an
   `LVM2_member` device to trigger VG activation on.
10. **Mounts root** (`Mounts::mount_root`): resolves the filesystem type (auto-detected via
    blkid, or forced via `root.type=`), loop-attaches it first if it's a plain image file
    (e.g. a baked-in squashfs), and either mounts it directly or — if `rd.live.overlay` was
    given — mounts it read-only as an overlayfs lowerdir with a writable upperdir (tmpfs by
    default, or a persistent directory), the same mechanism dracut's `dmsquash-live` uses for
    writable live-boot media.
11. **`move_mount`s `/dev`, `/sys`, `/proc` and the new root into place**, `chroot`s, and
    `exec`s `/sbin/init` on the new root. If `exec` itself fails (it only returns on failure),
    that's now surfaced as a real error instead of silently looking like success.

If any of steps 1–11 return `Err`, or if anything anywhere panics, `main()` catches it and
drops into the `brush` rescue shell instead of letting PID 1 die.

## Feature surface

- **Root discovery**: by UUID (`root=UUID=...`) or path (`root=/dev/...`), block device or a
  plain file (loop-attached automatically), NFSv4 (`root=nfs4:server:path`,
  `root=nfs:server:path`, or legacy `root=/dev/nfs nfsroot=server:path[,opts]` — NFSv3 is
  intentionally unsupported, see Non-goals), or CIFS (`root=cifs://server/share`, with
  `rootflags=credentials=<file>` to avoid a plaintext password on `/proc/cmdline`).
- **Filesystems**: ext4, btrfs (including multi-device — member devices are registered with
  `BTRFS_IOC_SCAN_DEV` before mount), xfs, f2fs, vfat, exfat, nfs4, cifs, or `auto`
  (blkid-detected).
- **LUKS**: via `/etc/crypttab.initramfs` (standard crypttab syntax: `name UUID=.../path
  luks keyfile-or-none`) or `rd.luks.uuid=`/`rd.luks.name=` on the cmdline. Passphrase prompt
  retries up to 3 times; keyfiles are read once (a wrong keyfile isn't retried, since
  re-reading the same file changes nothing).
- **LVM**: physical volume label + VG text metadata are parsed directly off disk
  (`lvm.rs`) and every logical volume is activated as a real device-mapper device
  (`device_mapper.rs`). Only linear segments and default-stripe-size striped segments are
  supported today — mirror/raid\*/thin/cache/snapshot segment types are logged and skipped
  per-LV (not fatal to the rest of the VG).
- **Hibernation resume**: `resume=`/`resume_offset=`/`noresume`, writing directly to
  `/sys/power/resume{,_offset}`.
- **Live/writable-overlay boot**: `rd.live.overlay[=dir]` mounts the resolved root read-only
  as an overlayfs lowerdir with a tmpfs (or persistent-directory) upperdir — dracut
  `dmsquash-live` parity.
- **Networking**: static `ip=` configuration (address/gateway/device) and `nameserver=` →
  `/etc/resolv.conf`. DHCP/autoconf requests are recognized and logged as unsupported rather
  than silently ignored.
- **Module loading**: dependency-resolving, idempotent, concurrency-safe (`RwLock`-guarded
  claim-before-load with rollback on failure) loading of xz/zstd-compressed `.ko` modules,
  matched either by exact name or by `/sys/**/modalias` pattern against `modules.alias`.
- **Logging/verbosity**: `quiet`, `loglevel=N` (syslog-style 0–7), `rd.debug` — last one on
  the cmdline wins.

## Non-goals (explicitly out of scope, not just "not done yet")

- **DHCP/network autoconf** — only static `ip=` is supported; `ip=dhcp` etc. is detected and
  skipped with a warning rather than attempted.
- **NFSv3 root** — would need a separate rpcbind/MOUNT-protocol RPC client in userspace; NFSv4
  talks directly to the server and the kernel client handles it once `mount(2)` has the right
  options, so only NFSv4 is supported.
- **mdadm RAID assembly** and **fsck repair** — deliberately deferred; see Philosophy above.
- **LVM mirror/raid\*/thin/cache/snapshot** segment types, and multi-stripe LVs with a
  non-default stripe size — rejected with a clear per-LV error/skip rather than silently
  misactivated.

## Testing

Three layers, each catching a different class of bug (see the CI-related session notes —
several real, boot-breaking bugs were only ever caught by the layer below the one that "should
have" caught them):

1. **Unit tests** (`cargo test --workspace`, 82 tests as of this writing: 74 in `initrz`, 8 in
   `mkinitrz`). Mostly pure-function tests: cmdline parsing, crypttab parsing, uevent
   parsing, LVM metadata-text parsing (including a byte-for-byte synthetic PV image
   constructed field-by-field per the on-disk spec — this is what actually caught a label
   field-offset bug that pure text-parser unit tests couldn't), device-mapper table
   encoding (decoded back out and checked against the kernel's own `dm_target_spec` layout),
   `ld.so.conf` include-resolution, etc. Fast, no root/devices/kernel required.
2. **`cargo clippy --workspace --all-targets` + `cargo fmt --all -- --check`**, gating CI.
3. **QEMU boot test** (`.github/scripts/qemu-boot-test.sh`, run as a second CI job after unit
   tests pass): builds a real initramfs with the just-built `initrz`/`mkinitrz` binaries,
   packs a minimal ext4 root filesystem with busybox as `/sbin/init` (built with `debugfs`,
   no root/mount privileges needed), and boots both under QEMU (KVM-accelerated when
   available) against the CI runner's own kernel. Success means a marker string written by
   the new root's `/sbin/init` shows up on the serial console — i.e. `initrz` actually
   reached `switch_root` and handed off control. This is the *only* test that exercises the
   real PID-1 path: netlink uevent reception, blkid probing, `mount_root`, `chroot`+`exec`.
   Nothing else in the test suite ever creates a block device, receives a real uevent, or
   calls `mount(2)`.

**Why layer 3 matters, concretely**: this project's history includes at least two bugs where
a full green `cargo test --workspace` (and even a successful `cargo build -p initrz`) said
nothing was wrong, but a real boot always failed:

- `uevent_listener::listen()`'s netlink `recv()` call passed a full-capacity `Vec` (`len ==
  capacity`), but `Socket::recv` writes into a buffer's *spare capacity past its length*, not
  from index 0. Every real uevent silently failed to parse — device detection could never work
  from a live uevent, only from the boot-time `/sys` scan — but the pure-parsing unit tests
  for `parse_uevent()` never exercised the actual `recv()` call at all, so they stayed green.
  Fixed by `buf.clear()` before each `recv()` instead of resizing.
- The `/sys/**/modalias` scan in `main.rs` passed each modalias *file's path* to
  `load_modalias()` instead of the file's *contents* — no `/sys` path ever matches a
  `modules.alias` pattern, so this scan silently loaded zero modules for any device already
  present at boot. Found only by reading the code carefully during a review pass, not by any
  test failure.

The lesson embedded in the repo (and worth re-stating for anyone extending it): a green
`cargo test` here proves the parsing/encoding logic is correct in isolation, not that the boot
path that calls it actually works. When changing anything in `uevent_listener.rs`,
`device_handler.rs`, `mounts.rs`, or `main.rs`'s boot sequence, run the QEMU boot test (or
`.github/scripts/qemu-boot-test.sh` locally, if you have `qemu-system-x86_64` + KVM + a kernel
with modules available) before trusting it.

**Known coverage gap**: the QEMU layer currently only boots a plain-partition root
(`qemu-boot-test.sh`) and an LVM root (`qemu-boot-test-lvm.sh`). `unlock_luks_device` in
`device_handler.rs` (real `libcryptsetup-rs` `CryptInit`/`activate_by_passphrase` calls) and the
CIFS/NFS4/overlayfs/loop-device mount paths in `mounts.rs`/`loop_device.rs` have no unit test
*or* QEMU test — the exact "parsing is tested, the adjacent real ioctl/syscall path is exercised
by nothing" gap that let the netlink `recv()` bug above ship silently for multiple sessions. A
LUKS QEMU test needs a real `cryptsetup luksFormat` on a loop device plus either a bundled
keyfile (deterministic, CI-friendly) or piping a passphrase into QEMU's stdin over the serial
console (`rd.luks.uuid=`/`rd.luks.name=` path); CIFS/NFS need a server the runner can talk to.
Not added yet because it can't be built and verified against a real boot in this project's usual
development sandbox (no `sudo`, no `cryptsetup` CLI, no loop-device access there) the way the
LVM test was before it was trusted in CI — same reasoning as the RAID/fsck deferral above.
Worth doing in a session with root + `cryptsetup` available; don't add it blind.

### Running the tests locally

```sh
# Requires libcryptsetup-dev/libcryptsetup-devel and libblkid-dev/libblkid-devel headers —
# without them, `cargo build -p initrz` fails outright (mkinitrz builds fine regardless).
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check

# Full PID-1 boot test (needs qemu-system-x86_64, busybox, e2fsprogs, debugfs; KVM used if
# /dev/kvm is accessible, falls back to TCG otherwise):
cargo build --release -p initrz -p mkinitrz
./.github/scripts/qemu-boot-test.sh
```

## Building an initramfs (`mkinitrz`)

```sh
INITRZ=target/release/initrz target/release/mkinitrz -k "$(uname -r)" -o initramfs.img
```

`mkinitrz` always bundles: the `initrz` binary itself (as `/init`), `busybox` (rescue shell
fallback — `brush` is the primary shell but busybox provides coreutils/a fallback `sh`),
`/etc/ld.so.conf`, and `modules.dep`/`modules.alias` for the target kernel. Every ELF binary
it adds gets its shared-library closure resolved recursively (`depend.rs`, parsing `DT_NEEDED`
directly from the ELF dynamic section) and library search paths come from `/etc/ld.so.conf`
(+ `include`d files) first, then a hardcoded fallback list (`/usr/lib64`, `/usr/lib`,
`/lib64`, `/lib`, `/usr/local/lib`) for systems without a usable one.

Two module-selection modes:
- **General** (default): includes any module matching a heuristic allowlist
  (`initramfs_modules.rs::is_module_needed`) — filesystems, crypto, device-mapper, storage
  controllers/block drivers, HID/keyboard input — regardless of what's loaded on the build
  host. This is what you'd ship in a distro package.
- **`--host-only`**: intersects that same heuristic with whatever's *actually* in
  `/proc/modules` right now — a much smaller, machine-specific image.

`/etc/initrz/mkinitrz.conf` (YAML; path overridable with `--config`) can list extra
`modules`, `binaries` (ELF, dependency-resolved same as the built-ins), and `files` (copied
verbatim) to include.

## Repository layout

```
initrz/
  src/
    main.rs              boot sequence, cmdline log-level, panic → rescue-shell fallback
    device_handler.rs     root/LUKS/resume device tracking, crypttab + rd.luks.* parsing
    uevent_listener.rs    netlink uevent socket, DEVPATH → /dev or /dev/mapper resolution
    module_loader.rs      modules.dep/modules.alias parsing, dependency-resolving loader
    root_device.rs        root=/rootflags=/nfsroot=/rd.live.overlay cmdline parsing
    mounts.rs              fsopen/fsconfig/fsmount mounting, overlay setup, switch_root
    lvm.rs                 LVM2 PV label + VG text-metadata parser, VG activation
    device_mapper.rs       raw DM_DEV_CREATE/DM_TABLE_LOAD/DM_DEV_SUSPEND ioctls
    identifier.rs          UUID-vs-path device identifier, blkid resolution
    filesystem.rs           filesystem type enum + blkid auto-detection
    encrypted_device.rs, encryption_type.rs, unlock_type.rs   crypttab line model
    resume.rs               hibernation resume via /sys/power/resume{,_offset}
    network.rs               ip=/nameserver= via raw NETLINK_ROUTE
    cifs_credentials.rs      credentials=<file> → username=/password=/domain=
    btrfs.rs                 BTRFS_IOC_SCAN_DEV for multi-device btrfs
    loop_device.rs           losetup-equivalent for file-backed roots
    utils.rs                 shared blkid cache handle
  test/                      fixtures for the tests above (crypttab, modules.alias, ...)

mkinitrz/
  src/
    main.rs                 CLI, top-level image assembly
    initramfs.rs             archive contents: base dirs/symlinks, ELF+lib closure, configs
    initramfs_modules.rs     general vs. host-only module selection heuristic
    depend.rs                 ELF DT_NEEDED parsing (shared-library resolution)
    ldconf.rs                 /etc/ld.so.conf (+ include) parsing
    newc.rs                    cpio "newc" archive format writer
    config.rs                  mkinitrz.conf (extra modules/binaries/files) schema

.github/
  workflows/ci.yml            fmt + clippy + unit tests, then the QEMU boot job
  scripts/qemu-boot-test.sh   builds + boots a real initramfs under QEMU
```

## Repo conventions

- This repo is colocated git+jj; history work uses jujutsu (`jj new`, `jj commit -m`, `jj
  log`), one commit per fix/feature/cleanup rather than large combined commits.
- Licensed under GPLv3 (`LICENSE.md`).
