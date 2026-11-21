#!/usr/bin/env python3
"""Compare `bk license trust --json` to the license embed env used at compile time."""

import json
import os
import subprocess
import sys

OVERRIDE_VARS = (
    "BEANKEEPER_LICENSE_SERVICE_URL",
    "BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON",
    "BEANKEEPER_LICENSE_KEYS_FILE",
)


def expected_trust(env):
    url = env.get("BEANKEEPER_LICENSE_SERVICE_URL", "")
    keys = env.get("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON", "")
    if not url.strip() or not keys.strip():
        raise SystemExit(
            "BEANKEEPER_LICENSE_SERVICE_URL and BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON must be set"
        )
    return url, json.loads(keys)


def child_env(env):
    out = dict(env)
    for name in OVERRIDE_VARS:
        out.pop(name, None)
    return out


def check(binary, env=None):
    env = os.environ if env is None else env
    expected_url, expected_keys = expected_trust(env)
    output = subprocess.check_output(
        [binary, "license", "trust", "--json"],
        env=child_env(env),
        text=True,
    )
    trust = json.loads(output)
    if trust.get("service_url") != expected_url:
        raise SystemExit(
            f"embedded service URL {trust.get('service_url')!r} != {expected_url!r}"
        )
    if trust.get("public_keys") != expected_keys:
        raise SystemExit(
            f"embedded public keys {trust.get('public_keys')!r} != {expected_keys!r}"
        )
    print("license embed ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: check-license-embed.py BINARY")
    check(sys.argv[1])
