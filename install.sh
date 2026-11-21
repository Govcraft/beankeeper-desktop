#!/bin/sh
# Install bk (Beankeeper Desktop) from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Govcraft/beankeeper-desktop/main/install.sh | sh
#
# Options (environment variables):
#   BK_VERSION   - version to install (default: latest)
#   BK_INSTALL   - install directory (default: ~/.local/bin)
#   BK_NO_SERVICE=1 or --no-service - install the binary without background hosting
#   BK_NO_AGENTS=1 or --no-agents - skip local agent configuration

set -eu

REPO="Govcraft/beankeeper-desktop"
INSTALL_DIR="${BK_INSTALL:-$HOME/.local/bin}"
NO_SERVICE="${BK_NO_SERVICE:-0}"
NO_AGENTS="${BK_NO_AGENTS:-0}"

main() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --no-service) NO_SERVICE=1 ;;
            --no-agents) NO_AGENTS=1 ;;
            *) fatal "usage: install.sh [--no-service] [--no-agents]" ;;
        esac
        shift
    done
    detect_platform
    resolve_version
    if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$'; then
        fatal "invalid version: $VERSION"
    fi
    download_and_install
    verify
    if [ "$NO_SERVICE" != 1 ]; then
        "${INSTALL_DIR}/bk" service install --no-agents || fatal "background setup failed; run bk service install --no-agents in your desktop session"
        if [ "$NO_AGENTS" != 1 ]; then
            "${INSTALL_DIR}/bk" agents connect --all || fatal "agent registration needs attention; run bk agents doctor and retry bk agents connect --all"
        fi
    fi
}

detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)  OS_TARGET="unknown-linux-gnu" ;;
        Darwin) OS_TARGET="apple-darwin" ;;
        *)      fatal "unsupported OS: $OS" ;;
    esac

    case "$ARCH" in
        x86_64|amd64)  ARCH_TARGET="x86_64" ;;
        aarch64|arm64) ARCH_TARGET="aarch64" ;;
        *)             fatal "unsupported architecture: $ARCH" ;;
    esac

    TARGET="${ARCH_TARGET}-${OS_TARGET}"
}

resolve_version() {
    if [ -n "${BK_VERSION:-}" ]; then
        VERSION="$BK_VERSION"
        return
    fi

    info "fetching latest version..."
    TAG="$(curl -fsSI -o /dev/null -w '%{redirect_url}' \
        "https://github.com/${REPO}/releases/latest" 2>/dev/null \
        | grep -oE '[^/]+$')"

    if [ -z "$TAG" ]; then
        fatal "could not determine latest release"
    fi

    VERSION="${TAG#beankeeper-desktop-v}"
}

download_and_install() {
    ARCHIVE="bk-${VERSION}-${TARGET}.tar.gz"
    URL="https://github.com/${REPO}/releases/download/beankeeper-desktop-v${VERSION}/${ARCHIVE}"
    CHECKSUM_URL="https://github.com/${REPO}/releases/download/beankeeper-desktop-v${VERSION}/checksums-sha256.txt"

    BK_TEMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$BK_TEMP_DIR"' EXIT

    info "downloading bk v${VERSION} for ${TARGET}..."
    curl -fsSL "$URL" -o "${BK_TEMP_DIR}/${ARCHIVE}"

    info "verifying checksum..."
    curl -fsSL "$CHECKSUM_URL" -o "${BK_TEMP_DIR}/checksums-sha256.txt"

    verify_signatures

    EXPECTED="$(awk -v archive="$ARCHIVE" '$2 == archive {print $1}' "${BK_TEMP_DIR}/checksums-sha256.txt")"
    if [ -z "$EXPECTED" ]; then
        fatal "no checksum found for ${ARCHIVE}"
    fi

    ACTUAL="$(compute_sha256 "${BK_TEMP_DIR}/${ARCHIVE}")"
    if [ "$ACTUAL" != "$EXPECTED" ]; then
        fatal "checksum mismatch: expected ${EXPECTED}, got ${ACTUAL}"
    fi

    tar xzf "${BK_TEMP_DIR}/${ARCHIVE}" -C "$BK_TEMP_DIR"

    if [ -x "${INSTALL_DIR}/bk" ] && "${INSTALL_DIR}/bk" service --help >/dev/null 2>&1; then
        "${INSTALL_DIR}/bk" service stop || fatal "could not stop the existing background service"
    fi
    mkdir -p "$INSTALL_DIR"
    if [ -w "$INSTALL_DIR" ]; then
        # Rename a complete binary so a running inode is never overwritten.
        install -m755 "${BK_TEMP_DIR}/bk-${VERSION}-${TARGET}/bk" "${INSTALL_DIR}/.bk-new-$$"
        mv -f "${INSTALL_DIR}/.bk-new-$$" "${INSTALL_DIR}/bk"
    else
        info "installing to ${INSTALL_DIR} (requires sudo)..."
        sudo install -m755 "${BK_TEMP_DIR}/bk-${VERSION}-${TARGET}/bk" "${INSTALL_DIR}/.bk-new-$$"
        sudo mv -f "${INSTALL_DIR}/.bk-new-$$" "${INSTALL_DIR}/bk"
    fi
}

verify_signatures() {
    if ! command -v cosign >/dev/null 2>&1; then
        warn "cosign not found: only the SHA-256 checksum will be checked; release authenticity is not verified"
        return
    fi

    IDENTITY="https://github.com/${REPO}/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v${VERSION}"
    for ARTIFACT in "$ARCHIVE" checksums-sha256.txt; do
        info "verifying Sigstore bundle for ${ARTIFACT}..."
        if BUNDLE_STATUS="$(curl -fsSL "${URL%/*}/${ARTIFACT}.sigstore" \
            -o "${BK_TEMP_DIR}/${ARTIFACT}.sigstore" -w '%{http_code}')"; then
            :
        elif [ "$BUNDLE_STATUS" = 404 ]; then
            warn "Sigstore bundle missing for ${ARTIFACT} (HTTP 404): this release predates Sigstore signing; only the SHA-256 checksum will be checked for this artifact; release authenticity is not verified"
            continue
        else
            fatal "could not download Sigstore bundle for ${ARTIFACT}"
        fi
        cosign verify-blob \
            --bundle "${BK_TEMP_DIR}/${ARTIFACT}.sigstore" \
            --certificate-identity "$IDENTITY" \
            --certificate-oidc-issuer https://token.actions.githubusercontent.com \
            "${BK_TEMP_DIR}/${ARTIFACT}" \
            || fatal "Sigstore verification failed for ${ARTIFACT}"
    done
}

compute_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fatal "no sha256sum or shasum found"
    fi
}

verify() {
    INSTALLED_VERSION="$("${INSTALL_DIR}/bk" --version)" || fatal "installed bk could not be executed"
    if [ "$INSTALLED_VERSION" != "bk ${VERSION}" ]; then
        fatal "installed binary reports '${INSTALLED_VERSION}', expected 'bk ${VERSION}'"
    fi

    info "installed bk v${VERSION} to ${INSTALL_DIR}/bk"
    if [ "$(command -v bk || true)" != "${INSTALL_DIR}/bk" ]; then
        warn "add ${INSTALL_DIR} to the front of your PATH to use this installation"
    fi
}

info() { printf '  \033[1;32m>\033[0m %s\n' "$1"; }
warn() { printf '  \033[1;33m>\033[0m %s\n' "$1" >&2; }
fatal() { printf '  \033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

main "$@"
