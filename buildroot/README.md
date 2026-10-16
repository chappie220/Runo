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
   drive or NVMe disk: with `runo-flash` (below), Raspberry Pi Imager or
   `xzcat sdcard.img.xz | sudo dd of=/dev/sdX bs=4M conv=fsync`.

## Writing the card from a PC: runo-flash

`runo-flash` (source in `flash/`) writes the image from a Linux or Windows
PC, with screens in the terminal. Download `runo-flash-x86_64-linux` or
`runo-flash-x86_64-windows.exe` from a release (or the CI run of a pull
request) and run it; it asks for root or administrator rights itself.

1. Choose the board.
2. Choose the image: the latest release from GitHub (checked against its
   `SHA256SUMS-<board>`), or a file: `runo-<board>.img.xz`, the `.zip`
   GitHub Actions gives for an Image build, or an `.img`. Files in the
   current folder and in Downloads are listed.
3. Optionally fill in Wi-Fi; it goes on the boot partition as
   `runo-wifi.txt`, read on the first boot.
4. The device password, for pairing the client app. One is made up to
   start with; write it down or type your own. It goes on the boot
   partition as `runo-password.txt`, so a Pi without a screen opens
   pairing by itself on its first start. Empty leaves it to the Pi's
   screen. Images too old to read that file are refused with it set.
5. Choose the card. Only removable disks are listed, never the one the PC
   runs from.
6. Type `yes`. The image is written, the partition table last, and read
   back to check it.

`runo-flash runo-rpi4.img.xz` starts with that file.

## First boot

- The `data` partition grows to fill the disk, and the Pi reboots once.
- For Wi-Fi without a keyboard, copy `runo-wifi.txt.example` on the boot
  partition to `runo-wifi.txt` and fill it in before the first boot; it
  is turned into `/data/etc/wpa_supplicant.conf` and removed. A cable
  needs nothing.
- tty1 asks for the device password (for pairing the client app; Enter
  skips), then the Agent asks for the model connection.
- Without a screen, copy `runo-password.txt.example` on the boot partition
  to `runo-password.txt` and put the device password in it. It is read
  once when runod starts, kept only as a hash and removed; with no client
  paired yet, the pairing window then stays open for the client app.

## Layout

| Partition | Size | Contents |
|---|---|---|
| 1 boot | 256 MiB FAT32 | firmware, `config.txt`; slot A's kernel, device trees and `cmdline.txt` under `a/` (`os_prefix=a/`) |
| 2 rootfs-A | 768 MiB ext4 | the system, mounted read-only |
| 3 rootfs-B | 768 MiB | empty, for A/B updates |
| 4 data | rest of the disk | `/data`: Runo's state (`/data/runo`), `/data/etc`, `/data/root`, `/data/log` |

The disk signature is `52554e4f`, so `cmdline.txt` names the root by
`PARTUUID=52554e4f-02` wherever the disk is attached.

### Changing the system by hand

The running system can be made writable for a while, as root:

```sh
mount -o remount,rw /
# ... change files ...
sync
mount -o remount,ro /
```

It goes back to read-only on the next boot anyway. Such changes are not
kept by an update: it writes a whole new system to the other slot, so
they are gone once it is the running one. There is no package manager
either; programs are added by copying them in. What should outlive an
update goes on `/data` instead (a program in `/data/<name>`, run from
there), or into the image (`configs/*_defconfig`).

Services (`/etc/init.d`): `S00data` mounts and grows `data`,
`S40network` brings up Ethernet and Wi-Fi, `S48time` sets the time zone
and clock, `S50runod` runs the management daemon, `S60gateway` runs the
gateway once enabled (`runo service install`).
