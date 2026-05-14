#!/bin/sh
# Install ptywright — drive interactive terminal applications through PTYs
# https://github.com/utensils/ptywright
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/utensils/ptywright/main/install.sh | sh
#
# Options (via environment):
#   PTYWRIGHT_INSTALL_DIR  — install directory (default: ~/.local/bin)
#   PTYWRIGHT_VERSION      — release tag (default: latest)

set -e

REPO="utensils/ptywright"
VERSION="${PTYWRIGHT_VERSION:-latest}"
INSTALL_DIR="${PTYWRIGHT_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
    Linux)
        case "${ARCH}" in
            x86_64)          ASSET="ptywright-x86_64-unknown-linux-gnu.tar.gz" ;;
            aarch64|arm64)   ASSET="ptywright-aarch64-unknown-linux-gnu.tar.gz" ;;
            *)
                echo "Error: unsupported Linux architecture: ${ARCH}" >&2
                exit 1
                ;;
        esac
        ;;
    Darwin)
        case "${ARCH}" in
            arm64|aarch64)   ASSET="ptywright-aarch64-apple-darwin.tar.gz" ;;
            x86_64)          ASSET="ptywright-x86_64-apple-darwin.tar.gz" ;;
            *)
                echo "Error: unsupported macOS architecture: ${ARCH}" >&2
                exit 1
                ;;
        esac
        ;;
    *)
        echo "Error: unsupported OS: ${OS}" >&2
        echo "  ptywright ships prebuilt binaries for Linux and macOS only." >&2
        echo "  For other platforms, install via cargo:" >&2
        echo "    cargo install --git https://github.com/${REPO}" >&2
        exit 1
        ;;
esac

if [ "${VERSION}" = "latest" ]; then
    URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
    SUMS_URL="https://github.com/${REPO}/releases/latest/download/SHA256SUMS"
else
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
    SUMS_URL="https://github.com/${REPO}/releases/download/${VERSION}/SHA256SUMS"
fi

echo "Installing ptywright (${VERSION}) for ${OS}/${ARCH}..."
echo "  from: ${URL}"
echo "  to:   ${INSTALL_DIR}/ptywright"

if ! command -v curl >/dev/null 2>&1; then
    echo "Error: curl is required but not installed." >&2
    exit 1
fi

mkdir -p "${INSTALL_DIR}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT

if ! curl -fsSL "${URL}" -o "${TMPDIR}/${ASSET}"; then
    echo "Error: failed to download ${URL}" >&2
    echo "  Available releases: https://github.com/${REPO}/releases" >&2
    exit 1
fi

if curl -fsSL "${SUMS_URL}" -o "${TMPDIR}/SHA256SUMS" 2>/dev/null; then
    EXPECTED="$(grep " ${ASSET}\$" "${TMPDIR}/SHA256SUMS" | awk '{print $1}')"
    if [ -n "${EXPECTED}" ]; then
        if command -v sha256sum >/dev/null 2>&1; then
            ACTUAL="$(sha256sum "${TMPDIR}/${ASSET}" | awk '{print $1}')"
        elif command -v shasum >/dev/null 2>&1; then
            ACTUAL="$(shasum -a 256 "${TMPDIR}/${ASSET}" | awk '{print $1}')"
        else
            ACTUAL=""
        fi
        if [ -n "${ACTUAL}" ] && [ "${ACTUAL}" != "${EXPECTED}" ]; then
            echo "Error: checksum mismatch for ${ASSET}" >&2
            echo "  expected: ${EXPECTED}" >&2
            echo "  got:      ${ACTUAL}" >&2
            exit 1
        fi
    fi
fi

tar -xzf "${TMPDIR}/${ASSET}" -C "${TMPDIR}"
install -m 755 "${TMPDIR}/ptywright" "${INSTALL_DIR}/ptywright"

if [ "${OS}" = "Darwin" ]; then
    xattr -d com.apple.quarantine "${INSTALL_DIR}/ptywright" 2>/dev/null || true
fi

echo ""
echo "ptywright installed to ${INSTALL_DIR}/ptywright"

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo ""
        echo "Add ${INSTALL_DIR} to your PATH:"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        echo ""
        echo "Or add it to your shell profile (~/.bashrc, ~/.zshrc, etc.)"
        ;;
esac

"${INSTALL_DIR}/ptywright" --version
