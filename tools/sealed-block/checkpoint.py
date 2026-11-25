#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Reference single-writer coordinator for FC_SEALED_BLOCK_PATH on Linux."""

import argparse
import concurrent.futures
import ctypes
import errno
import fcntl
import http.client
import json
import mmap
import os
import re
import shutil
import socket
import struct
import tempfile
from pathlib import Path

UPPERS = ("overlay.ext4", "upper.2", "upper.3")
MERGED = ("merged.0", "merged.1")
FICLONE = 0x40049409
FICLONERANGE = 0x4020940D
SECTOR = 512


def resize(path, size):
    with path.open("wb") as f:
        f.truncate(size)


def init(work, lower, parent):
    size = lower.stat().st_size
    if not size or size % SECTOR or (parent / "overlay.ext4").stat().st_size != size:
        raise ValueError("expected a complete, sector-aligned disk of matching size")
    for name in ("vmstate", "memory"):
        if not (parent / name).is_file():
            raise ValueError(f"parent lacks {name}")
    work.mkdir(mode=0o700)
    (work / "lower").symlink_to(lower.resolve())
    for name in UPPERS + MERGED:
        resize(work / name, size)
    resize(work / "epochs", 4096 + 2 * (size // SECTOR))
    with (work / "epochs").open("r+b") as f:
        f.write(struct.pack("=I", 1))
    save_state(work, {"parent": str(parent.resolve()), "generation": 0})


def save_state(work, state):
    path = work / "state.new"
    with path.open("w") as out:
        json.dump(state, out)
        out.flush()
        os.fsync(out.fileno())
    path.replace(work / "state.json")
    sync_dir(work)


class Words:
    def __init__(self, table):
        self.table = table
        atomics = ctypes.CDLL("libatomic.so.1")
        self.load = getattr(atomics, "__atomic_load_4")
        self.load.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.load.restype = ctypes.c_uint32
        self.store = getattr(atomics, "__atomic_store_4")
        self.store.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int]
        self.store.restype = None

    def address(self, offset):
        return ctypes.addressof(ctypes.c_char.from_buffer(self.table, offset))

    def get(self, offset):
        return self.load(self.address(offset), 2)

    def set(self, offset, value):
        self.store(self.address(offset), value, 3)


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=120)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(str(self.path))


def api(sock, method, route, body):
    conn = Connection(sock)
    try:
        conn.request(
            method, route, json.dumps(body), {"Content-Type": "application/json"}
        )
        resp = conn.getresponse()
        data = resp.read()
        if resp.status != 204:
            raise OSError(f"{route}: HTTP {resp.status}: {data!r}")
    finally:
        conn.close()


def merge(parent, delta, output, ranges):
    resize(output, parent.stat().st_size)
    with parent.open("rb") as old, delta.open("rb") as src, output.open("r+b") as dst:
        fcntl.ioctl(dst.fileno(), FICLONE, old.fileno())
        align = os.fstatvfs(dst.fileno()).f_frsize
        for offset, length in ranges:
            end = offset + length
            first = min(end, (offset + align - 1) // align * align)
            last = max(first, end // align * align)
            for begin, stop in [(offset, first), (last, end)]:
                if begin == stop:
                    continue
                buf = os.pread(src.fileno(), stop - begin, begin)
                if len(buf) != stop - begin or os.pwrite(
                    dst.fileno(), buf, begin
                ) != len(buf):
                    raise OSError("short sector edge copy")
            if last > first:
                fcntl.ioctl(
                    dst.fileno(),
                    FICLONERANGE,
                    struct.pack("=qQQQ", src.fileno(), first, last - first, first),
                )
        os.fsync(dst.fileno())


def data_ranges(path):
    with path.open("rb") as src:
        off = 0
        size = path.stat().st_size
        while off < size:
            try:
                start = os.lseek(src.fileno(), off, os.SEEK_DATA)
            except OSError as e:
                if e.errno == errno.ENXIO:
                    break
                raise
            off = min(size, os.lseek(src.fileno(), start, os.SEEK_HOLE))
            yield start, off - start


def clone_range(src, dst, start, target, length):
    while length:
        n = os.copy_file_range(
            src.fileno(), dst.fileno(), min(length, 256 << 20), start, target
        )
        if not n:
            raise OSError("short parent or delta pack")
        start += n
        target += n
        length -= n


def compose(parent, delta, path, ranges):
    size = parent.stat().st_size
    ranges = list(ranges)
    pack_path = path.with_suffix(".pack")
    with pack_path.open("x+b") as pack, delta.open("rb") as src:
        for off, length in ranges:
            if off + length > size:
                raise ValueError("range outside parent")
            src.seek(off)
            remaining = length
            while remaining:
                data = src.read(min(remaining, 1 << 20))
                if not data:
                    raise OSError("short frozen range")
                pack.write(data)
                remaining -= len(data)
        pack.flush()
        os.fsync(pack.fileno())
        with parent.open("rb") as old, path.open("xb") as dst:
            clone_range(old, dst, 0, 0, size)
            pos = 0
            for off, length in ranges:
                clone_range(pack, dst, pos, off, length)
                pos += length
            os.fsync(dst.fileno())
    pack_path.unlink()


def sync_dir(path):
    fd = os.open(path, os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def publish(parent, delta, capture, target, ranges, receipt):
    if target.exists():
        if json.loads((target / "manifest.json").read_text()) != receipt:
            raise FileExistsError("published generation conflicts with pending capture")
        return
    # Both source files for copy_file_range must be on the publisher mount.
    tmp = Path(tempfile.mkdtemp(prefix=".sealed-", dir=target.parent))
    try:
        compose(parent / "overlay.ext4", delta, tmp / "overlay.ext4", ranges)
        compose(
            parent / "memory",
            capture / "memory",
            tmp / "memory",
            data_ranges(capture / "memory"),
        )
        with (
            (capture / "vmstate").open("rb") as src,
            (tmp / "vmstate").open("xb") as out,
        ):
            shutil.copyfileobj(src, out)
            out.flush()
            os.fsync(out.fileno())
        with (tmp / "manifest.json").open("x") as f:
            json.dump(receipt, f)
            f.flush()
            os.fsync(f.fileno())
        sync_dir(tmp)
        tmp.rename(target)
        sync_dir(target.parent)
    finally:
        if tmp.exists():
            shutil.rmtree(tmp)


def checkpoint(work, words, state, target, sock):
    import uuid

    g = state["generation"] + 1
    if (
        "pending" in state
        or g >= 2**32 - 1
        or (words.get(0), words.get(4), words.get(8)) != (g, g - 1, 0)
    ):
        raise RuntimeError("checkpoint generation is busy or invalid")
    if target.exists():
        raise FileExistsError(target)
    size = (work / "lower").stat().st_size
    sectors = size // SECTOR
    resize(work / UPPERS[g % 3], size)
    bank = 4096 + (g % 2) * sectors
    words.table[bank : bank + sectors] = bytes(sectors)
    capture = work / f"capture-{g}"
    capture.mkdir()
    state["pending"] = {"generation": g, "target": str(target), "id": str(uuid.uuid4())}
    save_state(work, state)
    api(sock, "PATCH", "/vm", {"state": "Paused"})
    api(
        sock,
        "PUT",
        "/snapshot/create",
        {
            "snapshot_type": "Diff",
            "snapshot_path": str(capture / "vmstate"),
            "mem_file_path": str(capture / "memory"),
            "deferred_sync": True,
        },
    )
    if words.get(8):
        raise RuntimeError("disk drain, sync, or write failure; VM remains paused")
    words.set(0, g + 1)
    api(sock, "PATCH", "/vm", {"state": "Resumed"})


def finish(work, words, state):
    pending = state["pending"]
    g = pending["generation"]
    if words.get(0) != g + 1 or words.get(4) not in (g - 1, g) or words.get(8):
        raise RuntimeError("pending generation does not match the live drive")
    sectors = (work / "lower").stat().st_size // SECTOR
    start = 4096 + ((g - 1) % 2) * sectors
    marks = words.table[start : start + sectors]
    ranges = [
        (m.start() * SECTOR, (m.end() - m.start()) * SECTOR)
        for m in re.finditer(b"\x01+", marks)
    ]
    delta = work / UPPERS[(g - 1) % 3]
    target = Path(pending["target"])
    capture = work / f"capture-{g}"
    with concurrent.futures.ThreadPoolExecutor(1) as worker:
        sent = worker.submit(
            publish,
            Path(state["parent"]),
            delta,
            capture,
            target,
            ranges,
            {
                "generation": g,
                "id": pending["id"],
                "parent": Path(state["parent"]).name,
            },
        )
        if words.get(4) != g:
            merge(work / MERGED[(g - 1) % 2], delta, work / MERGED[g % 2], ranges)
            words.set(4, g)
        sent.result()
    save_state(work, {"generation": g, "parent": str(target)})
    shutil.rmtree(capture)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subs = parser.add_subparsers(dest="action", required=True)
    create = subs.add_parser("init")
    create.add_argument("workspace", type=Path)
    create.add_argument("readonly_disk", type=Path)
    create.add_argument("published_parent", type=Path)
    cut = subs.add_parser("capture")
    cut.add_argument("workspace", type=Path)
    cut.add_argument("socket", type=Path)
    cut.add_argument("target", type=Path)
    retry = subs.add_parser("finish")
    retry.add_argument("workspace", type=Path)
    args = parser.parse_args()
    work = args.workspace.resolve()
    if args.action == "init":
        init(work, args.readonly_disk, args.published_parent)
        return
    with (work / "lock").open("a+b") as lock, (work / "epochs").open("r+b") as fd:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with mmap.mmap(fd.fileno(), 0) as table:
            words = Words(table)
            state = json.loads((work / "state.json").read_text())
            if args.action == "capture":
                checkpoint(work, words, state, args.target.resolve(), args.socket)
            finish(work, words, state)


if __name__ == "__main__":
    main()
