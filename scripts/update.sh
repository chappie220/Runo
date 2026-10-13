#!/bin/sh
# Updates runo to the latest release: the same as install.sh --update
# (checksum checked, a running Gateway restarted when this account may).
#
#   curl -fsSL https://raw.githubusercontent.com/chappie220/Runo/main/scripts/update.sh | sh
#   sh update.sh --version 0.4.1      # a given release: how to go back
#
# Takes the same options as install.sh (--dir, --version, --force).
# `runo update` does the same from the binary itself; this script
# also works when the installed binary is too old to have it, or broken.

set -eu

# Run next to install.sh (a checkout), or download it.
case "$0" in
*update.sh)
	here=$(cd "$(dirname "$0")" && pwd)
	if [ -f "$here/install.sh" ]; then
		exec sh "$here/install.sh" --update "$@"
	fi
	;;
esac

repo="${RUNO_REPO:-${OPENCLAW_REPO:-chappie220/Runo}}"
url="${RUNO_INSTALL_URL:-${OPENCLAW_INSTALL_URL:-https://raw.githubusercontent.com/$repo/main/scripts/install.sh}}"
tmp=$(mktemp "${TMPDIR:-/tmp}/runo-install.XXXXXX")
trap 'rm -f "$tmp"' EXIT INT TERM
if command -v curl >/dev/null 2>&1; then
	curl -fsSL --retry 3 -o "$tmp" "$url"
elif command -v wget >/dev/null 2>&1; then
	wget -q -O "$tmp" "$url"
else
	echo "error: needs curl or wget (doas apk add curl)" >&2
	exit 1
fi
sh "$tmp" --update "$@"
