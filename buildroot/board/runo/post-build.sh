#!/bin/sh
#
# Buildroot's part of the Runo system: the firmware's links, the version
# and the update key. post-fakeroot.sh then puts it on Alpine. $2 is the
# board.
set -eu

TARGET=$1
BOARD=${2:-rpi5}

# brcmfmac asks for the board's own NVRAM file first; the Pi 5 has the
# same Wi-Fi module (CYW43455) as the Pi 4.
brcm=$TARGET/lib/firmware/brcm
if [ -e "$brcm/brcmfmac43455-sdio.raspberrypi,4-model-b.txt" ] &&
	[ ! -e "$brcm/brcmfmac43455-sdio.raspberrypi,5-model-b.txt" ]; then
	ln -s "brcmfmac43455-sdio.raspberrypi,4-model-b.txt" \
		"$brcm/brcmfmac43455-sdio.raspberrypi,5-model-b.txt"
fi

# The program is built for the Pi, so the version comes from its source:
# the release number, as tags and `runo update` know it.
cargo=$BR2_EXTERNAL_RUNO_PATH/../Cargo.toml
version=$(sed -n 's/^release = "\(.*\)"/\1/p' "$cargo" | head -n 1)
[ -n "$version" ] || version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$cargo" | head -n 1)
[ -n "$version" ] || version=unknown

# The release key's public half, so `runo update` takes only bundles
# signed with it. Without RUNO_UPDATE_KEY the image takes no updates.
mkdir -p "$TARGET/etc/runo"
rm -f "$TARGET/etc/runo/update-keys"
if [ -n "${RUNO_UPDATE_KEY:-}" ]; then
	printf '%s\n' "$RUNO_UPDATE_KEY" | openssl pkey -pubout -outform DER |
		tail -c 32 | od -An -tx1 | tr -d ' \n' > "$TARGET/etc/runo/update-keys"
	echo >> "$TARGET/etc/runo/update-keys"
fi

# Runo reads this to know it runs as the system (state in /data/runo).
cat > "$TARGET/etc/runo-system" <<EOT
board=$BOARD
version=$version
EOT

cat > "$TARGET/etc/os-release" <<EOT
NAME=Runo
ID=runo
PRETTY_NAME="Runo $version"
VERSION_ID=$version
HOME_URL="https://github.com/chappie220/Runo"
EOT
