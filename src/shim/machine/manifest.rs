// Copyright 2019-2023 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT
// Copyright 2021-2023 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use ahash::{HashMap, HashMapExt};
use anyhow::{anyhow, Context};
use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding3::CborStore;

// For details on actor name and version, see <https://github.com/filecoin-project/go-state-types/blob/1e6cf0d47cdda75383ef036fc2725d1cf51dbde8/manifest/manifest.go#L36>

pub const ACCOUNT_ACTOR_NAME: &str = "account";
pub const CRON_ACTOR_NAME: &str = "cron";
pub const INIT_ACTOR_NAME: &str = "init";
pub const MARKET_ACTOR_NAME: &str = "storagemarket";
pub const MINER_ACTOR_NAME: &str = "storageminer";
pub const MULTISIG_ACTOR_NAME: &str = "multisig";
pub const PAYCH_ACTOR_NAME: &str = "paymentchannel";
pub const POWER_ACTOR_NAME: &str = "storagepower";
pub const REWARD_ACTOR_NAME: &str = "reward";
pub const SYSTEM_ACTOR_NAME: &str = "system";
pub const VERIFREG_ACTOR_NAME: &str = "verifiedregistry";
// actor version >= 9
pub const DATACAP_ACTOR_NAME: &str = "datacap";
// actor version >= 10
pub const EVM_ACTOR_NAME: &str = "evm";
pub const EAM_ACTOR_NAME: &str = "eam";
pub const PLACEHOLDER_ACTOR_NAME: &str = "placeholder";
pub const ETH_ACCOUNT_ACTOR_NAME: &str = "ethaccount";

/// Manifest is serialized as a tuple of version and manifest actors CID
pub type ManifestCbor = (u32, Cid);

/// Manifest data is serialized as a vector of name-to-actor-CID pair
pub type ManifestActorsCbor = Vec<(String, Cid)>;

/// A mapping of builtin actor CIDs to their respective types.
pub struct Manifest {
    by_name: HashMap<String, Cid>,

    actors_cid: Cid,

    account_code: Cid,
    cron_code: Cid,
    init_code: Cid,
    system_code: Cid,
}

impl Manifest {
    /// Load a manifest from the block store with manifest CID.
    pub fn load<B: Blockstore>(bs: &B, manifest_cid: &Cid) -> anyhow::Result<Self> {
        let (version, actors_cid): ManifestCbor = bs.get_cbor(manifest_cid)?.ok_or_else(|| {
            anyhow::anyhow!("Failed to retrieve manifest with manifest cid {manifest_cid}")
        })?;

        Self::load_with_actors(bs, &actors_cid, version)
    }

    /// Load a manifest from the block store with actors CID and version.
    /// Note that only version 1 is supported.
    pub fn load_with_actors<B: Blockstore>(
        bs: &B,
        actors_cid: &Cid,
        version: u32,
    ) -> anyhow::Result<Self> {
        if version != 1 {
            anyhow::bail!("unsupported manifest version {version}");
        }

        let actors: ManifestActorsCbor = bs.get_cbor(actors_cid)?.ok_or_else(|| {
            anyhow::anyhow!("Failed to retrieve manifest actors with actors cid {actors_cid}")
        })?;

        Self::new(actors, *actors_cid)
    }

    /// Construct a new manifest from actor name/CID tuples.
    fn new(iter: impl IntoIterator<Item = (String, Cid)>, actors_cid: Cid) -> anyhow::Result<Self> {
        let by_name = HashMap::from_iter(iter.into_iter());

        let account_code = *by_name
            .get(ACCOUNT_ACTOR_NAME)
            .context("manifest missing account actor")?;

        let cron_code = *by_name
            .get(CRON_ACTOR_NAME)
            .context("manifest missing cron actor")?;

        let init_code = *by_name
            .get(INIT_ACTOR_NAME)
            .context("manifest missing init actor")?;

        let system_code = *by_name
            .get(SYSTEM_ACTOR_NAME)
            .context("manifest missing system actor")?;

        Ok(Self {
            by_name,
            actors_cid,
            account_code,
            cron_code,
            init_code,
            system_code,
        })
    }

    /// Returns optional actors CID
    pub fn actors_cid(&self) -> Cid {
        self.actors_cid
    }

    /// Returns the code CID for a builtin actor, given the actor's name.
    pub fn code_by_name(&self, name: &str) -> anyhow::Result<&Cid> {
        self.by_name
            .get(name)
            .ok_or_else(|| anyhow!("Failed to retrieve actor code by name {name}"))
    }

    /// Returns true if the given code CID and actor name match.
    pub fn is_actor_by_name(&self, cid: &Cid, name: &str) -> bool {
        self.by_name.get(name).map(|c| c == cid).unwrap_or_default()
    }

    pub fn actors_count(&self) -> usize {
        self.by_name.len()
    }

    pub fn builtin_actors(&self) -> impl Iterator<Item = (&String, &Cid)> {
        self.by_name.iter()
    }

    /// Returns the code CID for the account actor.
    pub fn account_code(&self) -> &Cid {
        &self.account_code
    }

    /// Returns true if the passed code CID is the account actor.
    pub fn is_account_actor(&self, cid: &Cid) -> bool {
        self.account_code() == cid
    }

    /// Returns the code CID for the cron actor.
    pub fn cron_code(&self) -> &Cid {
        &self.cron_code
    }

    /// Returns true if the passed code CID is the cron actor.
    pub fn is_cron_actor(&self, cid: &Cid) -> bool {
        self.cron_code() == cid
    }

    /// Returns the code CID for the init actor.
    pub fn init_code(&self) -> &Cid {
        &self.init_code
    }

    /// Returns true if the passed code CID is the init actor.
    pub fn is_init_actor(&self, cid: &Cid) -> bool {
        self.init_code() == cid
    }

    /// Returns the code CID for the system actor.
    pub fn system_code(&self) -> &Cid {
        &self.system_code
    }

    /// Returns true if the passed code CID is the system actor.
    pub fn is_system_actor(&self, cid: &Cid) -> bool {
        self.system_code() == cid
    }
}
