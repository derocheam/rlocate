//! Wire protocol for remote queries.
//!
//! A client sends one [`Request`] (pattern(s) + flags + a shared-secret token);
//! the server authenticates the token, runs the search against its local index,
//! and streams matched paths back as length-prefixed frames. The transport is
//! plain TCP with no encryption — intended for a trusted network or behind an
//! SSH tunnel; the token guards against casual access, not a network attacker
//! who can read traffic.
//!
//! ```text
//! Request:  "RLQ1" | token(bytes32) | flags:u8 | limit:u32 | npat:u32 | pattern(bytes32)*
//! Response: "RLR1" | status:u8
//!             status OK    -> (len:u32 | path)*  terminated by len == 0xFFFF_FFFF
//!             status != OK -> message(bytes32)
//! ```
//! `bytes32` = `u32` length followed by that many bytes.

use std::io::{self, Read, Write};

pub const REQ_MAGIC: [u8; 4] = *b"RLQ1";
pub const RESP_MAGIC: [u8; 4] = *b"RLR1";

pub const FLAG_REGEXP: u8 = 1 << 0;
pub const FLAG_IGNORE_CASE: u8 = 1 << 1;
pub const FLAG_BASENAME: u8 = 1 << 2;
pub const FLAG_EXISTING: u8 = 1 << 3;

pub const STATUS_OK: u8 = 0;
pub const STATUS_AUTH_FAILED: u8 = 1;
pub const STATUS_ERROR: u8 = 2;

/// Marks the end of the result stream (no real path frame is this long).
const END_SENTINEL: u32 = u32::MAX;

// Caps to bound allocation from untrusted peers.
const MAX_TOKEN: usize = 4096;
const MAX_PATTERN: usize = 64 * 1024;
const MAX_PATTERNS: usize = 4096;
const MAX_PATH: usize = 1 << 20;

/// A remote search request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub token: String,
    pub regexp: bool,
    pub ignore_case: bool,
    pub basename: bool,
    pub existing: bool,
    pub limit: u32,
    pub patterns: Vec<String>,
}

impl Request {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&REQ_MAGIC)?;
        write_bytes(w, self.token.as_bytes())?;
        let mut flags = 0u8;
        if self.regexp {
            flags |= FLAG_REGEXP;
        }
        if self.ignore_case {
            flags |= FLAG_IGNORE_CASE;
        }
        if self.basename {
            flags |= FLAG_BASENAME;
        }
        if self.existing {
            flags |= FLAG_EXISTING;
        }
        w.write_all(&[flags])?;
        w.write_all(&self.limit.to_le_bytes())?;
        w.write_all(&(self.patterns.len() as u32).to_le_bytes())?;
        for p in &self.patterns {
            write_bytes(w, p.as_bytes())?;
        }
        w.flush()
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Request> {
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if magic != REQ_MAGIC {
            return Err(err("bad request magic"));
        }
        let token = String::from_utf8_lossy(&read_bytes(r, MAX_TOKEN)?).into_owned();
        let mut flags = [0u8; 1];
        r.read_exact(&mut flags)?;
        let flags = flags[0];
        let limit = read_u32(r)?;
        let npat = read_u32(r)? as usize;
        if npat > MAX_PATTERNS {
            return Err(err("too many patterns"));
        }
        let mut patterns = Vec::with_capacity(npat);
        for _ in 0..npat {
            patterns.push(String::from_utf8_lossy(&read_bytes(r, MAX_PATTERN)?).into_owned());
        }
        Ok(Request {
            token,
            regexp: flags & FLAG_REGEXP != 0,
            ignore_case: flags & FLAG_IGNORE_CASE != 0,
            basename: flags & FLAG_BASENAME != 0,
            existing: flags & FLAG_EXISTING != 0,
            limit,
            patterns,
        })
    }
}

/// Write the response header (magic + status). Follow with result frames +
/// [`write_end`] for `STATUS_OK`, or [`write_message`] otherwise.
pub fn write_status<W: Write>(w: &mut W, status: u8) -> io::Result<()> {
    w.write_all(&RESP_MAGIC)?;
    w.write_all(&[status])
}

/// Write one matched path as a length-prefixed frame.
pub fn write_result<W: Write>(w: &mut W, path: &[u8]) -> io::Result<()> {
    debug_assert!((path.len() as u32) < END_SENTINEL);
    w.write_all(&(path.len() as u32).to_le_bytes())?;
    w.write_all(path)
}

/// Mark the end of the result stream.
pub fn write_end<W: Write>(w: &mut W) -> io::Result<()> {
    w.write_all(&END_SENTINEL.to_le_bytes())
}

/// Write an error message (for non-OK statuses).
pub fn write_message<W: Write>(w: &mut W, msg: &str) -> io::Result<()> {
    write_bytes(w, msg.as_bytes())
}

/// Read the response header, returning the status byte.
pub fn read_status<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if magic != RESP_MAGIC {
        return Err(err("bad response magic"));
    }
    let mut status = [0u8; 1];
    r.read_exact(&mut status)?;
    Ok(status[0])
}

/// Read the next result frame, or `None` at the end sentinel.
pub fn read_result<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let len = read_u32(r)?;
    if len == END_SENTINEL {
        return Ok(None);
    }
    if len as usize > MAX_PATH {
        return Err(err("result frame too large"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Read an error message (for non-OK statuses).
pub fn read_message<R: Read>(r: &mut R) -> io::Result<String> {
    Ok(String::from_utf8_lossy(&read_bytes(r, MAX_PATTERN)?).into_owned())
}

/// Constant-time token comparison (guards against trivial timing probes).
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn write_bytes<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)
}

fn read_bytes<R: Read>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let len = read_u32(r)? as usize;
    if len > max {
        return Err(err("field too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn err(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request {
            token: "s3cret".into(),
            regexp: true,
            ignore_case: false,
            basename: true,
            existing: true,
            limit: 42,
            patterns: vec!["foo".into(), "bar*baz".into()],
        };
        let mut buf = Vec::new();
        req.write_to(&mut buf).unwrap();
        let got = Request::read_from(&mut &buf[..]).unwrap();
        assert_eq!(got, req);
    }

    #[test]
    fn result_stream_roundtrip() {
        let mut buf = Vec::new();
        write_status(&mut buf, STATUS_OK).unwrap();
        write_result(&mut buf, b"/usr/bin/bash").unwrap();
        write_result(&mut buf, b"/etc/hosts").unwrap();
        write_end(&mut buf).unwrap();

        let mut r = &buf[..];
        assert_eq!(read_status(&mut r).unwrap(), STATUS_OK);
        assert_eq!(read_result(&mut r).unwrap().unwrap(), b"/usr/bin/bash");
        assert_eq!(read_result(&mut r).unwrap().unwrap(), b"/etc/hosts");
        assert_eq!(read_result(&mut r).unwrap(), None);
    }

    #[test]
    fn token_eq_works() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abc", "abd"));
        assert!(!token_eq("abc", "abcd"));
    }
}
