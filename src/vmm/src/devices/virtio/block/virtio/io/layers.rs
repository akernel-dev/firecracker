// SPDX-License-Identifier: Apache-2.0
//! Files and sector ownership for a single sealed checkpoint drive.
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::ops::Range;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use vm_memory::{FileOffset, MmapRegion};

const SECTOR: usize = 512;
const MARK_BANKS: usize = 2;

#[derive(Debug)]
pub(super) struct Layers {
    pub files: Vec<File>,
    owner: Vec<u32>,
    shared: MmapRegion<()>,
}

impl Layers {
    pub fn open(live: &File, dir: &Path) -> io::Result<Self> {
        let size = live.metadata()?.len();
        if size == 0 || !size.is_multiple_of(SECTOR as u64) {
            return Err(io::Error::other(
                "checkpoint disk must contain whole sectors",
            ));
        }
        let sectors = usize::try_from(size / SECTOR as u64).map_err(io::Error::other)?;
        let mut files = vec![File::open(dir.join("lower"))?, live.try_clone()?];
        for name in ["upper.2", "upper.3", "merged.0", "merged.1"] {
            files.push(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(dir.join(name))?,
            );
        }
        if files
            .iter()
            .any(|f| !matches!(f.metadata(), Ok(m) if m.len()==size))
        {
            return Err(io::Error::other("layer capacity mismatch"));
        }
        let table = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("epochs"))?;
        let len = sectors
            .checked_mul(MARK_BANKS)
            .and_then(|v| v.checked_add(4096))
            .ok_or_else(|| io::Error::other("checkpoint map too large"))?;
        let mut inodes = std::collections::HashSet::new();
        let local_dev = live.metadata()?.dev();
        for (i, f) in files.iter().chain(std::iter::once(&table)).enumerate() {
            let m = f.metadata()?;
            if !m.is_file() || (i > 0 && m.dev() != local_dev) || !inodes.insert((m.dev(), m.ino()))
            {
                return Err(io::Error::other(
                    "expected distinct regular files and node-local checkpoint slots on one filesystem",
                ));
            }
        }
        if table.metadata()?.len() != len as u64 {
            return Err(io::Error::other("bad map size"));
        }
        let shared = MmapRegion::build(
            Some(FileOffset::new(table, 0)),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
        )
        .map_err(io::Error::other)?;
        let layers = Self {
            files,
            owner: vec![0; sectors],
            shared,
        };
        if (
            layers.epoch(),
            layers.word(4).load(Ordering::Acquire),
            layers.failed(),
        ) != (1, 0, false)
        {
            return Err(io::Error::other(
                "restore requires a fresh checkpoint workspace",
            ));
        }
        Ok(layers)
    }
    // Single coordinator publishes aligned little-endian u32 words on x86_64.
    // Cut requires paused vCPUs and drained disk. A background merge changes
    // only the merged generation. Old fixed files survive until the next drain.
    fn word(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: all header offsets are aligned and lie in the checked 4 KiB
        // header. The host coordinator also accesses these words atomically.
        unsafe { AtomicU32::from_ptr(self.shared.as_ptr().add(offset).cast()) }
    }
    pub fn fail(&self) {
        self.word(8).store(1, Ordering::Release);
    }
    pub fn failed(&self) -> bool {
        self.word(8).load(Ordering::Acquire) != 0
    }
    pub fn epoch(&self) -> usize {
        self.word(0).load(Ordering::Acquire) as usize
    }
    fn write_slot(g: u32) -> u32 {
        1 + g.wrapping_sub(1) % 3
    }
    pub fn active(&self) -> u32 {
        Self::write_slot(self.epoch() as u32)
    }
    fn resolve(stamp: u32, merged: u32) -> u32 {
        if stamp == 0 {
            0
        } else if stamp <= merged {
            4 + merged % 2
        } else {
            Self::write_slot(stamp)
        }
    }
    pub fn sectors(&self, offset: u64, length: u32) -> io::Result<Range<usize>> {
        if self.failed() || self.epoch() == 0 {
            return Err(io::Error::other("invalidated checkpoint disk"));
        }
        let len = length as usize;
        let pos = usize::try_from(offset).map_err(io::Error::other)?;
        let end = pos
            .checked_add(len)
            .ok_or_else(|| io::Error::other("offset overflow"))?;
        if !pos.is_multiple_of(SECTOR)
            || !len.is_multiple_of(SECTOR)
            || end / SECTOR > self.owner.len()
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "unaligned or out-of-range sector I/O",
            ));
        }
        Ok(pos / SECTOR..end / SECTOR)
    }
    pub fn source(&self, range: Range<usize>) -> Option<u32> {
        let s = &self.owner[range];
        let merged = self.word(4).load(Ordering::Acquire);
        let first = Self::resolve(s.first().copied().unwrap_or(0), merged);
        s.iter()
            .all(|&v| Self::resolve(v, merged) == first)
            .then_some(first)
    }
    pub fn read_mixed(&self, range: Range<usize>, out: &mut [u8]) -> io::Result<()> {
        let start = range.start;
        let mut p = start;
        let merged = self.word(4).load(Ordering::Acquire);
        while p < range.end {
            let slot = Self::resolve(self.owner[p], merged);
            let end = p
                + 1
                + self.owner[p + 1..range.end]
                    .iter()
                    .take_while(|&&v| Self::resolve(v, merged) == slot)
                    .count();
            self.files[slot as usize].read_exact_at(
                &mut out[(p - start) * SECTOR..(end - start) * SECTOR],
                (p * SECTOR) as u64,
            )?;
            p = end;
        }
        Ok(())
    }
    pub fn complete(&mut self, range: Range<usize>, epoch: usize, slot: u32) {
        let base = 4096 + ((epoch - 1) % MARK_BANKS) * self.owner.len() + range.start;
        // SAFETY: sectors() validated range and epoch() validated the map index.
        // Only this VMM writes this epoch, the publisher reads it after the epoch closes.
        unsafe {
            self.shared.as_ptr().add(base).write_bytes(1, range.len());
        }
        debug_assert_eq!(Self::write_slot(epoch as u32), slot);
        self.owner[range].fill(epoch as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use vmm_sys_util::tempdir::TempDir;

    fn fixture() -> (TempDir, File) {
        let root = TempDir::new().unwrap();
        for name in [
            "lower", "live", "upper.2", "upper.3", "merged.0", "merged.1", "epochs",
        ] {
            let f = File::create(root.as_path().join(name)).unwrap();
            if name == "epochs" {
                f.set_len(4096 + 16).unwrap();
                f.write_all_at(&1u32.to_ne_bytes(), 0).unwrap();
            } else {
                f.set_len(4096).unwrap();
            }
        }
        let live = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.as_path().join("live"))
            .unwrap();
        (root, live)
    }

    #[test]
    fn handover_preserves_new_writes_and_inherited_sectors() {
        let (root, live) = fixture();
        let mut disk = Layers::open(&live, root.as_path()).unwrap();
        let mut expected = [0u8; 4096];
        for g in 1..=100 {
            disk.word(0).store(g, Ordering::Release);
            let sector = g as usize % 7;
            let slot = disk.active();
            let bytes = [g as u8; 512];
            disk.files[slot as usize]
                .write_all_at(&bytes, (sector * 512) as u64)
                .unwrap();
            disk.complete(sector..sector + 1, g as usize, slot);
            expected[sector * 512..(sector + 1) * 512].copy_from_slice(&bytes);
            if g > 1 {
                // Install a cumulative view of the preceding generation while
                // the current upper already contains a newer write.
                let mut previous = expected;
                previous[sector * 512..(sector + 1) * 512].fill(g.saturating_sub(7) as u8);
                disk.files[4 + (g as usize - 1) % 2]
                    .write_all_at(&previous, 0)
                    .unwrap();
                disk.word(4).store(g - 1, Ordering::Release);
            }
            let mut actual = [0; 4096];
            disk.read_mixed(0..8, &mut actual).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(disk.source(sector..sector + 1), Some(slot));
            assert_eq!(disk.source(7..8), Some(0));
        }
        disk.fail();
        assert!(disk.sectors(0, 512).is_err());
    }

    #[test]
    fn rejects_alias_and_stale_workspace() {
        let (root, live) = fixture();
        let epochs = OpenOptions::new()
            .write(true)
            .open(root.as_path().join("epochs"))
            .unwrap();
        epochs.write_all_at(&2u32.to_ne_bytes(), 0).unwrap();
        assert!(Layers::open(&live, root.as_path()).is_err());
        epochs.write_all_at(&1u32.to_ne_bytes(), 0).unwrap();
        std::fs::remove_file(root.as_path().join("upper.2")).unwrap();
        std::fs::hard_link(root.as_path().join("live"), root.as_path().join("upper.2")).unwrap();
        assert!(Layers::open(&live, root.as_path()).is_err());
    }
}
