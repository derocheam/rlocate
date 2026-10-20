//! Posting-list codec and intersection.
//!
//! A posting list is a strictly increasing list of docids. Full groups of
//! [`BLOCK_LEN`] (128) docids are delta-bitpacked with the SIMD `bitpacking`
//! codec (the Rust analogue of plocate's TurboPFor): each block stores one
//! `num_bits` byte followed by `num_bits * 16` packed bytes. The trailing
//! `< 128` docids are delta + LEB128 varint encoded. The varint path is also
//! the whole encoding for short lists, which are the common case.

use bitpacking::{BitPacker, BitPacker4x};

/// Number of docids per bitpacked block.
pub const BLOCK_LEN: usize = BitPacker4x::BLOCK_LEN; // 128

/// Append `value` to `out` as an unsigned LEB128 varint.
pub fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Read a LEB128 varint from `buf` starting at `*pos`, advancing `*pos`.
///
/// Returns `None` if the buffer ends mid-varint.
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// Encode a sorted, deduplicated posting list.
///
/// Full [`BLOCK_LEN`] groups are delta-bitpacked; the remainder is delta +
/// varint. Panics in debug builds if `docids` is not strictly increasing.
pub fn encode(docids: &[u32], out: &mut Vec<u8>) {
    debug_assert!(
        docids.windows(2).all(|w| w[1] > w[0]),
        "posting list must be strictly increasing"
    );
    let bp = BitPacker4x::new();
    let full = docids.len() / BLOCK_LEN;
    let mut initial = 0u32; // value preceding the current block (0 before the first)

    for b in 0..full {
        let chunk = &docids[b * BLOCK_LEN..(b + 1) * BLOCK_LEN];
        let num_bits = bp.num_bits_sorted(initial, chunk);
        out.push(num_bits);
        let start = out.len();
        out.resize(start + num_bits as usize * BLOCK_LEN / 8, 0);
        let written = bp.compress_sorted(initial, chunk, &mut out[start..], num_bits);
        debug_assert_eq!(written, num_bits as usize * BLOCK_LEN / 8);
        initial = chunk[BLOCK_LEN - 1];
    }

    // Tail: delta + varint, continuing the running value. `initial` is 0 when
    // there were no full blocks, so docid 0 and small lists encode naturally.
    let mut prev = u64::from(initial);
    for &id in &docids[full * BLOCK_LEN..] {
        write_varint(u64::from(id) - prev, out);
        prev = u64::from(id);
    }
}

/// Decode `count` docids written by [`encode`] from `buf`.
pub fn decode(buf: &[u8], count: usize) -> crate::Result<Vec<u32>> {
    let bp = BitPacker4x::new();
    let mut out = Vec::with_capacity(count);
    let mut pos = 0usize;
    let mut initial = 0u32;
    let full = count / BLOCK_LEN;
    let mut block = [0u32; BLOCK_LEN];

    for _ in 0..full {
        let num_bits = *buf
            .get(pos)
            .ok_or(crate::Error::Corrupt("truncated posting list"))?;
        pos += 1;
        let blen = num_bits as usize * BLOCK_LEN / 8;
        let packed = buf
            .get(pos..pos + blen)
            .ok_or(crate::Error::Corrupt("truncated posting block"))?;
        pos += blen;
        bp.decompress_sorted(initial, packed, &mut block, num_bits);
        out.extend_from_slice(&block);
        initial = block[BLOCK_LEN - 1];
    }

    let tail_len = count % BLOCK_LEN;
    let mut prev = u64::from(initial);
    for _ in 0..tail_len {
        let gap =
            read_varint(buf, &mut pos).ok_or(crate::Error::Corrupt("truncated posting list"))?;
        prev += gap;
        out.push(prev as u32);
    }
    Ok(out)
}

/// Intersect several sorted docid lists, returning the sorted docids present in
/// *all* of them. Processes the shortest list first for efficiency.
pub fn intersect(mut lists: Vec<Vec<u32>>) -> Vec<u32> {
    if lists.is_empty() {
        return Vec::new();
    }
    lists.sort_by_key(|l| l.len());
    let mut acc = lists.swap_remove(0);
    for list in &lists {
        acc = intersect_two(&acc, list);
        if acc.is_empty() {
            break;
        }
    }
    acc
}

/// Intersect two sorted lists with a galloping merge.
fn intersect_two(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16_384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_varint(v, &mut buf);
            let mut pos = 0;
            assert_eq!(read_varint(&buf, &mut pos), Some(v));
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn posting_roundtrip_edge_sizes() {
        for &n in &[0usize, 1, 2, 127, 128, 129, 1000] {
            let docids: Vec<u32> = (0..n as u32).map(|i| i * 3).collect();
            let mut buf = Vec::new();
            encode(&docids, &mut buf);
            assert_eq!(decode(&buf, docids.len()).unwrap(), docids);
        }
    }

    #[test]
    fn posting_roundtrip_irregular_gaps() {
        // Gaps vary widely so consecutive blocks pick different bit widths,
        // and a block straddles a large jump (exercises `initial` carry).
        let mut docids = Vec::new();
        let mut acc = 0u32;
        let mut step = 1u32;
        for i in 0..5 * BLOCK_LEN as u32 + 37 {
            step = 1 + (step.wrapping_mul(1103515245).wrapping_add(12345) >> 24) % 9973;
            acc = acc.checked_add(step).unwrap();
            if i == 200 {
                acc = acc.checked_add(5_000_000).unwrap(); // big jump mid-block
            }
            docids.push(acc);
        }
        let mut buf = Vec::new();
        encode(&docids, &mut buf);
        assert_eq!(decode(&buf, docids.len()).unwrap(), docids);
    }

    #[test]
    fn docid_zero_roundtrips() {
        let docids = vec![0u32, 5, 9];
        let mut buf = Vec::new();
        encode(&docids, &mut buf);
        assert_eq!(decode(&buf, 3).unwrap(), docids);
    }

    #[test]
    fn intersection_matches_reference() {
        let a = vec![1u32, 3, 5, 7, 9];
        let b = vec![3u32, 5, 8, 9];
        let c = vec![5u32, 9, 10];
        assert_eq!(intersect(vec![a, b, c]), vec![5, 9]);
    }

    #[test]
    fn intersection_empty_and_single() {
        assert_eq!(intersect(vec![]), Vec::<u32>::new());
        assert_eq!(intersect(vec![vec![1, 2, 3]]), vec![1, 2, 3]);
    }
}
