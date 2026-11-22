// iscsid/overlay.rs — writes on top of a disk (the games disk): a sparse file the size of the disk, same offsets, plus
// the set of 64 KB blocks written (saved next to it, so it survives a restart of the machine or the daemon). Reading a
// block: from the overlay once written, else from what is under it. The first write to a block copies it from below
// (read-modify-write), so a block in the set always holds its whole current content. The game update machine writes
// into one; saved, it becomes a frozen layer of a games disk version (games.rs), later merged into the disk itself.
use parking_lot::Mutex;
use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const BLOCK: u64 = 64 << 10;

/// How much a diskless machine may add to an overlay the server keeps for it (its session layer, its copy of a games
/// disk). These files are sparse and as large as the disk they sit on, and every machine's lie on the same drive: with
/// no limit one machine (a game install, a runaway download, a guest on purpose) fills it and every machine's writes
/// fail. Checked when a write needs a new 64 KB block; blocks already written are always rewritten.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limit {
    /// Most bytes this overlay may hold (0 = no limit of its own).
    pub max: u64,
    /// The drive keeps this % free for everyone (0 = no reserve): below it, only machines holding less than `FAIR`
    /// still get new blocks — the big writers stop first, the others go on.
    pub free_pct: u8,
    /// A folder on that drive.
    pub dir: PathBuf,
}

/// Below the reserve, an overlay holding less than this still grows: an ordinary session (page file, temp files,
/// updates of what is installed) stays well under it.
pub const FAIR: u64 = 8 << 30;
/// With less than this free on the drive nobody gets a new block: the server's own files there need room.
pub const FLOOR: u64 = 1 << 30;
/// How a write refused for room starts its error (conn.rs answers it as "space allocation failed", not as a broken
/// disk).
pub const NO_ROOM: &str = "no room: ";

/// Whether an overlay holding `held` bytes may take one more block under `l`, the drive having `space` = (free,
/// size) bytes (None = unknown: not held against the machine).
pub fn room(l: &Limit, held: u64, space: Option<(u64, u64)>) -> Result<(), String> {
    if l.max > 0 && held + BLOCK > l.max {
        return Err(format!(
            "this machine's limit of {} GB for one session is reached",
            l.max >> 30
        ));
    }
    let Some((free, size)) = space else {
        return Ok(());
    };
    if free < FLOOR {
        return Err("the server's drive for the machines' writes is full".into());
    }
    if l.free_pct > 0 && (free as u128) * 100 < (size as u128) * l.free_pct as u128 && held >= FAIR
    {
        return Err(format!(
            "the server's drive for the machines' writes has less than {}% free, and this machine already holds {} GB",
            l.free_pct,
            held >> 30
        ));
    }
    Ok(())
}

pub struct Overlay {
    file: File,
    size: u64,
    /// A diskless machine's own overlay: what it may grow to.
    limit: Option<Limit>,
    /// A write was refused for room (logged once per overlay, not per write).
    refused: std::sync::atomic::AtomicBool,
    /// Blocks written (whole content in `file`). Lock order: `write` then `blocks`.
    blocks: Mutex<HashSet<u32>>,
    /// Serializes writes (two first writes into one block must not both copy the disk over each other's data); holds
    /// the block count last saved to `list`.
    write: Mutex<usize>,
    list: PathBuf,
}

type Base<'a> = &'a dyn Fn(u64, &mut [u8]) -> Result<(), String>;

impl Overlay {
    /// The overlay at `path` with its block list `list`: reopened as it was, or new.
    pub fn open(path: &Path, list: &Path, size: u64) -> Result<Overlay, String> {
        Overlay::open_limited(path, list, size, None)
    }

    /// `open`, growing only as far as `limit` allows.
    pub fn open_limited(
        path: &Path,
        list: &Path,
        size: u64,
        limit: Option<Limit>,
    ) -> Result<Overlay, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if file.metadata().map_err(|e| e.to_string())?.len() < size {
            file.set_len(size)
                .map_err(|e| format!("{}: {e}", path.display()))?; // new, or the disk grew
        }
        let blocks: HashSet<u32> = std::fs::read(list)
            .map(|b| {
                b.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&c| u32::from_le_bytes(c))
                    .collect()
            })
            .unwrap_or_default();
        let n = blocks.len();
        Ok(Overlay {
            file,
            size,
            limit,
            refused: std::sync::atomic::AtomicBool::new(false),
            blocks: Mutex::new(blocks),
            write: Mutex::new(n),
            list: list.into(),
        })
    }

    /// Bytes held (written blocks).
    pub fn used(&self) -> u64 {
        self.blocks.lock().len() as u64 * BLOCK
    }

    fn has(&self, b: u32) -> bool {
        self.blocks.lock().contains(&b)
    }

    /// Whose layer this is, for the log: the folder it lies in (a diskless machine's layers lie in a folder named
    /// by its MAC).
    pub fn folder(&self) -> String {
        self.list
            .parent()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// True the first time it is asked after a write was refused for room: the caller logs that one.
    pub fn first_refusal(&self) -> bool {
        !self
            .refused
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// A write of this layer was refused for room (since it was opened).
    pub fn refused(&self) -> bool {
        self.refused.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Neighbouring blocks on the same side (overlay / below) are read in one go: a 1 MB read is one call, not 16.
    pub fn read_at(&self, base: Base, mut off: u64, mut buf: &mut [u8]) -> Result<(), String> {
        let Some(end) = off.checked_add(buf.len() as u64) else {
            return Err("read past the end of the disk".into());
        };
        // Which blocks of the range are the overlay's, under one lock.
        let first = off / BLOCK;
        let mine: Vec<bool> = {
            let set = self.blocks.lock();
            (first..end.div_ceil(BLOCK))
                .map(|b| set.contains(&(b as u32)))
                .collect()
        };
        while !buf.is_empty() {
            let i = (off / BLOCK - first) as usize;
            let run = mine[i..].iter().take_while(|&&m| m == mine[i]).count() as u64;
            let n = ((off / BLOCK + run) * BLOCK - off).min(buf.len() as u64) as usize;
            if mine[i] {
                self.file
                    .read_exact_at(&mut buf[..n], off)
                    .map_err(|e| format!("overlay read: {e}"))?;
            } else {
                base(off, &mut buf[..n])?;
            }
            buf = &mut buf[n..];
            off += n as u64;
        }
        Ok(())
    }

    pub fn write_at(&self, base: Base, mut off: u64, mut data: &[u8]) -> Result<(), String> {
        if off
            .checked_add(data.len() as u64)
            .is_none_or(|end| end > self.size)
        {
            return Err("write past the end of the disk".into());
        }
        let io = |e: std::io::Error| format!("overlay write: {e}");
        let _w = self.write.lock();
        while !data.is_empty() {
            let b = (off / BLOCK) as u32;
            let n = ((BLOCK - off % BLOCK) as usize).min(data.len());
            if !self.has(b) {
                if let Some(l) = &self.limit {
                    room(l, self.used(), super::games::fs_space(&l.dir))
                        .map_err(|why| format!("{NO_ROOM}{why}"))?;
                }
                // First write here: the whole block from the disk first, so the overlay holds all of it.
                let start = b as u64 * BLOCK;
                let mut whole = vec![0u8; (BLOCK.min(self.size - start)) as usize];
                base(start, &mut whole)?;
                whole[(off - start) as usize..(off - start) as usize + n]
                    .copy_from_slice(&data[..n]);
                self.file.write_all_at(&whole, start).map_err(io)?;
                self.blocks.lock().insert(b); // only now: readers see the disk until the block is complete
            } else {
                self.file.write_all_at(&data[..n], off).map_err(io)?;
            }
            data = &data[n..];
            off += n as u64;
        }
        Ok(())
    }

    /// On disk: data, then the block list naming it (rewritten whole when it grew; ~3 MB for 50 GB of update).
    pub fn flush(&self) -> Result<(), String> {
        let mut saved = self.write.lock();
        self.file
            .sync_data()
            .map_err(|e| format!("overlay sync: {e}"))?;
        let mut v: Vec<u32> = self.blocks.lock().iter().copied().collect();
        if v.len() == *saved {
            return Ok(());
        }
        v.sort_unstable();
        let bytes: Vec<u8> = v.iter().flat_map(|b| b.to_le_bytes()).collect();
        crate::versions::write_durable(&self.list, &bytes)
            .map_err(|e| format!("{}: {e}", self.list.display()))?;
        *saved = v.len();
        Ok(())
    }

    /// Copy every written block into `base` (opened for writing), flushed. Safe to run again after a crash midway.
    pub fn merge_into(&self, base: &File) -> Result<usize, String> {
        let mut v: Vec<u32> = self.blocks.lock().iter().copied().collect();
        v.sort_unstable();
        let mut buf = vec![0u8; BLOCK as usize];
        for &b in &v {
            let start = b as u64 * BLOCK;
            let n = BLOCK.min(self.size - start) as usize;
            self.file
                .read_exact_at(&mut buf[..n], start)
                .map_err(|e| format!("overlay read: {e}"))?;
            base.write_all_at(&buf[..n], start)
                .map_err(|e| format!("iSCSI disk write: {e}"))?;
        }
        base.sync_all()
            .map_err(|e| format!("iSCSI disk sync: {e}"))?;
        Ok(v.len())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/iscsid/overlay.rs"]
mod tests;
