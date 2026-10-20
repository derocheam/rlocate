//! Trigram extraction and query decomposition.
//!
//! A *trigram* is a 3-byte window over a path. Each distinct trigram that
//! appears in a path is associated with that path's docid in the index. At
//! query time we extract the trigrams that *must* appear in any matching path
//! and intersect their posting lists to get a small candidate set.
//!
//! Trigrams are packed into a `u32` (only the low 24 bits are used) so they are
//! cheap to sort, hash, and store.

use std::collections::BTreeSet;

/// A trigram packed into the low 24 bits of a `u32`: `b0 << 16 | b1 << 8 | b2`.
pub type Trigram = u32;

/// Pack three bytes into a [`Trigram`].
#[inline]
pub fn pack(b0: u8, b1: u8, b2: u8) -> Trigram {
    (b0 as u32) << 16 | (b1 as u32) << 8 | (b2 as u32)
}

/// Extract every distinct trigram in `path`, in sorted order.
///
/// Paths shorter than 3 bytes contribute no trigrams; such files are only
/// reachable via a full scan at query time.
pub fn extract(path: &[u8], out: &mut BTreeSet<Trigram>) {
    if path.len() < 3 {
        return;
    }
    for w in path.windows(3) {
        out.insert(pack(w[0], w[1], w[2]));
    }
}

/// Collect the distinct trigrams of a single path into a sorted `Vec`.
pub fn of_path(path: &[u8]) -> Vec<Trigram> {
    let mut set = BTreeSet::new();
    extract(path, &mut set);
    set.into_iter().collect()
}

/// The set of trigrams a query requires, plus whether the index can be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryTrigrams {
    /// The pattern yields at least one required trigram; intersect these lists.
    Required(Vec<Trigram>),
    /// The pattern is too short / too broad to prune with trigrams; the caller
    /// must fall back to a full scan of all filename blocks.
    FullScan,
}

/// Decompose a *literal* needle (already case-normalized if needed) into the
/// trigrams that every match must contain.
///
/// A literal of length < 3 cannot be pruned and yields [`QueryTrigrams::FullScan`].
pub fn for_literal(needle: &[u8]) -> QueryTrigrams {
    let tris = of_path(needle);
    if tris.is_empty() {
        QueryTrigrams::FullScan
    } else {
        QueryTrigrams::Required(tris)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_paths_have_no_trigrams() {
        assert!(of_path(b"").is_empty());
        assert!(of_path(b"a").is_empty());
        assert!(of_path(b"ab").is_empty());
    }

    #[test]
    fn extracts_and_dedups() {
        // "aaaa" -> trigrams "aaa","aaa" -> one distinct trigram.
        assert_eq!(of_path(b"aaaa"), vec![pack(b'a', b'a', b'a')]);
        // "abcd" -> "abc","bcd"
        assert_eq!(
            of_path(b"abcd"),
            vec![pack(b'a', b'b', b'c'), pack(b'b', b'c', b'd')]
        );
    }

    #[test]
    fn literal_fullscan_when_short() {
        assert_eq!(for_literal(b"ab"), QueryTrigrams::FullScan);
        match for_literal(b"abc") {
            QueryTrigrams::Required(t) => assert_eq!(t, vec![pack(b'a', b'b', b'c')]),
            _ => panic!("expected required trigrams"),
        }
    }
}
