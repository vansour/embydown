#!/usr/bin/env bash
# embydown 一键安装脚本
#
#   curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- --version v0.0.1
#
# 参数：
#   --version, -v <tag>   安装指定版本（如 v0.0.1 或 0.0.1），默认 latest
#   --prefix, -p <dir>    安装到 <dir>/bin，默认 root 用 /usr/local，普通用户用 ~/.local
#   --uninstall           卸载当前安装的二进制
#   -h, --help            显示帮助
set -euo pipefail

REPO="vansour/embydown"
TARGET="x86_64-unknown-linux-gnu"
BIN="embydown"
VERSION="latest"
PREFIX=""
UNINSTALL=0

usage() {
    cat <<'HELP'
用法：
  install.sh [--version <tag>] [--prefix <dir>] [--uninstall]

  --version, -v <tag>   安装指定版本（如 v0.0.1 或 0.0.1），默认 latest
  --prefix, -p <dir>    安装到 <dir>/bin，默认 root 用 /usr/local，普通用户用 ~/.local
  --uninstall           卸载当前安装的二进制
  -h, --help            显示帮助
HELP
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version | -v)
            VERSION="${2:?--version 需要一个值}"
            shift 2
            ;;
        --version=*)
            VERSION="${1#*=}"
            shift
            ;;
        --prefix | -p)
            PREFIX="${2:?--prefix 需要一个值}"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#*=}"
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "未知参数：$1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

if [ "$(uname -s)" != "Linux" ]; then
    echo "错误：embydown 仅支持 Linux（当前系统：$(uname -s)）" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64 | amd64) ;;
    *)
        echo "错误：暂未提供 $(uname -m) 的预编译包（目前只有 x86_64）。" >&2
        echo "可以从源码构建：git clone https://github.com/$REPO && cd embydown && cargo build --release" >&2
        exit 1
        ;;
esac

# musl 系统（Alpine 等）无法运行 glibc 预编译包
if ldd --version 2>&1 | head -n1 | grep -qi musl; then
    echo "错误：检测到 musl libc（Alpine 等），预编译包需要 glibc。" >&2
    echo "请从源码构建：cargo build --release" >&2
    exit 1
fi

if [ -z "$PREFIX" ]; then
    if [ "$(id -u)" -eq 0 ]; then
        PREFIX="/usr/local"
    else
        PREFIX="$HOME/.local"
    fi
fi
BINDIR="$PREFIX/bin"
DEST="$BINDIR/$BIN"

if [ "$UNINSTALL" -eq 1 ]; then
    if [ -e "$DEST" ]; then
        rm -f "$DEST"
        echo "已卸载 $DEST"
    else
        echo "没有找到 $DEST"
    fi
    exit 0
fi

# 版本号归一化：允许 0.0.1 / v0.0.1 / latest
case "$VERSION" in
    "" | latest | LATEST) VERSION="latest" ;;
    v*) ;;
    *) VERSION="v$VERSION" ;;
esac

if [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
    ASSET="embydown-$TARGET.tar.gz"
else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
    ASSET="embydown-$VERSION-$TARGET.tar.gz"
fi
SUMFILE="$ASSET.sha256"

fetch() {
    local url="$1" out="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 --connect-timeout 15 -o "$out" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$out" "$url"
    else
        echo "错误：需要 curl 或 wget" >&2
        exit 1
    fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "↓ 下载 $ASSET"
fetch "$BASE/$ASSET" "$tmp/$ASSET"
fetch "$BASE/$SUMFILE" "$tmp/$SUMFILE"

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && sha256sum -c "$SUMFILE")
elif command -v shasum >/dev/null 2>&1; then
    (cd "$tmp" && shasum -a 256 -c "$SUMFILE")
else
    echo "⚠️  未找到 sha256sum/shasum，跳过校验"
fi

tar -xzf "$tmp/$ASSET" -C "$tmp"
src="$(find "$tmp" -type f -name "$BIN" -print -quit)"
if [ -z "$src" ]; then
    echo "错误：安装包里没有找到 $BIN" >&2
    exit 1
fi

mkdir -p "$BINDIR"
cp "$src" "$DEST"
chmod 0755 "$DEST"

echo "✅ 已安装到 $DEST"
"$DEST" --version 2>/dev/null || true

case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *)
        echo
        echo "提示：$BINDIR 不在 PATH 中，请把它加入 shell 配置，例如："
        echo "  echo 'export PATH=\"$BINDIR:\$PATH\"' >> ~/.bashrc"
        ;;
esac
