#!/bin/sh
# Installs (or updates) openclaw-rs from GitHub Releases: downloads the
# binary for this machine, checks it against the release's SHA256SUMS and
# that it runs, then puts it in place. The binary it replaces is kept as
# openclaw-rs.old.
#
# What needs root (writing /usr/local/bin, the system service) goes through
# sudo or doas, which ask for your password. A first install then runs
# `openclaw-rs init` to set it up.
#
#   curl -fsSL https://raw.githubusercontent.com/chappie220/openclaw/main/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --service pi
#
# Options:
#   --dir DIR          install into DIR (default: /usr/local/bin, or
#                      ~/.local/bin without root, sudo or doas; on --update,
#                      where it is now)
#   --version X.Y.Z    this release instead of the latest
#   --service USER     also install the Gateway service running as USER
#                      (another account needs root; without root, only yourself)
#   --update           update the installed binary (what update.sh does)
#   --force            reinstall even if that version is already installed
#
# Environment: OPENCLAW_REPO (owner/name), OPENCLAW_API_URL and
# OPENCLAW_DOWNLOAD_URL (for mirrors), and LANG (zh_* for Chinese).

set -eu

REPO="${OPENCLAW_REPO:-chappie220/openclaw}"
API_URL="${OPENCLAW_API_URL:-https://api.github.com}"
DOWNLOAD_URL="${OPENCLAW_DOWNLOAD_URL:-https://github.com/$REPO/releases/download}"
NAME=openclaw-rs

DIR=""
VERSION=""
SERVICE_USER=""
MODE=install
FORCE=0

case "${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}" in
zh*) ZH=1 ;;
*) ZH=0 ;;
esac

# say "English" "中文": prints the one in the user's language.
say() {
	if [ "$ZH" = 1 ]; then printf '%s\n' "$2"; else printf '%s\n' "$1"; fi
}

die() {
	if [ "$ZH" = 1 ]; then printf '错误：%s\n' "$2" >&2; else printf 'error: %s\n' "$1" >&2; fi
	exit 1
}

usage() {
	sed -n '2,24p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' || true
}

while [ $# -gt 0 ]; do
	case "$1" in
	--dir)
		[ $# -ge 2 ] || die "--dir needs a directory" "--dir 需要一个目录"
		DIR="$2"
		shift 2
		;;
	--version)
		[ $# -ge 2 ] || die "--version needs a version" "--version 需要一个版本号"
		VERSION="${2#v}"
		shift 2
		;;
	--service)
		[ $# -ge 2 ] || die "--service needs an account name" "--service 需要一个账号名"
		SERVICE_USER="$2"
		shift 2
		;;
	--update) MODE=update; shift ;;
	--force) FORCE=1; shift ;;
	-h | --help) usage; exit 0 ;;
	*) die "unknown option $1 (see --help)" "未知选项 $1（见 --help）" ;;
	esac
done

is_root() {
	[ "$(id -u)" = 0 ]
}

# as_root CMD...: runs CMD as root, through sudo or doas (which ask for the password).
as_root() {
	if is_root; then
		"$@"
	elif command -v sudo >/dev/null 2>&1; then
		sudo "$@"
	elif command -v doas >/dev/null 2>&1; then
		doas "$@"
	else
		return 127
	fi
}

# Whether as_root can work: root already, or sudo/doas and a terminal to ask on.
can_ask() {
	is_root && return 0
	{ command -v sudo || command -v doas; } >/dev/null 2>&1 || return 1
	(: </dev/tty) 2>/dev/null
}

# The release target of this machine.
target() {
	[ "$(uname -s)" = Linux ] ||
		die "prebuilt binaries are for Linux only; build from source (see README)" \
			"预编译程序只支持 Linux；其他系统请从源码编译（见 README）"
	case "$(uname -m)" in
	aarch64 | arm64) echo aarch64-unknown-linux-musl ;;
	x86_64 | amd64) echo x86_64-unknown-linux-musl ;;
	armv6l | armv7l | armhf)
		die "no prebuilt binary for 32-bit ARM: install a 64-bit OS (Raspberry Pi 3 and newer), or build from source" \
			"没有 32 位 ARM 的预编译程序：请安装 64 位系统（树莓派 3 及以上），或从源码编译"
		;;
	*) die "no prebuilt binary for $(uname -m); build from source (see README)" \
		"没有 $(uname -m) 的预编译程序；请从源码编译（见 README）" ;;
	esac
}

# fetch URL FILE
fetch() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --retry 3 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		die "needs curl or wget (doas apk add curl)" "需要 curl 或 wget（doas apk add curl）"
	fi
}

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		die "needs sha256sum to check the download" "需要 sha256sum 来校验下载的文件"
	fi
}

latest_version() {
	fetch "$API_URL/repos/$REPO/releases/latest" "$TMP/release.json" 2>/dev/null ||
		die "cannot find a release of $REPO (none published yet, or GitHub cannot be reached)" \
			"找不到 $REPO 的发布版本（还没有发布，或者连不上 GitHub）"
	tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$TMP/release.json" | head -n 1)
	[ -n "$tag" ] || die "the latest release of $REPO has no tag" "$REPO 的最新版本没有 tag"
	echo "${tag#v}"
}

# The version an installed binary reports, or nothing.
installed_version() {
	[ -x "$1" ] && "$1" --version 2>/dev/null | sed -n 's/^openclaw-rs \([^ ]*\).*/\1/p' | head -n 1
}

# Restarts a running Gateway (system or user service, or background process).
restart_service() {
	"$BINARY" service status >/dev/null 2>&1 || return 0
	if "$BINARY" service restart >/dev/null 2>&1; then
		say "Restarted the Gateway." "已重启 Gateway。"
	else
		say "Restart the Gateway to use it: $NAME gateway restart" \
			"重启 Gateway 后生效：$NAME gateway restart"
	fi
}

# Checked before anything is downloaded.
if [ -n "$SERVICE_USER" ]; then
	if [ "$SERVICE_USER" != "$(id -un)" ] && ! can_ask; then
		die "--service for another account needs root, and cannot ask for it here (no sudo or doas, or no terminal): run it as root, or name yourself ($(id -un))" \
			"为其他账号安装 --service 需要 root，这里无法请求（没有 sudo/doas 或没有终端）：请用 root 运行，或者填你自己（$(id -un)）"
	fi
	home=$(awk -F: -v u="$SERVICE_USER" '$1 == u { print $6 }' /etc/passwd)
	[ -n "$home" ] || die "no account named $SERVICE_USER" "没有叫 $SERVICE_USER 的账号"
fi

TARGET=$(target)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/openclaw-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

if [ -z "$DIR" ]; then
	current=$(command -v $NAME 2>/dev/null || true)
	if [ "$MODE" = update ]; then
		[ -n "$current" ] ||
			die "$NAME is not installed (not on PATH); install it first, or pass --dir" \
				"没有安装 $NAME（PATH 里找不到）；请先安装，或用 --dir 指定位置"
		DIR=$(dirname "$current")
	elif is_root || [ -w /usr/local/bin ] || can_ask; then
		DIR=/usr/local/bin
	else
		DIR="$HOME/.local/bin"
	fi
fi
BINARY="$DIR/$NAME"

[ -n "$VERSION" ] || VERSION=$(latest_version)
have=$(installed_version "$BINARY" || true)
if [ "$have" = "$VERSION" ] && [ "$FORCE" = 0 ]; then
	say "$NAME $VERSION is already installed at $BINARY." "$BINARY 已经是 $NAME $VERSION。"
	[ -z "$SERVICE_USER" ] && exit 0
else
	if [ -n "$have" ]; then
		say "Updating $BINARY from $have to $VERSION ($TARGET) ..." "正在把 $BINARY 从 $have 更新到 $VERSION（$TARGET）..."
	else
		say "Installing $NAME $VERSION ($TARGET) to $BINARY ..." "正在安装 $NAME $VERSION（$TARGET）到 $BINARY ..."
	fi
	asset="$NAME-$TARGET"
	fetch "$DOWNLOAD_URL/v$VERSION/SHA256SUMS" "$TMP/SHA256SUMS" ||
		die "cannot download SHA256SUMS of release $VERSION" "无法下载版本 $VERSION 的 SHA256SUMS"
	expected=$(awk -v f="$asset" '{ n = $2; sub(/^\*/, "", n) } n == f { print $1; exit }' "$TMP/SHA256SUMS")
	[ -n "$expected" ] ||
		die "release $VERSION has no binary for $TARGET" "版本 $VERSION 没有 $TARGET 的程序"
	fetch "$DOWNLOAD_URL/v$VERSION/$asset" "$TMP/$NAME" ||
		die "cannot download $asset" "无法下载 $asset"
	actual=$(sha256 "$TMP/$NAME")
	[ "$actual" = "$expected" ] ||
		die "$asset does not match its checksum (got $actual, expected $expected)" \
			"$asset 和校验值不一致（实际 $actual，应为 $expected）"
	chmod 755 "$TMP/$NAME"
	"$TMP/$NAME" --version 2>/dev/null | grep -q "$VERSION" ||
		die "the downloaded binary does not run on this machine" "下载的程序无法在这台机器上运行"

	SUDO=""
	mkdir -p "$DIR" 2>/dev/null || true
	if [ ! -w "$DIR" ]; then
		can_ask ||
			die "cannot write $DIR, and cannot ask for root here (no sudo or doas, or no terminal): run it as root, or pass --dir" \
				"无法写入 $DIR，这里也无法请求 root（没有 sudo/doas 或没有终端）：请用 root 运行，或用 --dir 指定位置"
		say "Writing to $DIR needs root; sudo or doas may ask for your password." \
			"写入 $DIR 需要 root；sudo 或 doas 可能会要求输入你的密码。"
		SUDO=as_root
		$SUDO mkdir -p "$DIR"
	fi
	# Copied into the same directory first, so the switch is one rename.
	$SUDO cp "$TMP/$NAME" "$DIR/.$NAME.new.$$"
	$SUDO chmod 755 "$DIR/.$NAME.new.$$"
	if [ -f "$BINARY" ]; then
		$SUDO cp -p "$BINARY" "$BINARY.old"
	fi
	$SUDO mv -f "$DIR/.$NAME.new.$$" "$BINARY"
	if [ -n "$have" ]; then
		say "Updated to $VERSION; the previous binary is kept as $BINARY.old." \
			"已更新到 $VERSION；旧版本保存在 $BINARY.old。"
		restart_service
	else
		say "Installed $BINARY." "已安装 $BINARY。"
	fi
fi

if [ "$MODE" = install ] && [ -z "$have" ]; then
	case ":$PATH:" in
	*":$DIR:"*) ;;
	*) say "$DIR is not on your PATH; add it: export PATH=\"$DIR:\$PATH\"" \
		"$DIR 不在 PATH 里；请添加：export PATH=\"$DIR:\$PATH\"" ;;
	esac
fi

has_tty() {
	(: </dev/tty) 2>/dev/null
}

if [ -n "$SERVICE_USER" ]; then
	if [ "$SERVICE_USER" = "$(id -un)" ]; then
		config="${OPENCLAW_RS_HOME:-$home/.openclaw-rs}/config.toml"
	else
		config="$home/.openclaw-rs/config.toml"
	fi
	if [ -f "$config" ]; then
		# Asks for root itself when the service needs it.
		"$BINARY" gateway install --user "$SERVICE_USER"
	elif [ "$SERVICE_USER" = "$(id -un)" ] && has_tty; then
		# Setup ends by offering to start the Gateway.
		echo
		"$BINARY" init </dev/tty
	elif [ "$SERVICE_USER" = "$(id -un)" ]; then
		say "Set up your agent first, then start the Gateway:" "请先完成设置，然后启动 Gateway："
		echo "  $BINARY init"
		echo "  $BINARY gateway install"
	else
		say "Set up $SERVICE_USER's agent first, then install the service:" \
			"请先为 $SERVICE_USER 完成设置，然后安装服务："
		echo "  sudo -u $SERVICE_USER $BINARY init"
		echo "  $BINARY gateway install --user $SERVICE_USER"
	fi
	exit 0
fi

# A first install goes straight on to setup, which can also start the Gateway.
if [ "$MODE" = install ] && [ ! -f "${OPENCLAW_RS_HOME:-$HOME/.openclaw-rs}/config.toml" ]; then
	if has_tty; then
		echo
		"$BINARY" init </dev/tty
	else
		say "Next: $NAME init   (then $NAME doctor to check the setup)" \
			"下一步：$NAME init   （然后用 $NAME doctor 检查设置）"
	fi
fi
