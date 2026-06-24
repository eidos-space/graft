#!/bin/sh
set -eu

REPO="${GRAFT_REPO:-eidos-space/graft}"
VERSION_INPUT="${GRAFT_VERSION:-latest}"
INSTALL_DIR="${GRAFT_INSTALL_DIR:-}"
SKIP_VERIFY="${GRAFT_SKIP_VERIFY:-false}"

say() {
  printf '%s\n' "graft installer: $*"
}

fail() {
  printf '%s\n' "graft installer: error: $*" >&2
  exit 1
}

download() {
  url="$1"
  output="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --connect-timeout 20 -o "$output" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$output" "$url"
  else
    fail "curl or wget is required"
  fi
}

sha256_file() {
  file="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$file" | awk '{ print $NF }'
  else
    fail "sha256sum, shasum, or openssl is required to verify checksums"
  fi
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m | tr '[:upper:]' '[:lower:]')"

  case "$arch" in
    x86_64 | amd64)
      target_arch="x86_64"
      ;;
    arm64 | aarch64)
      target_arch="aarch64"
      ;;
    *)
      fail "unsupported architecture: $arch"
      ;;
  esac

  case "$os" in
    Darwin)
      target_os="apple-darwin"
      archive_ext="tar.gz"
      executable="graft"
      ;;
    Linux)
      target_os="unknown-linux-gnu"
      archive_ext="tar.gz"
      executable="graft"
      ;;
    MINGW* | MSYS* | CYGWIN*)
      target_os="pc-windows-msvc"
      archive_ext="zip"
      executable="graft.exe"
      ;;
    *)
      fail "unsupported operating system: $os"
      ;;
  esac

  TARGET="${target_arch}-${target_os}"
}

default_install_dir() {
  if [ -n "$INSTALL_DIR" ]; then
    return
  fi

  if [ "$target_os" = "pc-windows-msvc" ]; then
    INSTALL_DIR="${HOME}/bin"
  elif [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
    INSTALL_DIR="/usr/local/bin"
  else
    INSTALL_DIR="${HOME}/.local/bin"
  fi
}

make_temp_dir() {
  base="${TMPDIR:-/tmp}"
  tmp_dir="$(mktemp -d "${base%/}/graft-install.XXXXXX")"
}

resolve_version() {
  if [ "$VERSION_INPUT" = "latest" ] || [ -z "$VERSION_INPUT" ]; then
    release_json="${tmp_dir}/release.json"
    download "https://api.github.com/repos/${REPO}/releases/latest" "$release_json"
    tag="$(sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' "$release_json" | head -n 1)"
    [ -n "$tag" ] || fail "could not resolve latest release tag for ${REPO}"
    VERSION="${tag#v}"
  else
    VERSION="${VERSION_INPUT#v}"
    tag="v${VERSION}"
  fi
}

verify_archive() {
  archive_path="$1"
  checksum_path="$2"

  expected="$(
    awk -v file="$ARCHIVE" '
      {
        name = $2
        sub(/^\*/, "", name)
        if (name == file) {
          print $1
          exit
        }
      }
    ' "$checksum_path"
  )"

  [ -n "$expected" ] || fail "checksum for ${ARCHIVE} was not found in SHA256SUMS"

  actual="$(sha256_file "$archive_path")"
  [ "$actual" = "$expected" ] || fail "checksum mismatch for ${ARCHIVE}"
}

extract_archive() {
  archive_path="$1"
  extract_dir="$2"

  mkdir -p "$extract_dir"
  case "$archive_ext" in
    tar.gz)
      tar -xzf "$archive_path" -C "$extract_dir"
      ;;
    zip)
      command -v unzip >/dev/null 2>&1 || fail "unzip is required to extract Windows archives"
      unzip -q "$archive_path" -d "$extract_dir"
      ;;
    *)
      fail "unsupported archive extension: $archive_ext"
      ;;
  esac
}

install_binary() {
  src="$1"
  dest="${INSTALL_DIR%/}/${executable}"

  [ -f "$src" ] || fail "archive did not contain ${executable}"
  mkdir -p "$INSTALL_DIR" || fail "could not create install directory: ${INSTALL_DIR}"
  [ -w "$INSTALL_DIR" ] || fail "install directory is not writable: ${INSTALL_DIR}"

  cp "$src" "$dest"
  chmod 755 "$dest" 2>/dev/null || true

  installed_version="$("$dest" --version 2>/dev/null || true)"
  if [ -n "$installed_version" ]; then
    say "installed ${installed_version} to ${dest}"
  else
    say "installed ${dest}"
  fi

  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *) say "note: ${INSTALL_DIR} is not on PATH" ;;
  esac
}

make_temp_dir
trap 'rm -rf "$tmp_dir"' EXIT INT TERM

detect_target
default_install_dir
resolve_version

ARCHIVE="graft-cli-${VERSION}-${TARGET}.${archive_ext}"
BASE_URL="https://github.com/${REPO}/releases/download/${tag}"
ARCHIVE_PATH="${tmp_dir}/${ARCHIVE}"
CHECKSUM_PATH="${tmp_dir}/SHA256SUMS"
EXTRACT_DIR="${tmp_dir}/extract"

say "downloading ${ARCHIVE}"
download "${BASE_URL}/${ARCHIVE}" "$ARCHIVE_PATH"

if [ "$SKIP_VERIFY" = "true" ]; then
  say "skipping checksum verification"
else
  download "${BASE_URL}/SHA256SUMS" "$CHECKSUM_PATH"
  verify_archive "$ARCHIVE_PATH" "$CHECKSUM_PATH"
  say "checksum verified"
fi

extract_archive "$ARCHIVE_PATH" "$EXTRACT_DIR"
install_binary "${EXTRACT_DIR}/${executable}"
