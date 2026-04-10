#!/usr/bin/env bash
set -euo pipefail

REPO="${TGREP_REPO:-microsoft/tgrep}"
VERSION="${TGREP_VERSION:-}"
INSTALL_DIR="${TGREP_INSTALL_DIR:-}"

info()  { printf '\033[1;34m%s\033[0m\n' "$*"; }
error() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Detect platform
detect_target() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)
            case "$arch" in
                x86_64)  echo "x86_64-unknown-linux-musl" ;;
                aarch64) echo "aarch64-unknown-linux-musl" ;;
                *)       error "Unsupported architecture: $arch" ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64)  echo "x86_64-apple-darwin" ;;
                arm64)   echo "aarch64-apple-darwin" ;;
                *)       error "Unsupported architecture: $arch" ;;
            esac
            ;;
        *)  error "Unsupported OS: $os" ;;
    esac
}

# Resolve latest version from GitHub
resolve_version() {
    if [ -n "$VERSION" ]; then
        echo "$VERSION"
        return
    fi
    local url="https://api.github.com/repos/${REPO}/releases/latest"
    local tag
    tag="$(curl -fsSL "$url" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//')"
    [ -n "$tag" ] || error "Failed to resolve latest version from $url"
    echo "$tag"
}

# Pick install directory
pick_install_dir() {
    if [ -n "$INSTALL_DIR" ]; then
        echo "$INSTALL_DIR"
        return
    fi
    if [ -d "/usr/local/bin" ] && [ -w "/usr/local/bin" ]; then
        echo "/usr/local/bin"
    else
        local dir="${HOME}/.local/bin"
        mkdir -p "$dir"
        echo "$dir"
    fi
}

main() {
    local target version dir
    target="$(detect_target)"
    version="$(resolve_version)"
    dir="$(pick_install_dir)"

    info "Installing tgrep ${version} for ${target}"
    info "  install dir: ${dir}"

    local tmpdir
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    local base_url="https://github.com/${REPO}/releases/download/${version}"
    local archive="tgrep-${version}-${target}.tar.gz"
    local checksums="checksums.txt"

    info "Downloading ${archive}..."
    curl -fsSL "${base_url}/${archive}" -o "${tmpdir}/${archive}"
    curl -fsSL "${base_url}/${checksums}" -o "${tmpdir}/${checksums}"

    info "Verifying checksum..."
    (cd "$tmpdir" && grep "${archive}" "${checksums}" | sha256sum -c --quiet) \
        || error "Checksum verification failed"

    info "Extracting..."
    tar xzf "${tmpdir}/${archive}" -C "${tmpdir}"

    install -m 755 "${tmpdir}/tgrep" "${dir}/tgrep"

    if command -v "${dir}/tgrep" &>/dev/null; then
        info "Installed tgrep $(${dir}/tgrep --version) to ${dir}/tgrep"
    else
        info "Installed tgrep to ${dir}/tgrep"
        info "Make sure ${dir} is in your PATH"
    fi
}

main "$@"
