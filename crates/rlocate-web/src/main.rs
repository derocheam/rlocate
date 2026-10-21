//! `rlocate-web` — a small local web frontend for rlocate.
//!
//! Serves a single-page UI and a JSON API that searches the local index and any
//! saved remotes. Dependency-free: a minimal HTTP/1.1 server over `std::net`.
//! Binds to localhost by default — it exposes your index over HTTP with no auth,
//! so do not bind it to a public interface.

use anyhow::{Context, Result};
use clap::Parser;
use rlocate_core::index::Index;
use rlocate_core::matcher::{looks_like_glob, MatchKind, Options, Query};
use rlocate_core::remotes::{self, Remote};
use rlocate_core::{net, search};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

const INDEX_HTML: &str = include_str!("../static/index.html");
const DEFAULT_LIMIT: usize = 1000;
const MAX_LIMIT: usize = 20_000;

#[derive(Parser, Debug)]
#[command(name = "rlocate-web", version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long = "listen", value_name = "ADDR", default_value = "127.0.0.1:8080")]
    listen: String,

    /// Local index to search (default: platform db path).
    #[arg(short = 'd', long = "database", value_name = "PATH")]
    database: Option<PathBuf>,

    /// Saved-remotes file (default: ~/.config/rlocate/remotes.conf).
    #[arg(long = "remotes-file", value_name = "PATH")]
    remotes_file: Option<PathBuf>,
}

struct Config {
    db_path: PathBuf,
    remotes: Vec<Remote>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let db_path = args
        .database
        .clone()
        .unwrap_or_else(rlocate_core::default_db_path);
    let remotes = {
        let path = args.remotes_file.clone().or_else(remotes::default_path);
        match path {
            Some(p) => remotes::load(&p).unwrap_or_default(),
            None => Vec::new(),
        }
    };
    let cfg = Arc::new(Config { db_path, remotes });

    let listener =
        TcpListener::bind(&args.listen).with_context(|| format!("binding {}", args.listen))?;
    println!("rlocate-web: http://{}/", args.listen);
    if !args.listen.starts_with("127.") && !args.listen.starts_with("localhost") {
        eprintln!("rlocate-web: WARNING: listening on a non-local address with no authentication");
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cfg = Arc::clone(&cfg);
                thread::spawn(move || {
                    if let Err(e) = handle(stream, &cfg) {
                        eprintln!("rlocate-web: connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("rlocate-web: accept error: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, cfg: &Config) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    // Drain the rest of the request headers.
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" {
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"GET only",
        );
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        "/api/sources" => {
            let body = sources_json(cfg);
            respond(&mut stream, "200 OK", "application/json", body.as_bytes())
        }
        "/api/search" => {
            let body = search_json(cfg, &parse_query(query));
            respond(&mut stream, "200 OK", "application/json", body.as_bytes())
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

fn sources_json(cfg: &Config) -> String {
    let mut s = String::from("{\"local\":");
    s.push_str(if cfg.db_path.exists() {
        "true"
    } else {
        "false"
    });
    s.push_str(",\"remotes\":[");
    for (i, r) in cfg.remotes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&json_str(&r.name));
    }
    s.push_str("]}");
    s
}

fn search_json(cfg: &Config, params: &HashMap<String, String>) -> String {
    let q = params.get("q").map(String::as_str).unwrap_or("");
    if q.is_empty() {
        return r#"{"results":[],"errors":[],"truncated":false}"#.to_string();
    }
    let regexp = params.get("regexp").map(|v| v == "1").unwrap_or(false);
    let ignore_case = params.get("ignore_case").map(|v| v == "1").unwrap_or(false);
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);

    // Which sources to search. Default: local (if present) + every remote.
    let requested: Vec<String> = match params.get("sources") {
        Some(s) if !s.is_empty() => s.split(',').map(|x| x.to_string()).collect(),
        _ => {
            let mut v = Vec::new();
            if cfg.db_path.exists() {
                v.push("local".to_string());
            }
            v.extend(cfg.remotes.iter().map(|r| r.name.clone()));
            v
        }
    };

    let mut results: Vec<(String, Vec<u8>)> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    for src in &requested {
        if results.len() >= limit {
            break;
        }
        let remaining = limit - results.len();
        if src == "local" {
            match search_local(&cfg.db_path, q, regexp, ignore_case, remaining) {
                Ok(paths) => results.extend(paths.into_iter().map(|p| ("local".to_string(), p))),
                Err(e) => errors.push(("local".to_string(), e.to_string())),
            }
        } else if let Some(r) = cfg.remotes.iter().find(|r| &r.name == src) {
            let req = net::Request {
                token: r.token.clone(),
                regexp,
                ignore_case,
                basename: false,
                existing: false,
                limit: remaining as u32,
                patterns: vec![q.to_string()],
            };
            match net::query(&r.host, &req) {
                Ok(paths) => results.extend(paths.into_iter().map(|p| (src.clone(), p))),
                Err(e) => errors.push((src.clone(), e.to_string())),
            }
        } else {
            errors.push((src.clone(), "unknown source".to_string()));
        }
    }

    let truncated = results.len() >= limit;
    results.truncate(limit);

    let mut out = String::from("{\"results\":[");
    for (i, (source, path)) in results.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"source\":");
        out.push_str(&json_str(source));
        out.push_str(",\"path\":");
        out.push_str(&json_str(&String::from_utf8_lossy(path)));
        out.push('}');
    }
    out.push_str("],\"errors\":[");
    for (i, (source, msg)) in errors.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"source\":");
        out.push_str(&json_str(source));
        out.push_str(",\"error\":");
        out.push_str(&json_str(msg));
        out.push('}');
    }
    out.push_str("],\"truncated\":");
    out.push_str(if truncated { "true" } else { "false" });
    out.push('}');
    out
}

fn search_local(
    db_path: &std::path::Path,
    q: &str,
    regexp: bool,
    ignore_case: bool,
    limit: usize,
) -> Result<Vec<Vec<u8>>> {
    let index =
        Index::open(db_path).with_context(|| format!("opening index {}", db_path.display()))?;
    let kind = if regexp {
        MatchKind::Regex
    } else if looks_like_glob(q) {
        MatchKind::Glob
    } else {
        MatchKind::Substring
    };
    let query = Query::new(
        q,
        kind,
        Options {
            ignore_case,
            basename: false,
        },
    )?;
    Ok(search::search_collect(&index, &query, limit)?)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Parse an `application/x-www-form-urlencoded` query string.
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

/// Decode percent-encoding and `+` as space.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Serialize a string as a JSON string literal.
fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
