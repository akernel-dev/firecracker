# SPDX-License-Identifier: Apache-2.0
"""Run with python3 -m unittest discover -s tools/sealed-block."""

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import checkpoint as cp


class CaptureTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.parent = self.root / "G0"
        self.parent.mkdir()
        self.original = b"a" * 8192
        for name in ("overlay.ext4", "memory"):
            (self.parent / name).write_bytes(self.original)
        (self.parent / "vmstate").write_bytes(b"state-0")
        self.work = self.root / "B"
        cp.init(self.work, self.parent / "overlay.ext4", self.parent)

    def test_publish_retry_after_local_merge_failure(self):
        target = self.root / "G1"
        with (
            (self.work / "epochs").open("r+b") as f,
            cp.mmap.mmap(f.fileno(), 0) as table,
        ):
            words = cp.Words(table)
            state = json.loads((self.work / "state.json").read_text())
            delta = self.work / cp.UPPERS[0]
            with delta.open("r+b") as f:
                f.seek(512)
                f.write(bytes(512))
            table[4097] = 1
            capture = self.work / "capture-1"
            capture.mkdir()
            cp.resize(capture / "memory", 8192)
            with (capture / "memory").open("r+b") as f:
                f.write(b"x" * 4096)
            (capture / "vmstate").write_bytes(b"state-1")
            state["pending"] = {"id": "capture", "target": str(target), "generation": 1}
            words.set(0, 2)
            with (
                patch.object(cp, "merge", side_effect=OSError("injected")),
                self.assertRaises(OSError),
            ):
                cp.finish(self.work, words, state)
            self.assertEqual(words.get(4), 0)
            self.assertTrue(capture.is_dir())
            self.assertEqual(
                (target / "overlay.ext4").read_bytes(),
                self.original[:512] + bytes(512) + self.original[1024:],
            )
            self.assertEqual(
                (target / "memory").read_bytes(), b"x" * 4096 + self.original[4096:]
            )
            self.assertEqual((target / "vmstate").read_bytes(), b"state-1")
            inode = target.stat().st_ino
            with patch.object(cp, "merge") as merge:
                cp.finish(self.work, words, state)
                merge.assert_called_once()
            self.assertEqual(words.get(4), 1)
            self.assertEqual(target.stat().st_ino, inode)
            self.assertFalse(capture.exists())
            self.assertEqual((self.parent / "overlay.ext4").read_bytes(), self.original)
            self.assertEqual(
                json.loads((self.work / "state.json").read_text())["parent"],
                str(target),
            )
