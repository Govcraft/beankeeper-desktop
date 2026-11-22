#!/usr/bin/env bash
# Run only on disposable hosted runners. The macOS branch changes the default keychain.
set -euo pipefail
[[ "${CI:-}" == true && -n "${RUNNER_TEMP:-}" && -n "${GITHUB_ENV:-}" ]]

umask 077
password=$(openssl rand -hex 32)
case "$RUNNER_OS" in
  Linux)
    sudo apt-get update -qq
    sudo apt-get install -y --no-install-recommends gnome-keyring
    uid=$(id -u)
    sudo systemctl start "user@$uid.service"
    export XDG_RUNTIME_DIR="/run/user/$uid"
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
    keyring_data=$(mktemp -d "$RUNNER_TEMP/bk-test-keyring.XXXXXXXX")
    printf '%s' "$password" |
      XDG_DATA_HOME="$keyring_data" gnome-keyring-daemon --unlock --components=secrets >/dev/null
    for _ in {1..10}; do
      owner=$(busctl --user call org.freedesktop.DBus /org/freedesktop/DBus \
        org.freedesktop.DBus NameHasOwner s org.freedesktop.secrets)
      if [[ "$owner" == "b true" ]]; then break; fi
      sleep 1
    done
    if [[ "$owner" != "b true" ]]; then
      echo "The CI user's Secret Service did not register on D-Bus." >&2
      exit 1
    fi
    {
      echo "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
      echo "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"
    } >> "$GITHUB_ENV"
    ;;
  macOS)
    keychain="$RUNNER_TEMP/bk-test.keychain-db"
    security create-keychain -p "$password" "$keychain"
    security set-keychain-settings -lut 7200 "$keychain"
    security list-keychains -d user -s "$keychain"
    security default-keychain -d user -s "$keychain"
    security unlock-keychain -p "$password" "$keychain"
    echo "BK_TEST_KEYCHAIN=$keychain" >> "$GITHUB_ENV"
    ;;
esac
