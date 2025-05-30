use std::collections::BTreeSet;
use std::sync::{atomic::AtomicU64, mpsc::sync_channel};
use std::thread;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use rayon::prelude::*;
use tracing::*;

use crate::backend;
use crate::config::Configuration;
use crate::hashing::ObjectId;
use crate::index;
use crate::pack;
use crate::upload;

/// Copy a snapshot, filtering out given paths
#[derive(Debug, Parser)]
#[command(verbatim_doc_comment)]
pub struct Args {
    #[clap(short = 'n', long)]
    dry_run: bool,
}

pub fn run(config: &Configuration, repository: &camino::Utf8Path, args: Args) -> Result<()> {
    let (_cfg, cached_backend) = backend::open(
        repository,
        config.cache_size,
        backend::CacheBehavior::Normal,
    )?;

    let superseded = cached_backend
        .list_indexes()?
        .iter()
        .map(|(idx, _idx_len)| idx)
        .map(backend::id_from_path)
        .collect::<Result<BTreeSet<ObjectId>>>()?;

    let replacing = index::Index {
        supersedes: superseded.clone(),
        ..Default::default()
    };
    // Like backup::spawn_backup_threads, but with no packing threads.
    // We don't need to make new packs, just enumerate the ones we have, index them,
    // and upload that new index.
    let (pack_tx, pack_rx) = sync_channel(num_cpus::get_physical());
    let (upload_tx, upload_rx) = sync_channel(0);

    let indexed_packs = AtomicU64::new(0); // TODO: Progress CLI!
    let indexer = thread::spawn(move || {
        index::index(
            index::Resumable::No,
            replacing,
            pack_rx,
            upload_tx,
            &indexed_packs,
        )
    });

    info!("Reading all packs to build a new index");
    cached_backend
        .list_packs()?
        .par_iter()
        .try_for_each_with::<_, _, Result<()>>(pack_tx, |pack_tx, (pack_file, _pack_len)| {
            let id = backend::id_from_path(pack_file)?;
            let manifest = pack::load_manifest(&id, &cached_backend)?;
            let metadata = pack::PackMetadata { id, manifest };
            pack_tx
                .send(metadata)
                .context("Pack thread closed unexpectedly")?;
            Ok(())
        })?;

    let umode = if args.dry_run {
        upload::Mode::DryRun
    } else {
        upload::Mode::LiveFire
    };
    upload::upload(umode, &cached_backend, upload_rx)?;

    // NB: Before deleting the old indexes, we make sure the new one's been written.
    //     This ensures there's no point in time when we don't have a valid index
    //     of reachable blobs in packs. Prune plays the same game.
    //
    //     Any concurrent writers (writing a backup at the same time)
    //     will upload their own index only after all packs are uploaded,
    //     making sure indexes never refer to missing packs. (I hope...)
    ensure!(indexer.join().unwrap()?, "No new index built");

    if !args.dry_run {
        info!("Uploaded a new index; removing previous ones");
        for old_index in superseded {
            cached_backend.remove_index(&old_index)?;
        }
    }

    Ok(())
}
