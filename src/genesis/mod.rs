// Copyright 2019-2023 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use std::{sync::Arc, time};

use crate::blocks::{BlockHeader, Tipset, TipsetKeys};
use crate::state_manager::StateManager;
use crate::utils::{
    db::BlockstoreBufferedWriteExt,
    net::{get_fetch_progress_from_file, get_fetch_progress_from_url},
};
use anyhow::bail;
use cid::Cid;
use futures::AsyncRead;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_car::{load_car, CarReader};
use fvm_ipld_encoding::CborStore;
use log::{debug, info};
use tokio::{fs::File, io::BufReader};
use tokio_util::compat::TokioAsyncReadCompatExt;
use url::Url;

#[cfg(test)]
pub const EXPORT_SR_40: &[u8] = std::include_bytes!("export40.car");

/// Uses an optional file path or the default genesis to parse the genesis and
/// determine if chain store has existing data for the given genesis.
pub async fn read_genesis_header<DB>(
    genesis_fp: Option<&String>,
    genesis_bytes: Option<&[u8]>,
    db: &DB,
) -> Result<BlockHeader, anyhow::Error>
where
    DB: Blockstore + Send + Sync,
{
    let genesis = match genesis_fp {
        Some(path) => {
            let file = File::open(path).await?;
            let reader = BufReader::new(file);
            process_car(reader.compat(), db).await?
        }
        None => {
            debug!("No specified genesis in config. Using default genesis.");
            let genesis_bytes =
                genesis_bytes.ok_or_else(|| anyhow::anyhow!("No default genesis."))?;
            let reader = BufReader::<&[u8]>::new(genesis_bytes);
            process_car(reader.compat(), db).await?
        }
    };

    info!("Initialized genesis: {}", genesis);
    Ok(genesis)
}

pub fn get_network_name_from_genesis<BS>(
    genesis_header: &BlockHeader,
    state_manager: &StateManager<BS>,
) -> Result<String, anyhow::Error>
where
    BS: Blockstore + Clone + Send + Sync + 'static,
{
    // Get network name from genesis state.
    let network_name = state_manager
        .get_network_name(genesis_header.state_root())
        .map_err(|e| anyhow::anyhow!("Failed to retrieve network name from genesis: {}", e))?;
    Ok(network_name)
}

pub async fn initialize_genesis<BS>(
    genesis_fp: Option<&String>,
    state_manager: &StateManager<BS>,
) -> Result<(Tipset, String), anyhow::Error>
where
    BS: Blockstore + Clone + Send + Sync + 'static,
{
    let genesis_bytes = state_manager.chain_config().genesis_bytes();
    let genesis =
        read_genesis_header(genesis_fp, genesis_bytes, state_manager.blockstore()).await?;
    let ts = Tipset::from(&genesis);
    let network_name = get_network_name_from_genesis(&genesis, state_manager)?;
    Ok((ts, network_name))
}

async fn process_car<R, BS>(reader: R, db: &BS) -> Result<BlockHeader, anyhow::Error>
where
    R: AsyncRead + Send + Unpin,
    BS: Blockstore + Send + Sync,
{
    // Load genesis state into the database and get the Cid
    let genesis_cids: Vec<Cid> = load_car(db, reader).await?;
    if genesis_cids.len() != 1 {
        panic!("Invalid Genesis. Genesis Tipset must have only 1 Block.");
    }

    let genesis_block: BlockHeader = db.get_cbor(&genesis_cids[0])?.ok_or_else(|| {
        anyhow::anyhow!("Could not find genesis block despite being loaded using a genesis file")
    })?;

    Ok(genesis_block)
}

/// Import a chain from a CAR file. If the snapshot boolean is set, it will not
/// verify the chain state and instead accept the largest height as genesis.
pub async fn import_chain<DB>(
    sm: &Arc<StateManager<DB>>,
    path: &str,
    skip_load: bool,
) -> anyhow::Result<()>
where
    DB: Blockstore + Clone + Send + Sync + 'static,
{
    let is_remote_file: bool = path.starts_with("http://") || path.starts_with("https://");

    info!("Importing chain from snapshot at: {path}");
    // start import
    let stopwatch = time::Instant::now();
    let (cids, n_records) = if is_remote_file {
        info!("Downloading file...");
        let url = Url::parse(path)?;
        let reader = get_fetch_progress_from_url(&url).await?;
        load_and_retrieve_header(sm.blockstore().clone(), reader, skip_load).await?
    } else {
        info!("Reading file...");
        let reader = get_fetch_progress_from_file(&path).await?;
        load_and_retrieve_header(sm.blockstore().clone(), reader, skip_load).await?
    };

    info!(
        "Loaded {} records from .car file in {}s",
        n_records.unwrap_or_default(),
        stopwatch.elapsed().as_secs()
    );
    if let Some(n_records) = n_records {
        let mut meta = sm.chain_store().file_backed_chain_meta().lock();
        meta.inner_mut().estimated_reachable_records = n_records;
        meta.sync()?;
    }

    let ts = sm.chain_store().tipset_from_keys(&TipsetKeys::new(cids))?;

    if !skip_load {
        let gb = sm.chain_store().tipset_by_height(0, ts.clone(), true)?;
        sm.chain_store().set_genesis(&gb.blocks()[0])?;
        if sm.chain_config().genesis_cid.is_some()
            && !matches!(&sm.chain_config().genesis_cid, Some(expected_cid) if expected_cid ==  &gb.blocks()[0].cid().to_string())
        {
            bail!(
                "Snapshot incompatible with {}. Consider specifying the network with `--chain` flag or \
                 use a custom config file to set expected genesis CID for selected network", 
                sm.chain_config().network
            );
        }
    }

    // Update head with snapshot header tipset
    info!("Accepting {:?} as new head.", ts.cids());
    sm.chain_store().set_heaviest_tipset(ts)?;

    Ok(())
}

pub async fn validate_chain<DB>(
    sm: &Arc<StateManager<DB>>,
    validate_height: i64,
) -> anyhow::Result<()>
where
    DB: Blockstore + Clone + Send + Sync + 'static,
{
    let tipset = sm.chain_store().heaviest_tipset();
    let height = if validate_height > 0 {
        validate_height
    } else {
        (tipset.epoch() + validate_height).max(0)
    };

    info!("Validating imported chain from height: {}", height);
    sm.validate_chain(tipset.clone(), height).await?;

    Ok(())
}

/// Loads car file into database, and returns the block header CIDs from the CAR
/// header.
async fn load_and_retrieve_header<DB, R>(
    store: DB,
    mut reader: R,
    skip_load: bool,
) -> anyhow::Result<(Vec<Cid>, Option<usize>)>
where
    DB: Blockstore + Send + Sync + 'static,
    R: AsyncRead + Send + Unpin,
{
    let result = if skip_load {
        (CarReader::new(&mut reader).await?.header.roots, None)
    } else {
        let (roots, n_records) = forest_load_car(store, &mut reader).await?;
        (roots, Some(n_records))
    };

    Ok(result)
}

pub async fn forest_load_car<DB, R>(store: DB, reader: R) -> anyhow::Result<(Vec<Cid>, usize)>
where
    R: futures::AsyncRead + Send + Unpin,
    DB: Blockstore + Send + Sync + 'static,
{
    // 1GB
    const BUFFER_CAPCITY_BYTES: usize = 1024 * 1024 * 1024;

    let (tx, rx) = flume::bounded(100);
    let write_task =
        tokio::spawn(async move { store.buffered_write(rx, BUFFER_CAPCITY_BYTES).await });
    let mut car_reader = CarReader::new(reader).await?;
    let mut n_records = 0;
    while let Some(block) = car_reader.next_block().await? {
        debug!("Importing block: {}", block.cid);
        n_records += 1;
        tx.send_async((block.cid, block.data)).await?;
    }
    drop(tx);
    write_task.await??;
    Ok((car_reader.header.roots, n_records))
}
