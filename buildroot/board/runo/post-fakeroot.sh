#!/bin/sh
#
# Turns the root file system into Runo on Alpine Linux, under fakeroot
# while Buildroot makes the image ($1 is its copy of the target, $2 the
# board). Buildroot builds the kernel, its modules, the firmware and
# installs runo; everything else comes from Alpine: its minimal root file
# system plus alpine/packages, with apk, OpenRC and BusyBox init. Then
# Runo's own files (overlay/) and services go on top.
set -eu

ROOTFS=$1
BOARD=${2:-rpi5}
HERE=$(dirname "$(readlink -f "$0")")
# shellcheck source=alpine/alpine.conf
. "$HERE/alpine/alpine.conf"

DL=${BR2_DL_DIR:-${BUILD_DIR:-/tmp}/../dl}/alpine
mkdir -p "$DL"

# Downloads $1 from $2 into $DL once, checked against $3.
fetch() {
	file=$DL/$1
	if [ ! -f "$file" ] || ! echo "$3  $file" | sha256sum -c - >/dev/null 2>&1; then
		echo "post-fakeroot: downloading $2/$1"
		# Buildroot's fakeroot puts output/host/lib first in the library
		# path, and the OpenSSL there knows no certificates: download with
		# the host's own libraries, outside fakeroot.
		env -u LD_LIBRARY_PATH -u LD_PRELOAD \
			wget -nv --tries=3 -O "$file.part" "$2/$1" ||
			env -u LD_LIBRARY_PATH -u LD_PRELOAD \
				curl -fsSL --retry 3 -o "$file.part" "$2/$1" || {
			echo "post-fakeroot: cannot download $2/$1" >&2
			exit 1
		}
		mv "$file.part" "$file"
	fi
	echo "$3  $file" | sha256sum -c - >/dev/null || {
		echo "post-fakeroot: $1 does not match its checksum" >&2
		exit 1
	}
}

case "$(uname -m)" in
x86_64) ;;
*)
	echo "post-fakeroot: apk for the build is the x86_64 one; this machine is $(uname -m)" >&2
	exit 1
	;;
esac
fetch "$MINIROOTFS" "$ALPINE_MIRROR/$ALPINE_BRANCH/releases/aarch64" "$MINIROOTFS_SHA256"
fetch "$APK_STATIC" "$ALPINE_MIRROR/$ALPINE_BRANCH/main/x86_64" "$APK_STATIC_SHA256"
tools=$(mktemp -d)
# apk files carry their signature in extended headers tar does not know.
tar --warning=no-unknown-keyword -xzf "$DL/$APK_STATIC" -C "$tools" sbin/apk.static
echo "post-fakeroot: installing Alpine $ALPINE_BRANCH"

# The Alpine root, built next to Buildroot's.
NEW=$ROOTFS.alpine
rm -rf "$NEW"
mkdir -p "$NEW"
tar -xzpf "$DL/$MINIROOTFS" -C "$NEW"
printf '%s/%s/main\n%s/%s/community\n' "$ALPINE_MIRROR" "$ALPINE_BRANCH" \
	"$ALPINE_MIRROR" "$ALPINE_BRANCH" > "$NEW/etc/apk/repositories"
# Package scripts would run Alpine's programs, which this machine
# cannot; what they do that matters is done below.
# shellcheck disable=SC2046 # one package per word
"$tools/sbin/apk.static" --root "$NEW" --arch aarch64 --no-scripts --no-cache \
	--keys-dir "$NEW/etc/apk/keys" add $(sed 's/#.*//' "$HERE/alpine/packages")
rm -rf "$tools"
# apk.static is a static program, so fakeroot did not see what it wrote.
chown -h -R 0:0 "$NEW"
chown root:42 "$NEW/etc/shadow"
chmod 640 "$NEW/etc/shadow"
grep -qx /bin/bash "$NEW/etc/shells" || echo /bin/bash >> "$NEW/etc/shells"

# What Buildroot built: the kernel's modules, the firmware, runo and the
# files post-build.sh wrote.
for path in lib/modules lib/firmware usr/bin/runo etc/runo etc/runo-system etc/os-release; do
	[ -e "$ROOTFS/$path" ] || continue
	rm -rf "${NEW:?}/$path"
	mkdir -p "$(dirname "$NEW/$path")"
	cp -a "$ROOTFS/$path" "$NEW/$path"
done
echo "ID_LIKE=alpine" >> "$NEW/etc/os-release"

# Runo's own files, over Alpine's; what they replace goes first, as some
# of Alpine's are links (/sbin/init is one to BusyBox) cp would follow.
(cd "$HERE/overlay" && find . ! -type d) | while read -r file; do
	rm -f "$NEW/$file"
done
cp -a "$HERE/overlay/." "$NEW/"
chmod 0755 "$NEW/sbin/init" "$NEW"/etc/init.d/runo* "$NEW/etc/init.d/runod" \
	"$NEW/usr/libexec/runo-console"

# Where /sbin/init mounts things: the read-only system once the writable
# layer is on top, /data and /boot.
mkdir -p "$NEW/rom" "$NEW/data" "$NEW/boot"

# What only the device's own data says, on /data.
link() {
	rm -rf "${NEW:?}/$1"
	ln -s "$2" "$NEW/$1"
}
link etc/hostname /data/etc/hostname
link etc/timezone /data/etc/timezone
link etc/localtime /data/etc/localtime
link etc/wpa_supplicant.conf /data/etc/wpa_supplicant.conf
link var/log /data/log
# root's home is on /data; bash is the shell people get, /bin/sh stays
# BusyBox ash for scripts.
sed -i 's|^root:\([^:]*\):\([^:]*\):\([^:]*\):\([^:]*\):[^:]*:[^:]*$|root:\1:\2:\3:\4:/data/root:/bin/bash|' \
	"$NEW/etc/passwd"
# No password, no login: runod and the Agent's window are the ways in.
sed -i 's|^root:[^:]*:|root:*:|' "$NEW/etc/shadow"

# OpenRC runlevels: Alpine's usual ones, then Runo's services.
add() {
	mkdir -p "$NEW/etc/runlevels/$2"
	ln -sf "/etc/init.d/$1" "$NEW/etc/runlevels/$2/$1"
}
for svc in devfs dmesg mdev hwdrivers; do add $svc sysinit; done
# osclock stands for the clock, which runo-time sets once it can.
for svc in runo-data osclock localmount modules sysctl hostname bootmisc syslog seedrng runo-zram runo-firewall; do
	add $svc boot
done
# runo-gateway joins while `runo gateway install` has it on (runo-data).
for svc in runo-network runo-time runod local; do add $svc default; done
for svc in killprocs mount-ro; do add $svc shutdown; done
for svc in "$NEW"/etc/runlevels/*/*; do
	[ -e "$NEW/etc/init.d/${svc##*/}" ] || {
		echo "post-fakeroot: no service ${svc##*/}" >&2
		exit 1
	}
done

# The Alpine root becomes the image.
find "$ROOTFS" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
cp -a "$NEW/." "$ROOTFS/"
rm -rf "$NEW"
echo "post-fakeroot: Alpine $(cat "$ROOTFS/etc/alpine-release") for $BOARD"
