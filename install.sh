#!/bin/sh
# Install bk (Beankeeper Desktop) from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Govcraft/beankeeper-desktop/main/install.sh | sh
#
# Options (environment variables):
#   BK_VERSION   - version to install (default: latest stable, or signed release for guided setup)
#   BK_INSTALL   - install directory (default: ~/.local/bin)
#   BK_NO_SERVICE=1 or --no-service - install the binary without background hosting
#   BK_NO_AGENTS=1 or --no-agents - skip local agent configuration
#   BK_REQUIRE_SIGNATURES=1 - require cosign and both release signatures
#   BK_REQUIRE_AGENT_SETUP=1 - require managed service and registration commands

set -eu

REPO="Govcraft/beankeeper-desktop"
INSTALL_DIR="${BK_INSTALL:-$HOME/.local/bin}"
NO_SERVICE="${BK_NO_SERVICE:-0}"
NO_AGENTS="${BK_NO_AGENTS:-0}"
REQUIRE_AGENT_SETUP="${BK_REQUIRE_AGENT_SETUP:-0}"

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
    case "$VERSION" in *[!0-9.rc-]*) fatal "invalid version: $VERSION" ;; esac
    printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$' || fatal "invalid version: $VERSION"
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
    if [ "$REQUIRE_AGENT_SETUP" = 1 ]; then
        resolve_guided_version
        return
    fi

    info "fetching latest version..."
    latest="$(curl -fsSI -o /dev/null -w '%{redirect_url}' \
        "https://github.com/${REPO}/releases/latest")" || fatal "could not determine latest stable release"
    case "$latest" in
        "https://github.com/${REPO}/releases/tag/beankeeper-desktop-v"*) VERSION="${latest##*/beankeeper-desktop-v}" ;;
        *) fatal "no stable release found; set BK_VERSION to a published release version" ;;
    esac
}

resolve_guided_version() {
    command -v jq >/dev/null 2>&1 || fatal "jq is required to find a signed release; install jq or set BK_VERSION"
    info "finding the latest published release with signature bundles for ${TARGET}..."
    releases="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases?per_page=100")" \
        || fatal "could not list published releases; retry or set BK_VERSION to a signed release"
    # GitHub metadata selects a tag. Only cosign authenticates the artifacts.
    VERSION="$(printf '%s\n' "$releases" | jq -r --arg target "$TARGET" '
        [ .[] | select(.draft == false)
          | (.tag_name | capture("\\Abeankeeper-desktop-v(?<version>[0-9]+\\.[0-9]+\\.[0-9]+(?:-rc\\.[0-9]+)?)\\z").version) as $version
          | select(.prerelease == ($version | contains("-rc.")))
          | ("bk-\($version)-\($target).tar.gz") as $archive
          | [.assets[].name] as $assets
          | select(all($archive, "\($archive).sigstore", "checksums-sha256.txt", "checksums-sha256.txt.sigstore";
              . as $required | $assets | index($required) != null))
          | $version ]
        | max_by(split("-rc.") | [ (.[0] | split(".") | map(tonumber)), (length == 1), (.[1] | tonumber? // 0) ])
        // empty
    ')" || fatal "could not read published release metadata"
    [ -n "$VERSION" ] || fatal "no published release with signature bundles found for ${TARGET}"
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
    candidate="${BK_TEMP_DIR}/bk-${VERSION}-${TARGET}/bk"
    if [ "$REQUIRE_AGENT_SETUP" = 1 ]; then
        if ! "$candidate" service install --help >/dev/null 2>&1 ||
            ! "$candidate" agents connect --help >/dev/null 2>&1; then
            fatal "bk v${VERSION} lacks managed service or agent registration commands; select a compatible signed release"
        fi
    fi

    if [ -x "${INSTALL_DIR}/bk" ] && "${INSTALL_DIR}/bk" service --help >/dev/null 2>&1; then
        "${INSTALL_DIR}/bk" service stop || fatal "could not stop the existing background service"
    fi
    mkdir -p "$INSTALL_DIR"
    if [ -w "$INSTALL_DIR" ]; then
        # Rename a complete binary so a running inode is never overwritten.
        install -m755 "$candidate" "${INSTALL_DIR}/.bk-new-$$"
        mv -f "${INSTALL_DIR}/.bk-new-$$" "${INSTALL_DIR}/bk"
    else
        info "installing to ${INSTALL_DIR} (requires sudo)..."
        sudo install -m755 "$candidate" "${INSTALL_DIR}/.bk-new-$$"
        sudo mv -f "${INSTALL_DIR}/.bk-new-$$" "${INSTALL_DIR}/bk"
    fi
}

verify_signatures() {
    if ! command -v cosign >/dev/null 2>&1; then
        [ "${BK_REQUIRE_SIGNATURES:-0}" != 1 ] || fatal "cosign is required to verify this release"
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
            [ "${BK_REQUIRE_SIGNATURES:-0}" != 1 ] || fatal "required Sigstore bundle missing for ${ARTIFACT} (HTTP 404)"
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
