#!/bin/sh
# Install bk (Beankeeper Desktop) from beankeeper.app.
#
# Usage:
#   curl -fsSL https://beankeeper.app/download/latest/install.sh | sh
#
# Options (environment variables):
#   BK_VERSION   - version to install (default: the mirror's latest version)
#   BK_DOWNLOAD_BASE - HTTPS release mirror (default: https://beankeeper.app/download/)
#   BK_INSTALL   - install directory (default: ~/.local/bin)
#   BK_NO_SERVICE=1 or --no-service - install the binary without background hosting
#   BK_NO_AGENTS=1 or --no-agents - skip local agent configuration
#   BK_REQUIRE_AGENT_SETUP=1 - require managed service and registration commands
#   GH_TOKEN or GITHUB_TOKEN - authentication for discovery with the GitHub Releases override

set -eu

REPO="Govcraft/beankeeper-desktop"
GITHUB_DOWNLOAD_BASE="https://github.com/${REPO}/releases/download"
INSTALL_DIR="${BK_INSTALL:-$HOME/.local/bin}"
NO_SERVICE="${BK_NO_SERVICE:-0}"
NO_AGENTS="${BK_NO_AGENTS:-0}"
REQUIRE_AGENT_SETUP="${BK_REQUIRE_AGENT_SETUP:-0}"
# Filled by scripts/release.py for published installers; never hand-edit hashes.
COSIGN_VERSION='@COSIGN_VERSION@'
COSIGN_LINUX_AMD64='@COSIGN_LINUX_AMD64@'
COSIGN_LINUX_ARM64='@COSIGN_LINUX_ARM64@'
COSIGN_DARWIN_AMD64='@COSIGN_DARWIN_AMD64@'
COSIGN_DARWIN_ARM64='@COSIGN_DARWIN_ARM64@'
VERSION_PATTERN='^[0-9]+[.][0-9]+[.][0-9]+(-rc[.][0-9]+)?$'

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
    resolve_download_base
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

resolve_download_base() {
    DOWNLOAD_BASE="${BK_DOWNLOAD_BASE:-https://beankeeper.app/download}"
    while [ "${DOWNLOAD_BASE%/}" != "$DOWNLOAD_BASE" ]; do
        DOWNLOAD_BASE="${DOWNLOAD_BASE%/}"
    done
    case "$DOWNLOAD_BASE" in
        *[[:space:]]*|*\?*|*\#*) fatal "BK_DOWNLOAD_BASE must be an HTTPS URL without a query or fragment" ;;
        https://[!/]*) ;;
        *) fatal "BK_DOWNLOAD_BASE must be an HTTPS URL" ;;
    esac
}

download() {
    curl -q -fsSL --globoff --proto '=https' --proto-redir '=https' "$@"
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

# Each JSON reader emits one record per non-draft release: the prerelease
# boolean, tag, and asset names. Restrict fields to filename characters so
# metadata cannot inject a record or split a field.
read_release_records() {
    if command -v jq >/dev/null 2>&1; then
        jq -rs '
            def filename: type == "string" and test("\\A[A-Za-z0-9._-]+\\z");
            if length != 1 or (.[0] | type != "array") then error("expected a release list") else .[0][] end
            | objects
            | select(.draft == false and (.prerelease | type == "boolean")
                     and (.tag_name | filename) and (.assets | type == "array"))
            | [(.prerelease | tostring), .tag_name, (.assets[] | objects | .name | select(filename))]
            | join(" ")
        '
    elif [ "$OS" = Darwin ] && command -v osascript >/dev/null 2>&1; then
        osascript -l JavaScript -e '
            ObjC.import("Foundation");
            function filename(value) {
                return typeof value === "string" && value.length > 0 && !/[^A-Za-z0-9._-]/.test(value);
            }
            function run() {
                const data = $.NSFileHandle.fileHandleWithStandardInput.readDataToEndOfFile;
                const releases = JSON.parse(ObjC.unwrap($.NSString.alloc.initWithDataEncoding(data, $.NSUTF8StringEncoding)));
                if (!Array.isArray(releases)) throw new Error("expected a release list");
                return releases.filter(release =>
                    release && release.draft === false && typeof release.prerelease === "boolean" &&
                    filename(release.tag_name) && Array.isArray(release.assets)
                ).map(release => [
                    String(release.prerelease), release.tag_name,
                    ...release.assets.map(asset => asset && asset.name).filter(filename)
                ].join(" ")).join("\n");
            }
        '
    elif command -v python3 >/dev/null 2>&1; then
        python3 -c '
import json
import re
import sys

def filename(value):
    return isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9._-]+", value)

try:
    releases = json.load(sys.stdin.buffer)
    if not isinstance(releases, list):
        raise ValueError("expected a release list")
except ValueError as error:
    sys.exit(str(error))

for release in releases:
    if not isinstance(release, dict) or release.get("draft") is not False:
        continue
    prerelease, tag, assets = (release.get(field) for field in ("prerelease", "tag_name", "assets"))
    if not isinstance(prerelease, bool) or not filename(tag) or not isinstance(assets, list):
        continue
    names = (asset.get("name") for asset in assets if isinstance(asset, dict))
    print(str(prerelease).lower(), tag, *(name for name in names if filename(name)))
'
    else
        fatal "release discovery needs python3 or jq; install one or set BK_VERSION to a known signed release"
    fi
}

select_release_version() {
    prefer_stable=1
    if [ "$REQUIRE_AGENT_SETUP" = 1 ]; then
        prefer_stable=0
    fi
    LC_ALL=C awk -v target="$TARGET" -v prefer_stable="$prefer_stable" -v version_pattern="$VERSION_PATTERN" '
        function compare_digits(left, right) {
            sub(/^0+/, "", left)
            sub(/^0+/, "", right)
            if (length(left) != length(right)) return length(left) - length(right)
            if (("v" left) == ("v" right)) return 0
            return ("v" left) > ("v" right) ? 1 : -1
        }
        function is_newer(version, best,    left, right, left_stable, right_stable, i, order) {
            left_stable = split(version, left, /[.-]/) == 3
            right_stable = split(best, right, /[.-]/) == 3
            if (prefer_stable && left_stable != right_stable) return left_stable
            for (i = 1; i <= 3; i++) {
                order = compare_digits(left[i], right[i])
                if (order != 0) return order > 0
            }
            if (left_stable != right_stable) return left_stable
            return !left_stable && compare_digits(left[5], right[5]) > 0
        }
        function has_asset(name) { return index(" " $0 " ", " " name " ") != 0 }
        $2 ~ /^beankeeper-desktop-v/ {
            version = substr($2, length("beankeeper-desktop-v") + 1)
            if (version !~ version_pattern || ($1 == "true") != (version ~ /-rc\./)) next
            archive = "bk-" version "-" target ".tar.gz"
            if (!has_asset(archive) || !has_asset(archive ".sigstore") ||
                !has_asset("checksums-sha256.txt") || !has_asset("checksums-sha256.txt.sigstore")) next
            if (best == "" || is_newer(version, best)) best = version
        }
        END { if (best != "") print best }
    '
}

resolve_version() {
    VERSION="${BK_VERSION:-}"
    if [ -z "$VERSION" ]; then
        if [ "$DOWNLOAD_BASE" = "$GITHUB_DOWNLOAD_BASE" ]; then
            info "finding the latest published release with signature bundles for ${TARGET}..."
            releases="$(list_published_releases)"
            records="$(printf '%s\n' "$releases" | read_release_records)" ||
                fatal "could not read published release metadata"
            VERSION="$(printf '%s\n' "$records" | select_release_version)"
            [ -n "$VERSION" ] || fatal "no published release with signature bundles found for ${TARGET}"
        else
            VERSION="$(download "${DOWNLOAD_BASE}/latest/version.txt")" ||
                fatal "could not read the latest release version; retry or set BK_VERSION to a signed release"
            VERSION="${VERSION%"$(printf '\r')"}"
        fi
    fi
    case "$VERSION" in *[!0-9.rc-]*) fatal "invalid version: $VERSION" ;; esac
    printf '%s\n' "$VERSION" | LC_ALL=C grep -Eq "$VERSION_PATTERN" || fatal "invalid version: $VERSION"
}

download_and_install() {
    ARCHIVE="bk-${VERSION}-${TARGET}.tar.gz"
    RELEASE_URL="${DOWNLOAD_BASE}/${VERSION}"
    if [ "$DOWNLOAD_BASE" = "$GITHUB_DOWNLOAD_BASE" ]; then
        RELEASE_URL="${DOWNLOAD_BASE}/beankeeper-desktop-v${VERSION}"
    fi

    BK_TEMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$BK_TEMP_DIR"' EXIT

    info "downloading bk v${VERSION} for ${TARGET}..."
    download "${RELEASE_URL}/${ARCHIVE}" -o "${BK_TEMP_DIR}/${ARCHIVE}"

    info "verifying checksum..."
    download "${RELEASE_URL}/checksums-sha256.txt" -o "${BK_TEMP_DIR}/checksums-sha256.txt"

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
    ref="refs/tags/beankeeper-desktop-v${VERSION}"
    IDENTITY="https://github.com/${REPO}/.github/workflows/release.yml@${ref}"
    prepare_cosign

    for ARTIFACT in "$ARCHIVE" checksums-sha256.txt; do
        info "verifying Sigstore bundle for ${ARTIFACT}..."
        if BUNDLE_STATUS="$(download "${RELEASE_URL}/${ARTIFACT}.sigstore" \
            -o "${BK_TEMP_DIR}/${ARTIFACT}.sigstore" -w '%{http_code}')"; then
            :
        elif [ "$BUNDLE_STATUS" = 404 ]; then
            fatal "required Sigstore bundle missing for ${ARTIFACT} (HTTP 404)"
        else
            fatal "could not download Sigstore bundle for ${ARTIFACT}"
        fi
        "$COSIGN" verify-blob \
            --bundle "${BK_TEMP_DIR}/${ARTIFACT}.sigstore" \
            --certificate-identity "$IDENTITY" \
            --certificate-oidc-issuer https://token.actions.githubusercontent.com \
            "${BK_TEMP_DIR}/${ARTIFACT}" \
            || fatal "Sigstore verification failed for ${ARTIFACT}"
    done
}

# Nothing downloaded here can execute until its release-generated hash matches.
prepare_cosign() {
    COSIGN="$(command -v cosign || true)"
    [ -z "$COSIGN" ] || return 0
    case "$OS:$ARCH_TARGET" in
        Linux:x86_64) cosign_asset=cosign-linux-amd64; cosign_hash="$COSIGN_LINUX_AMD64" ;;
        Linux:aarch64) cosign_asset=cosign-linux-arm64; cosign_hash="$COSIGN_LINUX_ARM64" ;;
        Darwin:x86_64) cosign_asset=cosign-darwin-amd64; cosign_hash="$COSIGN_DARWIN_AMD64" ;;
        Darwin:aarch64) cosign_asset=cosign-darwin-arm64; cosign_hash="$COSIGN_DARWIN_ARM64" ;;
    esac
    COSIGN="${BK_TEMP_DIR}/cosign"
    if ! (
        printf '%s\n' "$cosign_hash" | LC_ALL=C grep -Eq '^[a-f0-9]{64}$' &&
        download "https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}/${cosign_asset}" -o "$COSIGN" &&
        [ "$(compute_sha256 "$COSIGN")" = "$cosign_hash" ] &&
        chmod 700 "$COSIGN"
    ) >/dev/null 2>&1; then
        printf '%s\n' "Could not securely prepare release verification; retry the installer." >&2
        exit 1
    fi
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
