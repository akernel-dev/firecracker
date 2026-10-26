#!/usr/bin/env python3
# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Keep ordinary PRs dependency-free while accepting official release rebases."""

import re
import subprocess
import sys


def git(*args):
    """Run Git in the checkout being validated."""
    return subprocess.check_output(["git", *args])


def version(ref):
    """Read the pinned upstream version without executing the environment file."""
    content = git("show", f"{ref}:resources/akernel/runtime-versions.env").decode()
    matches = re.findall(
        r"^FIRECRACKER_VERSION=([0-9]+\.[0-9]+\.[0-9]+)$", content, re.M
    )
    if len(matches) != 1:
        raise ValueError(f"missing or invalid FIRECRACKER_VERSION at {ref}")
    return matches[0]


def fetch_upstream(release):
    """Fetch the release from the official repository into a private ref."""
    ref = f"refs/akernel-upstream/v{release}"
    git(
        "fetch",
        "--no-tags",
        "https://github.com/firecracker-microvm/firecracker.git",
        f"refs/tags/v{release}:{ref}",
    )
    return ref


def blob(ref, path):
    """Return the tracked blob, or None when the path does not exist."""
    result = subprocess.run(
        ["git", "rev-parse", "--verify", f"{ref}:{path}"],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return result.stdout.strip() if result.returncode == 0 else None


def check(base):
    """Validate every changed lockfile against an official release when rebasing."""
    changed = git("diff", "--no-renames", "--name-only", "-z", base, "HEAD")
    locks = [
        p.decode() for p in changed.split(b"\0") if p.split(b"/")[-1] == b"Cargo.lock"
    ]
    if not locks:
        print("No Cargo.lock changes")
        return

    release = version("HEAD")
    if release == version(base):
        raise ValueError("Cargo.lock changed without an upstream version update")
    upstream = fetch_upstream(release)
    git("merge-base", "--is-ancestor", upstream, "HEAD")
    for path in locks:
        if blob("HEAD", path) != blob(upstream, path):
            raise ValueError(f"{path} differs from the official v{release} release")
    print(f"All {len(locks)} changed lockfiles match official upstream v{release}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: check-dependency-changes.py <base-commit>")
    try:
        check(sys.argv[1])
    except (ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
