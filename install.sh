#!/bin/sh
# Install bk (Beankeeper Desktop) from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Govcraft/beankeeper-desktop/main/install.sh | sh
#
# Options (environment variables):
#   BK_VERSION   - version to install (default: signed release, with stable preferred for regular installs)
#   BK_INSTALL   - install directory (default: ~/.local/bin)
#   BK_NO_SERVICE=1 or --no-service - install the binary without background hosting
#   BK_NO_AGENTS=1 or --no-agents - skip local agent configuration
#   BK_REQUIRE_SIGNATURES=1 - require cosign and both release signatures
#   BK_REQUIRE_AGENT_SETUP=1 - require managed service and registration commands
#   GH_TOKEN or GITHUB_TOKEN - optional GitHub authentication for release discovery

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

list_published_releases() (
    release_list_dir="$(mktemp -d)" || fatal "could not create a temporary directory for release discovery"
    trap 'rm -rf "$release_list_dir"' EXIT
    token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
    set -- -q -sS -D "$release_list_dir/headers" -o "$release_list_dir/body" \
        -w '%{http_code}' "https://api.github.com/repos/${REPO}/releases?per_page=100"

    for backoff in 60 120 0; do
        if [ -n "$token" ]; then
            status="$(printf 'Authorization: Bearer %s\n' "$token" | curl "$@" -H @-)" || break
        else
            status="$(curl "$@")" || break
        fi
        case "$status" in
            200) cat "$release_list_dir/body"; return ;;
            403|429) [ -z "$token" ] || break ;;
            *) break ;;
        esac

        # GitHub can ask us to wait longer than the local retry window.
        [ "$backoff" -ne 0 ] || break
        delay="$(awk -v now="$(date +%s)" -v delay="$backoff" '
            { sub(/\r$/, "") }
            tolower($1) == "retry-after:" && $2 ~ /^[0-9]+$/ && $2 > delay { delay = $2 }
            tolower($1) == "x-ratelimit-remaining:" && $2 == 0 { exhausted = 1 }
            tolower($1) == "x-ratelimit-reset:" && $2 ~ /^[0-9]+$/ { reset = $2 }
            END {
                if (exhausted && reset - now > delay) delay = reset - now
                print (delay > 120) ? 0 : delay
            }
        ' "$release_list_dir/headers")" || break
        [ "$delay" -gt 0 ] || break
        warn "GitHub rate limit (HTTP $status). Retrying release discovery in $delay seconds."
        sleep "$delay" || break
    done

    if [ "$status" = 429 ] || { [ "$status" = 403 ] && [ -z "$token" ]; }; then
        fatal "GitHub rate limit (HTTP $status) while listing releases. Set BK_VERSION to a signed release to skip discovery."
    fi
    fatal "could not list published releases; retry or set BK_VERSION to a signed release"
)

resolve_version() {
    if [ -n "${BK_VERSION:-}" ]; then
        VERSION="$BK_VERSION"
        case "$VERSION" in *[!0-9.rc-]*) fatal "invalid version: $VERSION" ;; esac
        printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$' || fatal "invalid version: $VERSION"
        return
    fi
    prefer_stable=true
    if [ "$REQUIRE_AGENT_SETUP" = 1 ]; then
        prefer_stable=false
    fi

    command -v jq >/dev/null 2>&1 || fatal "jq is required to find a signed release; install jq or set BK_VERSION"
    info "finding the latest published release with signature bundles for ${TARGET}..."
    releases="$(list_published_releases)"
    # GitHub metadata selects a tag. Only cosign authenticates the artifacts.
    VERSION="$(printf '%s\n' "$releases" | jq -r --arg target "$TARGET" --argjson prefer_stable "$prefer_stable" '
        [ .[] | select(.draft == false)
          | (.tag_name | capture("\\Abeankeeper-desktop-v(?<version>[0-9]+\\.[0-9]+\\.[0-9]+(?:-rc\\.[0-9]+)?)\\z").version) as $version
          | select(.prerelease == ($version | contains("-rc.")))
          | ("bk-\($version)-\($target).tar.gz") as $archive
          | [.assets[].name] as $assets
          | select(all($archive, "\($archive).sigstore", "checksums-sha256.txt", "checksums-sha256.txt.sigstore";
              . as $required | $assets | index($required) != null))
          | $version ]
        | max_by(split("-rc.") | [
            ($prefer_stable and (length == 1)),
            (.[0] | split(".") | map(tonumber)),
            (length == 1),
            (.[1] | tonumber? // 0)
          ])
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
