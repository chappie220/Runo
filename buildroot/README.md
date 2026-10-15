# Runo system image

A Buildroot external tree (`BR2_EXTERNAL`) that builds the Runo system
for the Raspberry Pi 5 and Pi 4: boot goes straight into the Agent on
tty1, the root file system is read-only and everything that changes lives
on the `data` partition. The design is in `docs/runo-1.0.md` (§3, §4.5).

## Building

Buildroot 2025.02.x (the long-term branch; CI uses 2025.02.18) on a Linux
host with the usual Buildroot prerequisites (`build-essential`, `bc`,
`cpio`, `file`, `rsync`, `unzip`, `wget`).

1. Build the static `runo` binary from the repository root; the image
   takes it from `target/aarch64-unknown-linux-musl/release/runo`
   (`BR2_PACKAGE_RUNO_BINARY` names another one):

       cargo zigbuild --locked --release --target aarch64-unknown-linux-musl

2. Configure and build, with the output directory outside the repository:

       make -C buildroot-2025.02.18 O=$HOME/br/out-rpi5 \
           BR2_EXTERNAL=$PWD/buildroot runo_rpi5_defconfig
       make -C $HOME/br/out-rpi5

   Use `runo_rpi4_defconfig` for the Pi 4. A first build takes about an
   hour; keep `BR2_DL_DIR` between builds to skip the downloads.

3. Write `images/sdcard.img.xz` (or `sdcard.img`) to an SD card, USB
   drive or NVMe disk, e.g. with Raspberry Pi Imager or
   `xzcat sdcard.img.xz | sudo dd of=/dev/sdX bs=4M conv=fsync`.

## First boot

- The `data` partition grows to fill the disk, and the Pi reboots once.
- For Wi-Fi without a keyboard, copy `runo-wifi.txt.example` on the boot
  partition to `runo-wifi.txt` and fill it in before the first boot; it
  is turned into `/data/etc/wpa_supplicant.conf` and removed. A cable
  needs nothing.
- tty1 asks for the device password (for pairing the client app; Enter
  skips), then the Agent asks for the model connection.

## Layout

| Partition | Size | Contents |
|---|---|---|
| 1 boot | 256 MiB FAT32 | firmware, `config.txt`; slot A's kernel, device trees and `cmdline.txt` under `a/` (`os_prefix=a/`) |
| 2 rootfs-A | 768 MiB ext4 | the system, mounted read-only |
| 3 rootfs-B | 768 MiB | empty, for A/B updates |
| 4 data | rest of the disk | `/data`: Runo's state (`/data/runo`), `/data/etc`, `/data/root`, `/data/log` |

The disk signature is `52554e4f`, so `cmdline.txt` names the root by
`PARTUUID=52554e4f-02` wherever the disk is attached.

Services (`/etc/init.d`): `S00data` mounts and grows `data`,
`S40network` brings up Ethernet and Wi-Fi, `S48time` sets the time zone
and clock, `S50runod` runs the management daemon, `S60gateway` runs the
gateway once enabled (`runo service install`).
