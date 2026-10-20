//! The on-disk index format (`rlocate.db`).
//!
//! Layout (little-endian, all region offsets absolute from file start):
//!
//! ```text
//! [Header]            fixed size, see HEADER_LEN
//! [Trigram table]     num_trigrams entries, sorted by trigram, 16 bytes each:
//!                       u32 trigram | u32 doc_count | u64 posting_offset
//! [Posting region]    per-trigram delta+varint posting lists (see postings.rs)
//! [Name dictionary]   zstd dictionary (may be empty in M1)
//! [Name block index]  (num_blocks + 1) u64 offsets into the name blocks region
//! [Name blocks]       zstd-compressed, NUL-separated filename blocks
//! ```

use crate::names::{NameStore, NameStoreData};
use crate::postings;
use crate::trigram::Trigram;
use crate::{Error, Result, FORMAT_VERSION, MAGIC};
use memmap2::Mmap;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

const HEADER_LEN: usize = 108;
const TABLE_ENTRY_LEN: usize = 16;

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn get_u32(buf: &[u8], off: usize) -> Result<u32> {
    let b = buf
        .get(off..off + 4)
        .ok_or(Error::Corrupt("header truncated"))?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}
fn get_u64(buf: &[u8], off: usize) -> Result<u64> {
    let b = buf
        .get(off..off + 8)
        .ok_or(Error::Corrupt("header truncated"))?;
    Ok(u64::from_le_bytes(b.try_into().unwrap()))
}

/// Region (offset, length) pairs parsed from the header.
#[derive(Debug, Clone, Copy)]
struct Region {
    off: u64,
    len: u64,
}

impl Region {
    fn slice<'a>(&self, data: &'a [u8]) -> Result<&'a [u8]> {
        data.get(self.off as usize..(self.off + self.len) as usize)
            .ok_or(Error::Corrupt("region out of bounds"))
    }
}

/// Header fields the reader consults after opening. (Regions used only during
/// open — the posting region and block index — are read locally there.)
#[derive(Debug)]
struct Header {
    num_docs: u32,
    num_trigrams: u32,
    table: Region,
    dict: Region,
    blocks: Region,
}

/// Serialize an index to `writer`.
///
/// * `num_docs` — total number of paths (docids `0..num_docs`).
/// * `postings` — sorted map of trigram → strictly-increasing docids.
/// * `names` — the compressed filename store produced by `NameStoreBuilder`.
pub fn write<W: Write>(
    mut writer: W,
    num_docs: u32,
    postings: &BTreeMap<Trigram, Vec<u32>>,
    names: &NameStoreData,
) -> io::Result<()> {
    let num_trigrams = postings.len() as u32;
    let num_blocks = names.block_offsets.len().saturating_sub(1) as u32;

    // Build the posting region and the table entries (with relative offsets).
    let mut posting_region: Vec<u8> = Vec::new();
    let mut table: Vec<u8> = Vec::with_capacity(postings.len() * TABLE_ENTRY_LEN);
    for (&trigram, docids) in postings {
        let rel = posting_region.len() as u64;
        postings::encode(docids, &mut posting_region);
        put_u32(&mut table, trigram);
        put_u32(&mut table, docids.len() as u32);
        put_u64(&mut table, rel); // patched to absolute below
    }

    // Block index as raw little-endian u64s.
    let mut block_index: Vec<u8> = Vec::with_capacity(names.block_offsets.len() * 8);
    for &o in &names.block_offsets {
        put_u64(&mut block_index, o);
    }

    // Compute absolute region offsets.
    let table_off = HEADER_LEN as u64;
    let posting_off = table_off + table.len() as u64;
    let dict_off = posting_off + posting_region.len() as u64;
    let block_index_off = dict_off + names.dict.len() as u64;
    let blocks_off = block_index_off + block_index.len() as u64;

    // Patch table posting offsets from relative to absolute.
    for entry in table.chunks_exact_mut(TABLE_ENTRY_LEN) {
        let rel = u64::from_le_bytes(entry[8..16].try_into().unwrap());
        entry[8..16].copy_from_slice(&(posting_off + rel).to_le_bytes());
    }

    // Header.
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(&MAGIC);
    put_u32(&mut header, FORMAT_VERSION);
    put_u32(&mut header, 0); // flags (reserved)
    put_u32(&mut header, num_docs);
    put_u32(&mut header, num_trigrams);
    put_u32(&mut header, num_blocks);
    put_u32(&mut header, 0); // pad to keep u64s aligned
    put_u64(&mut header, table_off);
    put_u64(&mut header, table.len() as u64);
    put_u64(&mut header, posting_off);
    put_u64(&mut header, posting_region.len() as u64);
    put_u64(&mut header, dict_off);
    put_u64(&mut header, names.dict.len() as u64);
    put_u64(&mut header, block_index_off);
    put_u64(&mut header, block_index.len() as u64);
    put_u64(&mut header, blocks_off);
    put_u64(&mut header, names.data.len() as u64);
    debug_assert_eq!(header.len(), HEADER_LEN);

    writer.write_all(&header)?;
    writer.write_all(&table)?;
    writer.write_all(&posting_region)?;
    writer.write_all(&names.dict)?;
    writer.write_all(&block_index)?;
    writer.write_all(&names.data)?;
    Ok(())
}

/// A memory-mapped, read-only index.
pub struct Index {
    mmap: Mmap,
    header: Header,
    /// Materialized block offsets (num_blocks + 1 entries).
    block_offsets: Vec<u64>,
}

impl Index {
    /// Open and validate an index file at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the index is treated as immutable for the process lifetime.
        let mmap = unsafe { Mmap::map(&file)? };
        Self::from_mmap(mmap)
    }

    fn from_mmap(mmap: Mmap) -> Result<Self> {
        let data = &mmap[..];
        if data.len() < HEADER_LEN {
            return Err(Error::Corrupt("file smaller than header"));
        }
        if data[0..4] != MAGIC {
            return Err(Error::BadMagic);
        }
        let version = get_u32(data, 4)?;
        if version != FORMAT_VERSION {
            return Err(Error::VersionMismatch {
                found: version,
                expected: FORMAT_VERSION,
            });
        }
        let num_docs = get_u32(data, 12)?;
        let num_trigrams = get_u32(data, 16)?;
        let num_blocks = get_u32(data, 20)?;
        // bytes 24..28 are reserved padding; regions start at 28.
        let mut off = 28;
        let mut region = || -> Result<Region> {
            let o = get_u64(data, off)?;
            let l = get_u64(data, off + 8)?;
            off += 16;
            Ok(Region { off: o, len: l })
        };
        let table = region()?;
        let _posting = region()?;
        let dict = region()?;
        let block_index = region()?;
        let blocks = region()?;
        drop(region);

        let header = Header {
            num_docs,
            num_trigrams,
            table,
            dict,
            blocks,
        };

        // Materialize the block-offset table once.
        let bi = block_index.slice(data)?;
        if bi.len() != (num_blocks as usize + 1) * 8 && !(num_blocks == 0 && bi.is_empty()) {
            return Err(Error::Corrupt("block index size mismatch"));
        }
        let block_offsets = bi
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();

        Ok(Self {
            mmap,
            header,
            block_offsets,
        })
    }

    pub fn num_docs(&self) -> u32 {
        self.header.num_docs
    }

    pub fn num_trigrams(&self) -> u32 {
        self.header.num_trigrams
    }

    /// Number of filename blocks.
    pub fn num_blocks(&self) -> usize {
        self.block_offsets.len().saturating_sub(1)
    }

    fn data(&self) -> &[u8] {
        &self.mmap[..]
    }

    /// Look up a trigram's posting list, decoded to docids.
    ///
    /// Returns `Ok(None)` if the trigram is absent from the index (meaning no
    /// path contains it, so the overall result is necessarily empty).
    pub fn posting_list(&self, trigram: Trigram) -> Result<Option<Vec<u32>>> {
        let table = self.header.table.slice(self.data())?;
        let n = self.header.num_trigrams as usize;
        // Binary search the sorted 16-byte entries by trigram key.
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let base = mid * TABLE_ENTRY_LEN;
            let key = u32::from_le_bytes(table[base..base + 4].try_into().unwrap());
            match key.cmp(&trigram) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let count =
                        u32::from_le_bytes(table[base + 4..base + 8].try_into().unwrap()) as usize;
                    let poff =
                        u64::from_le_bytes(table[base + 8..base + 16].try_into().unwrap()) as usize;
                    let buf = self
                        .data()
                        .get(poff..)
                        .ok_or(Error::Corrupt("posting offset out of range"))?;
                    return Ok(Some(postings::decode(buf, count)?));
                }
            }
        }
        Ok(None)
    }

    /// A read-side view over the filename store.
    pub fn names(&self) -> Result<NameStore<'_>> {
        let dict = self.header.dict.slice(self.data())?;
        let blocks = self.header.blocks.slice(self.data())?;
        Ok(NameStore::new(dict, &self.block_offsets, blocks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::NameStoreBuilder;
    use crate::trigram;

    fn build(paths: &[&[u8]]) -> Vec<u8> {
        let mut nb = NameStoreBuilder::new(19);
        let mut postings: BTreeMap<Trigram, Vec<u32>> = BTreeMap::new();
        for (docid, p) in paths.iter().enumerate() {
            nb.push(p);
            for t in trigram::of_path(p) {
                postings.entry(t).or_default().push(docid as u32);
            }
        }
        let names = nb.finish();
        let mut out = Vec::new();
        write(&mut out, paths.len() as u32, &postings, &names).unwrap();
        out
    }

    fn open_bytes(bytes: Vec<u8>) -> Index {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp.as_file(), &bytes).unwrap();
        Index::open(tmp.path()).unwrap()
    }

    #[test]
    fn roundtrip_index() {
        let paths: Vec<&[u8]> = vec![
            b"/etc/hosts",
            b"/usr/bin/bash",
            b"/usr/bin/zsh",
            b"/usr/lib/libc.so",
        ];
        let idx = open_bytes(build(&paths));
        assert_eq!(idx.num_docs(), 4);

        // "bin" posting list should include docids 1 and 2.
        let t = trigram::pack(b'b', b'i', b'n');
        let list = idx.posting_list(t).unwrap().unwrap();
        assert_eq!(list, vec![1, 2]);

        // Missing trigram returns None.
        assert!(idx
            .posting_list(trigram::pack(b'Z', b'Z', b'Z'))
            .unwrap()
            .is_none());

        // Name lookups.
        let names = idx.names().unwrap();
        assert_eq!(names.get(0).unwrap(), b"/etc/hosts".to_vec());
        assert_eq!(names.get(3).unwrap(), b"/usr/lib/libc.so".to_vec());
    }

    #[test]
    fn bad_magic_rejected() {
        let bytes = vec![0u8; HEADER_LEN + 8];
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp.as_file(), &bytes).unwrap();
        assert!(matches!(Index::open(tmp.path()), Err(Error::BadMagic)));
    }
}
