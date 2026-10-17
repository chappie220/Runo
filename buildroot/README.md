# Runo system image

A Buildroot external tree (`BR2_EXTERNAL`) that builds the Runo system
for the Raspberry Pi 5 and Pi 4: boot goes straight into the Agent on
tty1. Buildroot builds the kernel and firmware; the rest is Alpine Linux
(BusyBox init, OpenRC, apk), with the system read-only under a writable
layer kept on the `data` partition. The design is in `docs/runo-1.0.md` (§3, §4.5).

## Building

Buildroot 2025.02.x (the long-term branch; CI uses 2025.02.18) on a Linux
x86_64 host with the usual Buildroot prerequisites (`build-essential`,
`bc`, `cpio`, `file`, `rsync`, `unzip`, `wget`). The build downloads
Alpine's minimal root file system and packages (`board/runo/alpine/`).

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
| 2 rootfs-A | 768 MiB ext4 | the system, mounted read-only (at `/rom`) |
| 3 rootfs-B | 768 MiB | empty, for A/B updates |
| 4 data | rest of the disk | `/data`: Runo's state (`/data/runo`), the writable layer (`/data/system`), `/data/etc`, `/data/root`, `/data/log` |

The disk signature is `52554e4f`, so `cmdline.txt` names the root by
`PARTUUID=52554e4f-02` wherever the disk is attached.

### Alpine, apk and OpenRC

`board/runo/post-fakeroot.sh` builds the root file system from Alpine's
minimal one (`alpine/alpine.conf` pins the release and checksums) plus
the packages in `alpine/packages`, installed with Alpine's static apk,
then puts on the kernel's modules, the firmware, `runo` and
`overlay/`. `/etc/apk/repositories` points at the Alpine repositories, so
on the device:

```sh
apk add tmux                      # a package from the repositories
rc-update add <service> default   # a service started at boot
rc-service <service> start
```

### The writable layer

`/sbin/init` (`overlay/sbin/init`) runs first, as process 1: it checks
and mounts `data` (growing it to the end of the disk on the first boot),
then lays an overlay over the read-only system with its upper half in
`/data/system`, and hands over to BusyBox init and OpenRC in it. The
read-only system stays visible at `/rom`. So everything can be changed as
on any Alpine system, packages installed with apk and services added
under `/etc/init.d` included, and it all outlives updates, which only
replace the system underneath. Programs that are not packages can still
go on `/data`. Without a usable `data` partition the layer is in memory
until the next boot.

`runo-reset.txt` on the boot partition with the word `system` drops the
layer at the next boot: what was installed and changed is gone,
`/data/runo` and the rest of `data` stay.

Services (`/etc/init.d`, OpenRC): `runo-data` mounts `/boot` and the
parts of `data` the system links to, `runo-zram` sets up compressed swap
in RAM (as big as the RAM below 8 GB, half of it from 8 GB up, or the MiB
in `/data/etc/zram`, `0` for none), `runo-firewall` loads the firewall,
`runo-network` brings up Ethernet and Wi-Fi, `runo-time` sets the time
zone and clock and runs ntpd, `runod` runs the management daemon,
`runo-gateway` runs the gateway once enabled (`runo service install`).
