import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cache import (
    LEGACY_PREFIX,
    MAIN,
    PREFIX,
    current_keys,
    identity,
    missing,
    obsolete,
    trim,
    warm_commands,
)
from catalog import ROOT, families, msrv_toolchain, stable_toolchain

INPUTS = (
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    ".cargo/config.toml",
    "crates/beankeeper-desktop/Cargo.toml",
    "packaging/linux-release.Dockerfile",
    ".github/actions/release-build/action.yml",
)


class Workspace:
    """A copy of only the files cache identities read."""

    def __enter__(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        for name in INPUTS:
            (self.root / name).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / name, self.root / name)
        return self.root

    def __exit__(self, *error):
        self.directory.cleanup()


def entry(key, ref=MAIN, cache_id=1):
    return {"id": cache_id, "key": key, "ref": ref}


class IdentityTests(unittest.TestCase):
    def test_source_edits_keep_every_key(self):
        with Workspace() as root:
            before = current_keys(root)
            source = root / "crates/beankeeper-desktop/src/main.rs"
            source.parent.mkdir(parents=True)
            source.write_text("fn main() {}\n")
            (root / "crates/beankeeper-desktop/build.rs").write_text("fn main() {}\n")
            self.assertEqual(before, current_keys(root))

    def test_lock_file_rotates_every_family(self):
        with Workspace() as root:
            before = current_keys(root)
            with (root / "Cargo.lock").open("a") as lock:
                lock.write("\n# changed\n")
            after = current_keys(root)
            self.assertTrue(all(before[name] != after[name] for name in before))

    def test_line_endings_do_not_change_keys(self):
        with Workspace() as root:
            before = current_keys(root)
            lock = root / "Cargo.lock"
            lock.write_bytes(lock.read_bytes().replace(b"\n", b"\r\n"))
            self.assertEqual(before, current_keys(root))

    def test_linux_build_image_rotates_only_linux_release_families(self):
        with Workspace() as root:
            before = current_keys(root)
            with (root / "packaging/linux-release.Dockerfile").open("a") as image:
                image.write("\n# changed\n")
            after = current_keys(root)
            changed = {name for name in before if before[name] != after[name]}
            self.assertEqual(
                changed,
                {
                    "release-x86_64-unknown-linux-gnu",
                    "release-aarch64-unknown-linux-gnu",
                },
            )

    def test_keys_are_unique_and_prefixed_by_family_and_compiler(self):
        keys = current_keys()
        self.assertEqual(len(set(keys.values())), len(keys))
        for name, family in families().items():
            key, prefix = identity(family)
            self.assertTrue(key.startswith(prefix))
            self.assertTrue(prefix.startswith(f"{PREFIX}{name}-"))
        self.assertIn(msrv_toolchain(), identity(families()["msrv-linux"])[1])
        self.assertIn(stable_toolchain(), identity(families()["dev-linux"])[1])

    def test_family_prefixes_do_not_overlap(self):
        prefixes = [f"{PREFIX}{name}-" for name in families()]
        for prefix in prefixes:
            others = [p for p in prefixes if p != prefix]
            self.assertFalse(any(prefix.startswith(o) for o in others), prefix)


KEYS = {
    "dev-linux": f"{PREFIX}dev-linux-1.100.0-new",
    "msrv-linux": f"{PREFIX}msrv-linux-1.91.0-new",
}


class MaintenanceTests(unittest.TestCase):
    keys = KEYS

    def test_missing_counts_only_main_entries(self):
        entries = [
            entry(self.keys["dev-linux"]),
            entry(self.keys["msrv-linux"], ref="refs/pull/7/merge"),
        ]
        self.assertEqual(missing(entries, self.keys), ["msrv-linux"])

    def test_prune_waits_for_the_replacement(self):
        stale = entry(f"{PREFIX}msrv-linux-1.91.0-old", cache_id=5)
        self.assertEqual(obsolete([stale], self.keys), [])

    def test_prune_removes_superseded_and_legacy_main_entries(self):
        entries = [
            entry(self.keys["dev-linux"], cache_id=1),
            entry(f"{PREFIX}dev-linux-1.99.0-old", cache_id=2),
            entry(f"{LEGACY_PREFIX}stable-Linux-x64", cache_id=3),
            entry(f"{LEGACY_PREFIX}stable-Linux-x64", ref="refs/pull/4/merge", cache_id=4),
            entry(f"{PREFIX}msrv-linux-1.91.0-old", cache_id=6),
            entry("unrelated-cache", cache_id=7),
        ]
        self.assertEqual(sorted(obsolete(entries, self.keys)), [2, 3])


class WarmAndTrimTests(unittest.TestCase):
    def test_warm_recipes_build_what_validation_builds(self):
        catalog = families()
        dev = warm_commands(catalog["dev-linux"])
        self.assertIn(["cargo", "build", "--locked"], dev)
        self.assertTrue(any(c[:2] == ["cargo", "clippy"] for c in dev))
        self.assertTrue(any("--no-run" in c for c in dev))
        self.assertEqual(warm_commands(catalog["dev-windows"]), [["cargo", "build", "--locked"]])
        self.assertEqual(warm_commands(catalog["release-x86_64-apple-darwin"]), [])

    def test_trim_keeps_dependency_libraries_only(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            debug = target / "debug"
            for relative in (
                "bk",
                "bk.d",
                "deps/libserde-1.rlib",
                "deps/serde-1.d",
                "deps/beankeeper_desktop-2",
                "deps/navigation-3.exe",
                "deps/navigation-3.pdb",
                "incremental/x/y",
                "build/ring-4/out/lib.a",
            ):
                path = debug / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("x")
            commands = []
            trim(families()["dev-linux"], target, run=lambda c, check: commands.append(c))
            self.assertEqual(commands, [["cargo", "clean", "--package", "beankeeper-desktop"]])
            remaining = sorted(
                p.relative_to(debug).as_posix() for p in debug.rglob("*") if p.is_file()
            )
            self.assertEqual(
                remaining,
                ["build/ring-4/out/lib.a", "deps/libserde-1.rlib", "deps/serde-1.d"],
            )

    def test_release_trim_cleans_the_target_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            commands = []
            family = families()["release-x86_64-pc-windows-msvc"]
            trim(family, Path(directory), run=lambda c, check: commands.append(c))
            self.assertEqual(commands[0][-3:], ["--release", "--target", "x86_64-pc-windows-msvc"])


if __name__ == "__main__":
    unittest.main()
