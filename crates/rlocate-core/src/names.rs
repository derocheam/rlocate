//! The filename block store.
//!
//! Sorted paths are grouped into fixed-size blocks ([`crate::BLOCK_SIZE`]
//! paths per block). Each block is NUL-separated and zstd-compressed. A block
//! index records the byte offset of every block so any docid's block can be
//! located in O(1) and decompressed on demand.
//!
//! Framing (grouping paths into NUL-separated blocks) is separated from
//! compression so milestone M3 can train a shared zstd dictionary over the raw
//! blocks and compress them in parallel. The serial [`NameStoreBuilder::finish`]
//! path remains for simple callers and tests.

use crate::BLOCK_SIZE;

/// Minimum number of blocks before attempting zstd dictionary training; ZDICT
/// needs a reasonable number of samples to produce a useful dictionary.
pub const MIN_BLOCKS_FOR_DICT: usize = 8;

/// Builder that frames sorted paths into raw (uncompressed) blocks.
pub struct NameStoreBuilder {
    level: i32,
    dict_size: usize,
    current: Vec<u8>,
    current_count: usize,
    raw_blocks: Vec<Vec<u8>>,
}

/// The serialized filename store: a dictionary, per-block byte offsets, and the
/// concatenated compressed block data.
pub struct NameStoreData {
    pub dict: Vec<u8>,
    /// `block_offsets.len()` == number of blocks + 1; the final entry is the
    /// total length so block `i` spans `[offsets[i], offsets[i + 1])`.
    pub block_offsets: Vec<u64>,
    pub data: Vec<u8>,
}

impl NameStoreBuilder {
    /// Frame at `level`, with no trained dictionary.
    pub fn new(level: i32) -> Self {
        Self::with_dict_size(level, 0)
    }

    /// Frame at `level`; `dict_size > 0` requests a trained dictionary of up to
    /// that many bytes (used by the serial [`Self::finish`] path).
    pub fn with_dict_size(level: i32, dict_size: usize) -> Self {
        Self {
            level,
            dict_size,
            current: Vec::new(),
            current_count: 0,
            raw_blocks: Vec::new(),
        }
    }

    /// Append one path (raw bytes, no trailing NUL). Paths must be pushed in the
    /// same sorted order used to assign docids.
    pub fn push(&mut self, path: &[u8]) {
        if self.current_count == BLOCK_SIZE {
            self.seal_current();
        }
        if self.current_count > 0 {
            self.current.push(0); // NUL separator between entries
        }
        self.current.extend_from_slice(path);
        self.current_count += 1;
    }

    fn seal_current(&mut self) {
        if self.current_count == 0 {
            return;
        }
        self.raw_blocks.push(std::mem::take(&mut self.current));
        self.current_count = 0;
    }

    /// Finish framing and return the raw (uncompressed) blocks, for callers that
    /// want to train a dictionary and compress in parallel.
    pub fn into_raw_blocks(mut self) -> Vec<Vec<u8>> {
        self.seal_current();
        self.raw_blocks
    }

    /// Serial build: optionally train a dictionary, then compress every block.
    pub fn finish(mut self) -> NameStoreData {
        self.seal_current();
        let dict = train_dict(&self.raw_blocks, self.dict_size);
        let compressed: Vec<Vec<u8>> = self
            .raw_blocks
            .iter()
            .map(|b| compress_block(b, &dict, self.level))
            .collect();
        assemble(&compressed, dict)
    }
}

/// Train a zstd dictionary over `raw_blocks`, up to `max_size` bytes. Returns an
/// empty dictionary (meaning "no dictionary") when there are too few samples or
/// training fails.
pub fn train_dict(raw_blocks: &[Vec<u8>], max_size: usize) -> Vec<u8> {
    if max_size == 0 || raw_blocks.len() < MIN_BLOCKS_FOR_DICT {
        return Vec::new();
    }
    zstd::dict::from_samples(raw_blocks, max_size).unwrap_or_default()
}

/// Compress one raw block, with or without a dictionary.
pub fn compress_block(raw: &[u8], dict: &[u8], level: i32) -> Vec<u8> {
    if dict.is_empty() {
        zstd::bulk::compress(raw, level).expect("zstd compress")
    } else {
        let mut c = zstd::bulk::Compressor::with_dictionary(level, dict).expect("zstd dict");
        c.compress(raw).expect("zstd compress")
    }
}

/// Concatenate compressed blocks and compute the block offset index.
pub fn assemble(compressed_blocks: &[Vec<u8>], dict: Vec<u8>) -> NameStoreData {
    let mut data = Vec::new();
    let mut block_offsets = Vec::with_capacity(compressed_blocks.len() + 1);
    block_offsets.push(0u64);
    for b in compressed_blocks {
        data.extend_from_slice(b);
        block_offsets.push(data.len() as u64);
    }
    NameStoreData {
        dict,
        block_offsets,
        data,
    }
}

/// Read-side view over a serialized name store backed by (mmapped) slices.
pub struct NameStore<'a> {
    dict: &'a [u8],
    block_offsets: &'a [u64],
    data: &'a [u8],
}

impl<'a> NameStore<'a> {
    pub fn new(dict: &'a [u8], block_offsets: &'a [u64], data: &'a [u8]) -> Self {
        Self {
            dict,
            block_offsets,
            data,
        }
    }

    /// Number of filename blocks.
    pub fn block_count(&self) -> usize {
        self.block_offsets.len().saturating_sub(1)
    }

    /// Decompress block `i` and return its paths as owned byte vectors.
    pub fn block(&self, i: usize) -> crate::Result<Vec<Vec<u8>>> {
        let start = *self
            .block_offsets
            .get(i)
            .ok_or(crate::Error::Corrupt("block index out of range"))? as usize;
        let end = *self
            .block_offsets
            .get(i + 1)
            .ok_or(crate::Error::Corrupt("block index out of range"))? as usize;
        let raw = self
            .data
            .get(start..end)
            .ok_or(crate::Error::Corrupt("block offset out of range"))?;
        let decompressed = self.decompress(raw)?;
        Ok(decompressed
            .split(|&b| b == 0)
            .map(|s| s.to_vec())
            .collect())
    }

    /// Fetch a single path by docid.
    pub fn get(&self, docid: u32) -> crate::Result<Vec<u8>> {
        let block_no = docid as usize / BLOCK_SIZE;
        let within = docid as usize % BLOCK_SIZE;
        let block = self.block(block_no)?;
        block
            .into_iter()
            .nth(within)
            .ok_or(crate::Error::Corrupt("docid not found in block"))
    }

    fn decompress(&self, raw: &[u8]) -> crate::Result<Vec<u8>> {
        // Blocks are small and bounded; cap the decompressed capacity generously.
        let cap = 1 << 20;
        let out = if self.dict.is_empty() {
            zstd::bulk::decompress(raw, cap)
        } else {
            let mut d = zstd::bulk::Decompressor::with_dictionary(self.dict)
                .map_err(|_| crate::Error::Corrupt("bad zstd dictionary"))?;
            d.decompress(raw, cap)
        };
        out.map_err(|_| crate::Error::Corrupt("zstd block decompress failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(paths: &[&[u8]], dict_size: usize) {
        let mut b = NameStoreBuilder::with_dict_size(19, dict_size);
        for p in paths {
            b.push(p);
        }
        let d = b.finish();
        let store = NameStore::new(&d.dict, &d.block_offsets, &d.data);
        for (i, p) in paths.iter().enumerate() {
            assert_eq!(store.get(i as u32).unwrap(), p.to_vec(), "docid {i}");
        }
    }

    #[test]
    fn roundtrip_no_dict() {
        let paths: Vec<&[u8]> = vec![
            b"/usr/bin/bash",
            b"/usr/bin/zsh",
            b"/etc/hosts",
            b"/home/user/file with spaces.txt",
            "unicode/\u{00e9}\u{00e8}.rs".as_bytes(),
        ];
        roundtrip(&paths, 0);
    }

    #[test]
    fn roundtrip_crosses_block_boundary() {
        // More than BLOCK_SIZE entries to force multiple blocks.
        let owned: Vec<Vec<u8>> = (0..BLOCK_SIZE * 2 + 5)
            .map(|i| format!("/path/number/{i:05}").into_bytes())
            .collect();
        let refs: Vec<&[u8]> = owned.iter().map(|v| v.as_slice()).collect();
        roundtrip(&refs, 0);
    }

    #[test]
    fn roundtrip_with_trained_dict() {
        // Enough blocks (> MIN_BLOCKS_FOR_DICT) with repetitive structure so
        // dictionary training kicks in and round-trips correctly.
        let owned: Vec<Vec<u8>> = (0..BLOCK_SIZE * 12)
            .map(|i| format!("/usr/share/doc/package-{i:04}/README.md").into_bytes())
            .collect();
        let refs: Vec<&[u8]> = owned.iter().map(|v| v.as_slice()).collect();

        let mut b = NameStoreBuilder::with_dict_size(19, 16 * 1024);
        for p in &refs {
            b.push(p);
        }
        let d = b.finish();
        assert!(!d.dict.is_empty(), "expected a trained dictionary");
        let store = NameStore::new(&d.dict, &d.block_offsets, &d.data);
        for (i, p) in refs.iter().enumerate() {
            assert_eq!(store.get(i as u32).unwrap(), p.to_vec(), "docid {i}");
        }
    }
}
