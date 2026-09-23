#!/bin/sh
# Install lazyamp from GitHub Releases.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/miltonparedes/lazyamp/main/scripts/install.sh | sh
#   curl -fsSL ... | VERSION=v0.1.1 sh
#   sh install.sh --prefix /usr/local
#
# VERSION must be passed to `sh`, not to `curl`. `VERSION=v0.1.1 curl … | sh`
# sets the variable on curl only and does not pin the install.
set -eu

REPO="miltonparedes/lazyamp"
BINARY="lazyamp"
GITHUB="https://github.com/${REPO}"

usage() {
  cat <<'EOF'
Install lazyamp from GitHub Releases.

Usage:
  install.sh [--prefix DIR]

Environment:
  VERSION   Release tag or semver (e.g. v0.1.1 or 0.1.1). Default: latest.
            Pin with: curl -fsSL …/install.sh | VERSION=v0.1.1 sh
  PREFIX    Install prefix (binary goes in PREFIX/bin). Same as --prefix.
  LAZYAMP_INSECURE_SKIP_VERIFY
            Set to 1 to skip sha256 checks (not recommended).

Linux installs the musl static binary (x86_64/aarch64-unknown-linux-musl).
macOS installs the native darwin archive.

The binary is installed to PREFIX/bin, or /usr/local/bin if writable,
or ~/.local/bin otherwise. The install is staged then renamed so a
running lazyamp is not overwritten in place (avoids Text-file-busy).
EOF
}

PREFIX="${PREFIX:-}"
VERSION="${VERSION:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --prefix)
      if [ $# -lt 2 ]; then
        echo "install.sh: --prefix requires a directory" >&2
        exit 2
      fi
      PREFIX="$2"
      shift 2
      ;;
    --prefix=*)
      PREFIX="${1#--prefix=}"
      shift
      ;;
    --version)
      if [ $# -lt 2 ]; then
        echo "install.sh: --version requires a tag" >&2
        exit 2
      fi
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#--version=}"
      shift
      ;;
    *)
      echo "install.sh: unknown argument \`$1\`" >&2
      echo "Try \`install.sh --help\`." >&2
      exit 2
      ;;
  esac
done

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "install.sh: missing required command: $1" >&2
    exit 1
  fi
}

need_cmd curl
need_cmd tar
need_cmd uname
need_cmd mktemp

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux) rust_os="unknown-linux-musl" ;;
    Darwin) rust_os="apple-darwin" ;;
    *)
      echo "install.sh: unsupported OS \`$os\` (linux and darwin only)" >&2
      exit 1
      ;;
  esac

  case "$arch" in
    x86_64 | amd64) rust_arch="x86_64" ;;
    aarch64 | arm64) rust_arch="aarch64" ;;
    *)
      echo "install.sh: unsupported architecture \`$arch\`" >&2
      exit 1
      ;;
  esac

  echo "${rust_arch}-${rust_os}"
}

resolve_tag() {
  requested="${1:-}"
  if [ -n "$requested" ]; then
    case "$requested" in
      v*) echo "$requested" ;;
      *) echo "v${requested}" ;;
    esac
    return
  fi

  # Follow the Releases "latest" redirect (no API token required).
  final="$(curl -fsSL -o /dev/null -w '%{url_effective}' "${GITHUB}/releases/latest")"
  tag="${final##*/}"
  if [ -z "$tag" ] || [ "$tag" = "latest" ]; then
    echo "install.sh: could not resolve the latest release from ${GITHUB}/releases/latest" >&2
    exit 1
  fi
  echo "$tag"
}

sha256_file() {
  file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  else
    echo ""
  fi
}

skip_verify() {
  [ "${LAZYAMP_INSECURE_SKIP_VERIFY:-}" = "1" ]
}

verify_checksum() {
  archive="$1"
  sums="$2"
  name="$(basename "$archive")"

  if skip_verify; then
    echo "install.sh: LAZYAMP_INSECURE_SKIP_VERIFY=1; skipping sha256 verification"
    return 0
  fi

  if [ ! -f "$sums" ]; then
    echo "install.sh: checksums.txt is required for this release" >&2
    echo "Set LAZYAMP_INSECURE_SKIP_VERIFY=1 to skip (not recommended)." >&2
    exit 1
  fi

  expected="$(awk -v name="$name" '$2 == name { print $1; exit }' "$sums")"
  if [ -z "$expected" ]; then
    echo "install.sh: no checksum entry for ${name} in checksums.txt" >&2
    echo "Set LAZYAMP_INSECURE_SKIP_VERIFY=1 to skip (not recommended)." >&2
    exit 1
  fi

  actual="$(sha256_file "$archive")"
  if [ -z "$actual" ]; then
    echo "install.sh: need sha256sum or shasum to verify ${name}" >&2
    echo "Set LAZYAMP_INSECURE_SKIP_VERIFY=1 to skip (not recommended)." >&2
    exit 1
  fi

  if [ "$actual" != "$expected" ]; then
    echo "install.sh: sha256 mismatch for ${name}" >&2
    echo "  expected: ${expected}" >&2
    echo "  actual:   ${actual}" >&2
    exit 1
  fi
  echo "Verified sha256 for ${name}"
}

install_dir() {
  if [ -n "$PREFIX" ]; then
    echo "${PREFIX}/bin"
    return
  fi
  if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
    echo "/usr/local/bin"
    return
  fi
  echo "${HOME}/.local/bin"
}

atomic_install() {
  src="$1"
  dest_dir="$2"
  dest="${dest_dir}/${BINARY}"
  staging="${dest_dir}/.${BINARY}.tmp.$$"
  mkdir -p "$dest_dir"
  cp "$src" "$staging"
  chmod 755 "$staging"
  # rename over the destination so a running binary is not overwritten in place
  if ! mv -f "$staging" "$dest"; then
    rm -f "$staging"
    echo "install.sh: failed to install ${dest}" >&2
    exit 1
  fi
}

target="$(detect_target)"
tag="$(resolve_tag "${VERSION}")"
version="${tag#v}"
archive_name="${BINARY}-${version}-${target}.tar.gz"
dest="$(install_dir)"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Installing ${BINARY} ${tag} (${target})"
echo "Downloading ${GITHUB}/releases/download/${tag}/${archive_name}"

if ! curl -fsSL -o "${tmp}/${archive_name}" \
  "${GITHUB}/releases/download/${tag}/${archive_name}"; then
  echo "install.sh: failed to download ${archive_name}" >&2
  echo "Check ${GITHUB}/releases for supported targets." >&2
  echo "Linux artifacts are musl static builds (*-unknown-linux-musl)." >&2
  exit 1
fi

if skip_verify; then
  echo "install.sh: LAZYAMP_INSECURE_SKIP_VERIFY=1; skipping checksums.txt"
else
  if ! curl -fsSL -o "${tmp}/checksums.txt" \
    "${GITHUB}/releases/download/${tag}/checksums.txt"; then
    echo "install.sh: failed to download checksums.txt for ${tag}" >&2
    echo "Set LAZYAMP_INSECURE_SKIP_VERIFY=1 to skip (not recommended)." >&2
    exit 1
  fi
  verify_checksum "${tmp}/${archive_name}" "${tmp}/checksums.txt"
fi

tar -xzf "${tmp}/${archive_name}" -C "$tmp"

bin=""
for candidate in \
  "${tmp}/${BINARY}-${version}-${target}/${BINARY}" \
  "${tmp}/${BINARY}"; do
  if [ -f "$candidate" ]; then
    bin="$candidate"
    break
  fi
done

if [ -z "$bin" ]; then
  bin="$(find "$tmp" -type f -name "$BINARY" | head -n 1)"
fi

if [ -z "$bin" ] || [ ! -f "$bin" ]; then
  echo "install.sh: archive did not contain ${BINARY}" >&2
  exit 1
fi

atomic_install "$bin" "$dest"

echo "Installed ${dest}/${BINARY}"

case ":${PATH}:" in
  *":${dest}:"*)
    echo "Run \`${BINARY}\` (Amp must be on PATH)."
    ;;
  *)
    echo "${dest} is not on PATH. Add it, then run \`${BINARY}\`:"
    echo "  export PATH=\"${dest}:\$PATH\""
    ;;
esac

echo "Amp must be on PATH (or set AMP_BIN). See ${GITHUB}#readme"
