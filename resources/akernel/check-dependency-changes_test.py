#!/usr/bin/env python3
# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Exercise release dependency policy with real temporary Git histories."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "dependency_policy", Path(__file__).with_name("check-dependency-changes.py")
)
POLICY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(POLICY)


class DependencyPolicyTest(unittest.TestCase):
    """Official release updates are allowed; extra lockfile edits are rejected."""

    def setUp(self):
        self.original = Path.cwd()
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.addCleanup(os.chdir, self.original)
        os.chdir(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Dependency policy test")
        self.git("config", "user.email", "policy@example.invalid")
        self.write("Cargo.lock", "old lock\n")
        self.write(
            "resources/akernel/runtime-versions.env", "FIRECRACKER_VERSION=1.16.1\n"
        )
        self.base = self.commit()
        self.write("Cargo.lock", "upstream release lock\n")
        self.upstream = self.commit()
        self.write(
            "resources/akernel/runtime-versions.env", "FIRECRACKER_VERSION=1.17.0\n"
        )
        self.commit()

    @staticmethod
    def git(*args):
        """Run Git against this test's isolated repository."""
        return (
            subprocess.check_output(["git", *args], stderr=subprocess.DEVNULL)
            .decode()
            .strip()
        )

    @staticmethod
    def write(path, content):
        """Create or replace a fixture file."""
        path = Path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def commit(self):
        """Commit the fixture and return its identity."""
        self.git("add", "--all")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def check(self, base=None):
        """Use a local official-release fixture, without network access."""
        with mock.patch.object(POLICY, "fetch_upstream", return_value=self.upstream):
            POLICY.check(base or self.base)

    def test_official_release_upgrade(self):
        self.check()

    def test_no_lockfile_changes(self):
        with mock.patch.object(POLICY, "fetch_upstream") as fetch:
            POLICY.check("HEAD")
        fetch.assert_not_called()

    def test_same_version_lockfile_edit(self):
        self.git("checkout", "-q", self.base)
        self.write("Cargo.lock", "unexpected dependency\n")
        self.commit()
        with self.assertRaisesRegex(ValueError, "without an upstream version update"):
            self.check()

    def test_extra_edit_during_upgrade(self):
        self.write("Cargo.lock", "unexpected dependency\n")
        self.commit()
        with self.assertRaisesRegex(ValueError, "differs from the official"):
            self.check()

    def test_deleted_lockfile(self):
        Path("Cargo.lock").unlink()
        self.commit()
        with self.assertRaisesRegex(ValueError, "differs from the official"):
            self.check()

    def test_added_nested_lockfile(self):
        self.write("unexpected/Cargo.lock", "unapproved dependency\n")
        self.commit()
        with self.assertRaisesRegex(ValueError, "differs from the official"):
            self.check()

    def test_release_must_be_ancestor(self):
        candidate = self.git("rev-parse", "HEAD")
        self.git("checkout", "-qb", "unrelated-release", self.base)
        self.write("Cargo.lock", "unrelated lock\n")
        self.upstream = self.commit()
        self.git("checkout", "-q", candidate)
        with self.assertRaises(subprocess.CalledProcessError):
            self.check()

    def test_invalid_version_pin(self):
        self.write(
            "resources/akernel/runtime-versions.env", "FIRECRACKER_VERSION=$(unsafe)\n"
        )
        self.commit()
        with self.assertRaisesRegex(ValueError, "missing or invalid"):
            self.check()


if __name__ == "__main__":
    unittest.main()
