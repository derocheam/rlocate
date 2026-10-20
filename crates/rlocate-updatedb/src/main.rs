//! `rlocate-updatedb` — build an rlocate index by walking the filesystem.

use anyhow::{Context, Result};
use clap::Parser;
use rayon::prelude::*;
use rlocate_core::{config::PruneConfig, fs, index, names, trigram};
use std::collections::BTreeMap;
use std::fs as stdfs;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::Instant;

/// Build an rlocate index.
#[derive(Parser, Debug)]
#[command(name = "rlocate-updatedb", version, about)]
struct Args {
    /// Directories to index (default: filesystem root).
    #[arg(value_name = "ROOT")]
    roots: Vec<PathBuf>,

    /// Write the index here (default: platform db path).
    #[arg(short = 'o', long = "output", value_name = "PATH")]
    output: Option<PathBuf>,

    /// Read additional prune rules from an updatedb.conf-style file.
    #[arg(long, value_name = "FILE")]
    conf: Option<PathBuf>,

    /// Stay on a single filesystem; do not descend into other mounts.
    #[arg(short = 'x', long = "one-file-system")]
    one_file_system: bool,

    /// zstd compression level for filename blocks.
    #[arg(long, default_value_t = 19)]
    level: i32,

    /// Maximum trained zstd dictionary size in bytes (0 to disable).
    #[arg(long, default_value_t = 64 * 1024)]
    dict_size: usize,

    /// Verbose progress output.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(if args.verbose { "info" } else { "warn" })
        .with_target(false)
        .without_time()
        .init();

    let roots = if args.roots.is_empty() {
        vec![PathBuf::from("/")]
    } else {
        args.roots.clone()
    };
    let db_path = args
        .output
        .clone()
        .unwrap_or_else(rlocate_core::default_db_path);

    let mut cfg = PruneConfig::platform_default();
    if args.one_file_system {
        cfg.cross_mounts = false;
    }
    if let Some(conf) = &args.conf {
        let body = stdfs::read_to_string(conf)
            .with_context(|| format!("reading conf {}", conf.display()))?;
        cfg.merge_conf(&body);
    }

    // 1. Walk.
    let t0 = Instant::now();
    let mut paths = fs::walk(&roots, &cfg);
    tracing::info!("walked {} entries in {:?}", paths.len(), t0.elapsed());

    // 2. Sort (bytewise) + dedup; position in this order is the docid.
    paths.sort_unstable();
    paths.dedup();
    let num_docs = paths.len();
    tracing::info!("{num_docs} unique paths after sort/dedup");

    // 3a. Trigram postings, extracted in parallel. Each path emits
    // (trigram, docid) pairs; sorting by (trigram, docid) groups them with
    // docids already increasing within each trigram.
    let t1 = Instant::now();
    let mut pairs: Vec<(trigram::Trigram, u32)> = paths
        .par_iter()
        .enumerate()
        .flat_map_iter(|(docid, path)| {
            let docid = docid as u32;
            trigram::of_path(path).into_iter().map(move |t| (t, docid))
        })
        .collect();
    pairs.par_sort_unstable();
    let mut postings: BTreeMap<trigram::Trigram, Vec<u32>> = BTreeMap::new();
    for (t, docid) in pairs {
        postings.entry(t).or_default().push(docid);
    }
    tracing::info!("{} distinct trigrams in {:?}", postings.len(), t1.elapsed());

    // 3b. Filename store: frame, train a shared dictionary, compress blocks in
    // parallel.
    let t2 = Instant::now();
    let raw_blocks = {
        let mut nb = names::NameStoreBuilder::new(args.level);
        for path in &paths {
            nb.push(path);
        }
        nb.into_raw_blocks()
    };
    let dict = names::train_dict(&raw_blocks, args.dict_size);
    let compressed: Vec<Vec<u8>> = raw_blocks
        .par_iter()
        .map(|b| names::compress_block(b, &dict, args.level))
        .collect();
    let name_store = names::assemble(&compressed, dict);
    tracing::info!(
        "{} filename blocks ({}B dict) in {:?}",
        raw_blocks.len(),
        name_store.dict.len(),
        t2.elapsed()
    );

    // 4. Write atomically: temp file in the target dir, then rename.
    if let Some(parent) = db_path.parent() {
        stdfs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp_path = db_path.with_extension("db.tmp");
    {
        let file = stdfs::File::create(&tmp_path)
            .with_context(|| format!("creating {}", tmp_path.display()))?;
        let mut w = BufWriter::new(file);
        index::write(&mut w, num_docs as u32, &postings, &name_store).context("writing index")?;
        use std::io::Write;
        w.flush()?;
        w.into_inner()?.sync_all()?;
    }
    stdfs::rename(&tmp_path, &db_path)
        .with_context(|| format!("installing index at {}", db_path.display()))?;

    // Make a root-built system index readable by all users (best effort).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = stdfs::set_permissions(&db_path, stdfs::Permissions::from_mode(0o644));
    }

    let size = stdfs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    println!(
        "rlocate: indexed {num_docs} paths -> {} ({} bytes) in {:?}",
        db_path.display(),
        size,
        t0.elapsed()
    );
    Ok(())
}
