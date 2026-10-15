#!/bin/sh
#
# Lays out the boot partition and writes sdcard.img:
#
#   1 boot     FAT32  firmware, config.txt; the kernel, device trees and
#                     cmdline.txt of slot A under a/ (os_prefix)
#   2 rootfs-A ext4   read-only system
#   3 rootfs-B        empty; for updates
#   4 data     ext4   grows to the end of the disk on the first boot
#
# The disk signature is fixed, so cmdline.txt names the root partition
# by PARTUUID and the same image boots from SD, USB or NVMe.
set -eu

BOARD=${2:-rpi5}
EXTERNAL=$BR2_EXTERNAL_RUNO_PATH/board/runo
BOOT=$BINARIES_DIR/bootfs

rm -rf "$BOOT"
mkdir -p "$BOOT/a"
for f in "$BINARIES_DIR"/rpi-firmware/*; do
	case "${f##*/}" in
	config.txt | cmdline.txt) ;;
	overlays) cp -r "$f" "$BOOT/a/" ;;
	*) cp -r "$f" "$BOOT/" ;;
	esac
done
cp "$EXTERNAL/$BOARD/config.txt" "$BOOT/config.txt"
cp "$BINARIES_DIR/Image" "$BOOT/a/Image"
cp "$BINARIES_DIR"/*.dtb "$BOOT/a/"
cp "$EXTERNAL/$BOARD/cmdline.txt" "$BOOT/a/cmdline.txt"
cp "$EXTERNAL/runo-wifi.txt.example" "$BOOT/"

# Data starts small and empty; S00data grows it.
rm -f "$BINARIES_DIR/data.ext4"
"$HOST_DIR/sbin/mkfs.ext4" -q -L data -E root_owner=0:0 "$BINARIES_DIR/data.ext4" 64M

files=""
for f in "$BOOT"/*; do
	files="$files\"bootfs/${f##*/}\", "
done
sed "s|#BOOT_FILES#|$files|" "$EXTERNAL/genimage.cfg.in" > "$BINARIES_DIR/genimage.cfg"

ROOTPATH_TMP=$(mktemp -d)
trap 'rm -rf "$ROOTPATH_TMP"' EXIT
rm -rf "$BUILD_DIR/genimage.tmp"
genimage \
	--rootpath "$ROOTPATH_TMP" \
	--tmppath "$BUILD_DIR/genimage.tmp" \
	--inputpath "$BINARIES_DIR" \
	--outputpath "$BINARIES_DIR" \
	--config "$BINARIES_DIR/genimage.cfg"

rm -f "$BINARIES_DIR/sdcard.img.xz"
"$HOST_DIR/bin/xz" -T0 -k -9 "$BINARIES_DIR/sdcard.img"

# The system update bundle (4.5): slot A's boot files and the root file
# system, under a manifest signed with the release key. `runo update`
# writes it to the other slot and tries it once. Without
# RUNO_UPDATE_KEY (an Ed25519 private key in PEM) no bundle is made.
UPDATE=$BINARIES_DIR/update
rm -rf "$UPDATE" "$BINARIES_DIR/runo-$BOARD.update"
if [ -n "${RUNO_UPDATE_KEY:-}" ]; then
	mkdir -p "$UPDATE"
	(cd "$BOOT/a" && tar --owner=0 --group=0 --numeric-owner --exclude=cmdline.txt -cf - -- *) |
		gzip -n -9 > "$UPDATE/boot.tar.gz"
	gzip -n -c "$BINARIES_DIR/rootfs.ext4" > "$UPDATE/rootfs.ext4.gz"
	sum() {
		printf '{"size": %s, "sha256": "%s"}' "$(stat -c %s "$1")" "$(sha256sum "$1" | cut -d' ' -f1)"
	}
	version=$(sed -n 's/^version=//p' "$TARGET_DIR/etc/runo-system")
	cat > "$UPDATE/manifest.json" <<EOT
{
  "board": "$BOARD",
  "version": "$version",
  "files": {
    "boot.tar.gz": $(sum "$UPDATE/boot.tar.gz"),
    "rootfs.ext4.gz": $(sum "$UPDATE/rootfs.ext4.gz")
  },
  "rootfs": $(sum "$BINARIES_DIR/rootfs.ext4")
}
EOT
	key=$(mktemp)
	trap 'rm -rf "$ROOTPATH_TMP" "$key"' EXIT
	printf '%s\n' "$RUNO_UPDATE_KEY" > "$key"
	openssl pkeyutl -sign -inkey "$key" -rawin -in "$UPDATE/manifest.json" -out "$UPDATE/manifest.sig"
	tar -C "$UPDATE" --owner=0 --group=0 --numeric-owner -cf "$BINARIES_DIR/runo-$BOARD.update" \
		manifest.json manifest.sig boot.tar.gz rootfs.ext4.gz
fi
