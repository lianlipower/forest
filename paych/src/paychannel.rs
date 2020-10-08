// Copyright 2020 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use super::Error;
use crate::{
    ChannelInfo, FundsReq, Manager, MergeFundsReq, MsgListeners, PaychFundsRes, PaychStore,
    VoucherInfo,
};
extern crate log;
use super::ResourceAccessor;
use actor::account::State as AccountState;
use actor::init::{ExecParams, ExecReturn};
use actor::paych::{
    ConstructorParams, LaneState, Method::Collect, Method::Settle, Method::UpdateChannelState,
    SignedVoucher, State as PaychState, UpdateChannelStateParams,
};
use actor::{ExitCode, Serialized};
use address::Address;
use async_std::sync::{Arc, RwLock};
use async_std::task;
use blockstore::BlockStore;
use chain::get_heaviest_tipset;
use cid::Cid;
use encoding::Cbor;
use flo_stream::{MessagePublisher, Publisher, Subscriber};
use futures::StreamExt;
use ipld_amt::Amt;
use message::UnsignedMessage;
use num_bigint::BigInt;
use num_traits::Zero;
use state_manager::StateManager;
use std::collections::HashMap;
use std::ops::{Add, Sub};
use wallet::KeyStore;

const MESSAGE_CONFIDENCE: i64 = 5;

pub struct ChannelAccessor<DB, KS>
where
    DB: BlockStore + Send + Sync + 'static,
    KS: KeyStore + Send + Sync + 'static,
{
    store: Arc<RwLock<PaychStore>>,
    msg_listeners: MsgListeners,
    funds_req_queue: Arc<RwLock<Vec<FundsReq>>>,
    state: Arc<ResourceAccessor<DB, KS>>,
}

// VoucherCreateResult is the response to calling PaychVoucherCreate
struct _VoucherCreateResult {
    // Voucher that was created, or nil if there was an error or if there
    // were insufficient funds in the channel
    voucher: SignedVoucher,
    // Shortfall is the additional amount that would be needed in the channel
    // in order to be able to create the voucher
    shortfall: BigInt,
}

impl<DB, KS> ChannelAccessor<DB, KS>
where
    DB: BlockStore + Send + Sync + 'static,
    KS: KeyStore + Send + Sync + 'static,
{
    pub fn new(pm: &Manager<DB, KS>) -> Self {
        ChannelAccessor {
            store: pm.store.clone(),
            msg_listeners: MsgListeners::new(),
            funds_req_queue: Arc::new(RwLock::new(Vec::new())),
            state: pm.state.clone(),
        }
    }
    // TODO ask about message0
    async fn message_builder(&self, from: Address) {
        unimplemented!()
    }

    /// Returns channel info by address
    pub async fn get_channel_info(&self, addr: &Address) -> Result<ChannelInfo, Error> {
        self.store.read().await.get_channel_info(addr).await
    }
    /// creates a voucher with the given specification, setting its
    /// nonce, signing the voucher and storing it in the local datastore.
    /// If there are not enough funds in the channel to create the voucher, returns
    /// the shortfall in funds.
    pub async fn create_voucher(
        &mut self,
        ch: Address,
        mut voucher: SignedVoucher,
    ) -> Result<SignedVoucher, Error> {
        let st = self.store.read().await;
        let _ci = st.by_address(ch).await?;

        // set the voucher channel
        voucher.channel_addr = ch;

        // Get the next sequence on the given lane
        voucher.nonce = self.next_sequence_for_lane(ch, voucher.lane).await?;

        // sign the voucher
        let _vb = voucher
            .signing_bytes()
            .map_err(|e| Error::Other(e.to_string()))?;

        // TODO fix
        // let ks = self.state.keystore.read().await;
        // let mut w = Wallet::new(*ks);

        // let sig = w.sign(&ci.control, &vb).unwrap(); // TODO fix
        // voucher.signature = Some(sig);

        // store the voucher
        // TODO determine if returning insufficent error with shortfall is required?
        self.add_voucher(ch, voucher.clone(), Vec::new(), BigInt::zero())
            .await?;

        Ok(voucher)
    }
    /// Returns the next available nonce for lane allocation
    pub async fn next_sequence_for_lane(&self, ch: Address, lane: u64) -> Result<u64, Error> {
        let store = self.store.read().await;
        let vouchers = store.vouchers_for_paych(&ch).await?;

        let mut max_sequence = 0;

        for v in vouchers {
            if v.voucher.lane == lane && max_sequence < v.voucher.nonce {
                max_sequence = v.voucher.nonce;
            }
        }
        Ok(max_sequence + 1)
    }
    /// Returns a HashMap representing validated voucher lane(s)
    pub async fn check_voucher_valid(
        &self,
        ch: Address,
        sv: SignedVoucher,
    ) -> Result<HashMap<u64, LaneState>, Error> {
        let sm = self.state.sa.sm.read().await;
        if sv.channel_addr != ch {
            return Err(Error::Other(
                "voucher channel address doesn't match channel address".to_string(),
            ));
        }

        let (act, pch_state) = self.state.sa.load_paych_state(&ch).await?;
        let heaviest_ts = get_heaviest_tipset(sm.get_block_store().as_ref())
            .map_err(|_| Error::HeaviestTipset)?
            .ok_or_else(|| Error::HeaviestTipset)?;
        let cid = heaviest_ts.parent_state();
        let act_state: AccountState = sm
            .load_actor_state(&pch_state.from, cid)
            .map_err(|err| Error::Other(err.to_string()))?;
        let from = act_state.address;

        let vb = sv
            .signing_bytes()
            .map_err(|err| Error::Other(err.to_string()))?;

        let sig = sv.signature.clone();
        sig.ok_or_else(|| Error::Other("no sig".to_owned()))?
            .verify(&vb, &from)
            .map_err(Error::Other)?;

        let lane_states = self.lane_state(&pch_state, ch).await?;
        let ls = lane_states
            .get(&sv.lane)
            .ok_or_else(|| Error::Other("No lane state for given nonce".to_owned()))?;
        if ls.nonce >= sv.nonce {
            return Err(Error::Other("nonce too low".to_owned()));
        }
        if ls.redeemed >= sv.amount {
            return Err(Error::Other("Voucher amount is lower than amount for voucher amount for voucher with lower nonce".to_owned()));
        }

        // Total redeemed is the total redeemed amount for all lanes, including
        // the new voucher
        // eg
        //
        // lane 1 redeemed:            3
        // lane 2 redeemed:            2
        // voucher for lane 1:         5
        //
        // Voucher supersedes lane 1 redeemed, therefore
        // effective lane 1 redeemed:  5
        //
        // lane 1:  5
        // lane 2:  2
        //          -
        // total:   7
        let merge_len = sv.merges.len();
        let total_redeemed = self.total_redeemed_with_voucher(&lane_states, sv).await?;

        // Total required balance = total redeemed + to send
        // must not exceed actor balance
        let new_total = total_redeemed + pch_state.to_send;
        if act.balance < new_total {
            return Err(Error::Other(
                "Not enough funds in channel to cover voucher".to_owned(),
            ));
        }

        if merge_len != 0 {
            return Err(Error::Other(
                "don't currently support paych lane merges".to_owned(),
            ));
        }

        Ok(lane_states)
    }

    async fn check_voucher_spendable(
        &self,
        ch: Address,
        sv: SignedVoucher,
        secret: Vec<u8>,
        mut proof: Vec<u8>,
    ) -> Result<bool, Error> {
        let recipient = self.get_paych_recipient(&ch).await?;
        let st = self.store.read().await;
        let ci: ChannelInfo = st.by_address(ch).await?;

        // check if voucher has already been submitted
        let submitted = ci.was_voucher_submitted(&sv)?;
        if submitted {
            return Ok(false);
        }

        if (sv.extra != None) & (!proof.is_empty()) {
            let store = self.store.read().await;
            let known = store.vouchers_for_paych(&ch).await?;
            for vi in known {
                if (proof == vi.proof) & (sv == vi.voucher) {
                    info!("using stored proof");
                    proof = vi.proof;
                    break;
                }
                if proof.is_empty() {
                    log::warn!("empty proof for voucher with validation")
                }
            }
        }
        // TODO ask about version compatibility
        let enc = Serialized::serialize(UpdateChannelStateParams { sv, secret, proof })?;

        let sm = self.state.sa.sm.read().await;
        let ret = sm
            .call(
                &mut UnsignedMessage::builder()
                    .to(recipient)
                    .from(ch)
                    .method_num(UpdateChannelState as u64)
                    .params(enc)
                    .build()
                    .map_err(Error::Other)?,
                None,
            )
            .map_err(|e| Error::Other(e.to_string()))?;

        if let Some(code) = ret.msg_rct {
            if code.exit_code != ExitCode::Ok {
                return Ok(false);
            }
        }

        Ok(true)
    }

    async fn get_paych_recipient(&self, ch: &Address) -> Result<Address, Error> {
        let sm = self.state.sa.sm.read().await;
        let heaviest_ts = get_heaviest_tipset(sm.get_block_store().as_ref())
            .map_err(|_| Error::HeaviestTipset)?
            .ok_or_else(|| Error::HeaviestTipset)?;
        let cid = heaviest_ts.parent_state();
        let state: PaychState = sm
            .load_actor_state(ch, cid)
            .map_err(|err| Error::Other(err.to_string()))?;
        Ok(state.to)
    }
    /// Adds voucher to store and returns the delta; the difference between the voucher amount and the highest
    /// previous voucher amount for the lane
    pub async fn add_voucher(
        &mut self,
        ch: Address,
        sv: SignedVoucher,
        proof: Vec<u8>,
        min_delta: BigInt,
    ) -> Result<BigInt, Error> {
        let mut store = self.store.write().await;
        let mut ci = store.by_address(ch).await?;

        // Check if voucher has already been added
        for mut vi in ci.vouchers.iter_mut() {
            if sv != vi.voucher {
                continue;
            }

            // This is a duplicate voucher.
            // Update the proof on the existing voucher
            if (!proof.is_empty()) & (vi.proof != proof) {
                warn!("adding proof to stored voucher");
                vi.proof = proof.clone();
                store.put_channel_info(ci).await?;
                return Ok(BigInt::from(1));
            }
            warn!("Voucher re-added with matching proof");
            return Ok(BigInt::default());
        }

        // Check voucher validity
        let lane_states = self.check_voucher_valid(ch, sv.clone()).await?;

        // the change in value is the delta between the voucher amount and the highest
        // previous voucher amount for the lane
        let mut redeemed = BigInt::default();
        let lane_state = lane_states.get(&sv.lane);
        if let Some(redeem) = lane_state {
            redeemed = redeem.redeemed.clone();
        }

        let delta = sv.amount.clone() - redeemed;

        if min_delta > delta {
            return Err(Error::Other("supplied token amount too low".to_string()));
        }

        ci.vouchers.push(VoucherInfo {
            voucher: sv.clone(),
            proof,
            submitted: false,
        });

        if ci.next_lane <= sv.lane {
            ci.next_lane += 1;
        }

        store.put_channel_info(ci).await?;
        Ok(delta)
    }

    async fn submit_voucher(
        &self,
        ch: Address,
        sv: &SignedVoucher,
        secret: &[u8],
    ) -> Result<Cid, Error> {
        let mut store = self.store.write().await;
        let mut ci = store.by_address(ch).await?;

        let has = ci.has_voucher(sv)?;

        if has.is_some() {
            // Check that the voucher hasn't already been submitted
            if ci.was_voucher_submitted(sv)? {
                return Err(Error::Other(
                    "cannot submit voucher that has already been submitted".to_string(),
                ));
            }
        } else {
            // add voucher to the channel
            ci.vouchers.push(VoucherInfo {
                voucher: sv,
                proof: secret,
                submitted: false,
            });
        }

        // TODO ask about version compatibility
        let enc = Serialized::serialize(UpdateChannelStateParams { ch, sv, secret })?;
        let sm = self.state.sa.sm.read().await;
        let umsg = &mut UnsignedMessage::builder()
            .to(ch)
            .from(ci.control)
            .method_num(UpdateChannelState as u64)
            .params(enc)
            .build()
            .map_err(Error::Other)?;

        sm.call(umsg, None)
            .map_err(|e| Error::Other(e.to_string()))?;

        let smgs = self
            .state
            .mpool
            .mpool_unsigned_msg_push(umsg, self.state.keystore.clone())
            .await
            .map_err(|e| Error::Other(e.to_string()))?;

        // Mark the voucher and any lower-nonce vouchers as having been submitted
        st.mark_voucher_submitted(ci, sv)?;

        Ok(smgs.cid())
    }

    /// Allocates a lane for given address
    pub async fn allocate_lane(&self, ch: Address) -> Result<u64, Error> {
        let mut store = self.store.write().await;
        store.allocate_lane(ch).await
    }
    /// Lists vouchers for given address
    pub async fn list_vouchers(&self, ch: Address) -> Result<Vec<VoucherInfo>, Error> {
        let store = self.store.read().await;
        store.vouchers_for_paych(&ch).await
    }

    /// Retrieves lane states from chain, then applies all vouchers in the data store over the chain state
    pub async fn lane_state(
        &self,
        state: &PaychState,
        ch: Address,
    ) -> Result<HashMap<u64, LaneState>, Error> {
        let sm = self.state.sa.sm.read().await;
        let ls_amt: Amt<LaneState, _> =
            Amt::load(&state.lane_states, sm.get_block_store_ref()).unwrap();
        // Note: we use a map instead of an array to store laneStates because the
        // client sets the lane ID (the index) and potentially they could use a
        // very large index.
        let mut lane_states: HashMap<u64, LaneState> = HashMap::new();
        ls_amt
            .for_each(|i, v| {
                lane_states.insert(i, v.clone());
                Ok(())
            })
            .map_err(|e| Error::Encoding(format!("failed to iterate over values in AMT: {}", e)))?;

        // apply locally stored vouchers
        let st = self.store.read().await;
        let vouchers = st.vouchers_for_paych(&ch).await?;

        for v in vouchers {
            // TODO ask about for range operation in lotus
            let ok = lane_states.contains_key(&v.voucher.lane);
            if !ok {
                lane_states.insert(
                    v.voucher.lane,
                    LaneState {
                        redeemed: BigInt::zero(),
                        nonce: 0,
                    },
                );
            }
            if let Some(mut ls) = lane_states.get_mut(&v.voucher.lane) {
                if v.voucher.nonce < ls.nonce {
                    continue;
                }

                ls.nonce = v.voucher.nonce;
                ls.redeemed = v.voucher.amount;
            } else {
                return Err(Error::Other(format!(
                    "failed to retrieve lane state for {}",
                    v.voucher.lane
                )));
            }
        }

        Ok(lane_states)
    }

    async fn total_redeemed_with_voucher(
        &self,
        lane_states: &HashMap<u64, LaneState>,
        sv: SignedVoucher,
    ) -> Result<BigInt, Error> {
        if !sv.merges.is_empty() {
            return Err(Error::Other("merges not supported yet".to_string()));
        }

        let mut total = BigInt::default();
        for ls in lane_states.values() {
            let val = total.add(ls.nonce);
            total = val
        }

        let lane_ret = lane_states.get(&sv.lane);
        if let Some(lane) = lane_ret {
            // If the voucher is for an existing lane, and the voucher nonce is higher than the lane nonce
            if sv.nonce > lane.nonce {
                // add the delta between the redeemed amount and the voucher
                // amount to the total
                total += sv.amount.sub(&lane.redeemed);
            }
        } else {
            // If the voucher is not for an existing lane, add its value
            total += sv.amount
        }

        Ok(total)
    }
    /// Returns CID of signed message thats prepared to be settled on-chain
    pub async fn settle(&self, ch: Address) -> Result<Cid, Error> {
        let mut store = self.store.write().await;
        let mut ci = store.by_address(ch).await?;

        let umsg: UnsignedMessage = UnsignedMessage::builder()
            .to(ch)
            .from(ci.control)
            .value(BigInt::default())
            .method_num(Settle as u64)
            .build()
            .map_err(Error::Other)?;

        let smgs = self
            .state
            .mpool
            .mpool_unsigned_msg_push(umsg, self.state.keystore.clone())
            .await
            .map_err(|e| Error::Other(e.to_string()))?;

        ci.settling = true;
        store.put_channel_info(ci).await?;

        Ok(smgs.cid()?)
    }
    /// Returns CID of signed message ready to be collected
    pub async fn collect(&self, ch: Address) -> Result<Cid, Error> {
        let store = self.store.read().await;
        let ci = store.by_address(ch).await?;

        let umsg: UnsignedMessage = UnsignedMessage::builder()
            .to(ch)
            .from(ci.control)
            .value(BigInt::default())
            .method_num(Collect as u64)
            .build()
            .map_err(Error::Other)?;

        let smgs = self
            .state
            .mpool
            .mpool_unsigned_msg_push(umsg, self.state.keystore.clone())
            .await
            .map_err(|e| Error::Other(e.to_string()))?;

        Ok(smgs.cid()?)
    }

    // getPaych ensures that a channel exists between the from and to addresses,
    // and adds the given amount of funds.
    // If the channel does not exist a create channel message is sent and the
    // message CID is returned.
    // If the channel does exist an add funds message is sent and both the channel
    // address and message CID are returned.
    // If there is an in progress operation (create channel / add funds), getPaych
    // blocks until the previous operation completes, then returns both the channel
    // address and the CID of the new add funds message.
    // If an operation returns an error, subsequent waiting operations will still
    // be attempted.
    pub async fn get_paych(
        &self,
        from: Address,
        to: Address,
        amt: BigInt,
    ) -> Result<PaychFundsRes, Error> {
        // add the request to add funds to a queue and wait for the result
        let freq = FundsReq::new(from, to, amt);
        let mut sub = freq.promise().await;
        self.enqueue(freq).await?;

        // if there is no promise, block and wait until one is made
        loop {
            let f = sub.next().await;
            if f.is_some() {
                let promise = f.unwrap();
                return Ok(promise);
            }
        }
    }
    /// Queue up an add funds operation
    async fn enqueue(&self, task: FundsReq) -> Result<(), Error> {
        let mut funds_req_vec = self.funds_req_queue.write().await;
        funds_req_vec.push(task);
        drop(funds_req_vec);
        task::spawn(async { self.process_queue().await })
    }

    /// Run operations in the queue
    async fn process_queue(&self) -> Result<(), Error> {
        // Remove cancelled requests
        self.filter_queue().await;

        let funds_req_queue = self.funds_req_queue.read().await;

        // if funds req queue is empty return
        if funds_req_queue.len() == 0 {
            return Ok(());
        }

        // Merge all pending requests into one.
        // For example if there are pending requests for 3, 2, 4 then
        // amt = 3 + 2 + 4 = 9
        let mut merged = MergeFundsReq::new(funds_req_queue.clone())
            .ok_or_else(|| Error::Other("MergeFunds creation".to_owned()))?;
        let amt = merged.sum();
        if amt == BigInt::zero() {
            // Note: The amount can be zero if requests are cancelled while
            // building the mergedFundsReq

            // TODO current available funds call missing
            return Ok(());
        }

        // drop read lock to allow process_task to acquire write lock on self
        // TODO check if this is necessary
        drop(funds_req_queue);

        let res = self.process_task(merged.from()?, merged.to()?, amt).await;

        // If the task is waiting on an external event (eg something to appear on
        // chain) it will return
        if res.is_none() {
            // Stop processing the fundsReqQueue and wait. When the event occurs it will
            // call process_queue() again
            return Ok(());
        }

        let mut queue = self.funds_req_queue.write().await;
        queue.clear();

        merged.on_complete(res.unwrap()).await;
        Ok(())
    }

    /// Remove all inactive fund requests from self
    async fn filter_queue(&self) {
        let mut queue = self.funds_req_queue.write().await;
        // Remove cancelled requests
        queue.retain(|val| val.active);
    }

    /// Checks the state of the channel and takes appropriate action
    /// (see description of getPaych).
    /// Note that process_task may be called repeatedly in the same state, and should
    /// return none if there is no state change to be made (eg when waiting for a
    /// message to be confirmed on chain)
    async fn process_task(&self, from: Address, to: Address, amt: BigInt) -> Option<PaychFundsRes> {
        // Get the payment channel for the from/to addresses.
        // Note: It's ok if we get ErrChannelNotTracked. It just means we need to
        // create a channel.
        let store = self.store.write().await;
        let channel_info_res = store.outbound_active_by_from_to(from, to).await;
        if channel_info_res.is_err() {
            let err = channel_info_res.err().unwrap();
            if err == Error::ChannelNotTracked {
                return Some(PaychFundsRes {
                    channel: None,
                    mcid: None,
                    err: Some(err),
                });
            }

            // If a channel has not yet been created, create one.
            let mcid = self.create_paych(from, to, amt).await;
            if mcid.is_err() {
                let err = mcid.err().unwrap();
                return Some(PaychFundsRes {
                    channel: None,
                    mcid: None,
                    err: Some(err),
                });
            }
            return Some(PaychFundsRes {
                channel: None,
                mcid: Some(mcid.ok()?),
                err: None,
            });
        }

        // If the create channel message has been sent but the channel hasn't
        // been created on chain yet
        let mut channel_info = channel_info_res.ok()?;
        if channel_info.create_msg.is_some() {
            // Wait for the channel to be created before trying again
            return None;
        }

        // If add funds message was sent to the chain but hasn't been confirmed to cover the
        // amount for the request
        if channel_info.add_funds_msg != None {
            // Wait for the add funds message to be confirmed before trying again
            return None;
        }

        // We need to add more funds, so send an add funds message to
        // cover the amount for this request
        let mcid = self.add_funds(&mut channel_info, amt).await.ok()?;

        Some(PaychFundsRes {
            channel: channel_info.channel,
            mcid: Some(mcid),
            err: None,
        })
    }

    /// Sends a message to create the channel and returns the message cid
    async fn create_paych(&self, from: Address, to: Address, amt: BigInt) -> Result<Cid, Error> {
        let params: ConstructorParams = ConstructorParams { from, to };
        let serialized =
            Serialized::serialize(params).map_err(|err| Error::Other(err.to_string()))?;
        let exec: ExecParams = ExecParams {
            code_cid: Default::default(),
            constructor_params: serialized,
        };
        let param = Serialized::serialize(exec).map_err(|err| Error::Other(err.to_string()))?;
        let umsg: UnsignedMessage = UnsignedMessage::builder()
            .from(from)
            .to(to)
            .value(amt.clone())
            .params(param)
            .build()
            .map_err(Error::Other)?;

        let smgs = self
            .state
            .mpool
            .mpool_unsigned_msg_push(umsg, self.state.keystore.clone())
            .await
            .map_err(|e| Error::Other(e.to_string()))?;

        let mcid = smgs.cid()?;

        // create a new channel in the store
        let mut store = self.store.write().await;
        let ci = store.create_channel(from, to, mcid.clone(), amt).await?;

        // TODO determine if this should be blocking
        task::spawn(async move || {
            self.wait_paych_create_msg(ci.id, mcid.clone()).await?;
        });

        Ok(mcid)
    }
    // TODO fix tuple matching here
    pub async fn wait_paych_create_msg(&self, ch_id: String, mcid: Cid) -> Result<(), Error> {
        let sm = self.state.sa.sm.read().await;

        let (ts, msg) = StateManager::wait_for_message(
            sm.get_block_store(),
            sm.get_subscriber(),
            &mcid,
            MESSAGE_CONFIDENCE,
        )
        .await
        .map_err(|e| Error::Other(e.to_string()))?;

        let _t = ts.ok_or_else(|| "its none".to_string()).unwrap(); // TODO fix
        let m = msg.ok_or_else(|| "its none".to_string()).unwrap(); // TODO fix

        let mut store = self.store.write().await;
        if m.exit_code != ExitCode::Ok {
            // channel creation failed, remove the channel from the datastore
            let _d = store
                .remove_channel(ch_id.clone())
                .await
                .map_err(|e| Error::Other(format!("failed to remove channel {}", e.to_string())))?;
        }

        let exec_ret: ExecReturn = Serialized::deserialize(&m.return_data).unwrap(); // TODO handle err

        // store robust address of channel
        let mut ch_info = store.by_channel_id(&ch_id).await?;
        ch_info.channel = Some(exec_ret.robust_address);
        ch_info.amount = ch_info.pending_amount;
        ch_info.pending_amount = BigInt::zero();
        ch_info.create_msg = None;

        store.put_channel_info(ch_info).await?;

        Ok(())
    }

    async fn add_funds(&self, ci: &mut ChannelInfo, amt: BigInt) -> Result<Cid, Error> {
        let to = ci
            .channel
            .clone()
            .ok_or_else(|| Error::Other("no address found".to_owned()))?;
        let from = ci.control;
        let umsg: UnsignedMessage = UnsignedMessage::builder()
            .to(to)
            .from(from)
            .value(amt.clone())
            .method_num(0)
            .build()
            .map_err(Error::Other)?;

        let smgs = self
            .state
            .mpool
            .mpool_unsigned_msg_push(umsg, self.state.keystore.clone())
            .await
            .map_err(|e| Error::Other(e.to_string()))?;

        let mcid = smgs.cid()?;

        let mut store = self.store.write().await;

        ci.pending_amount = amt;
        ci.add_funds_msg = Some(mcid.clone());

        let res = store.put_channel_info(ci.clone()).await;
        if res.is_err() {
            warn!("Error writing channel info to store: {}", res.unwrap_err());
        }

        let res = store.save_new_message(ci.id.clone(), mcid.clone()).await;
        if res.is_err() {
            warn!("saving add funds message cid: {}", res.unwrap_err())
        }

        // TODO ask about if this should be blocking
        task::spawn(async {
            self.wait_add_funds_msg(ci, mcid.clone()).await?;
        });

        Ok(mcid)
    }
    // TODO fix tuple matching
    pub async fn wait_add_funds_msg(
        &self,
        channel_info: &mut ChannelInfo,
        mcid: Cid,
    ) -> Result<(), Error> {
        let sm = self.state.sa.sm.read().await;

        let (ts, msg) = StateManager::wait_for_message(
            sm.get_block_store(),
            sm.get_subscriber(),
            &mcid,
            MESSAGE_CONFIDENCE,
        )
        .await
        .map_err(|e| Error::Other(e.to_string()))?;

        let _t = ts.ok_or_else(|| "its none".to_string()).unwrap(); // TODO fix
        let m = msg.ok_or_else(|| "its none".to_string()).unwrap(); // TODO fix

        if m.exit_code != ExitCode::Ok {
            channel_info.pending_amount = BigInt::zero();
            channel_info.add_funds_msg = None;
            return Err(Error::Other(format!(
                "voucher channel creation failed: adding funds (exit code {:?})",
                m.exit_code
            )));
        }

        channel_info.amount += &channel_info.pending_amount;
        channel_info.pending_amount = BigInt::zero();
        channel_info.add_funds_msg = None;

        // TODO refactor to handle error return for msg wait completed
        // TODO ask about if this should be blocking
        task::spawn(async {
            self.msg_wait_completed(mcid, err: Option<Error>).await?;
        });

        Ok(())
    }

    async fn msg_wait_completed(&mut self, mcid: Cid, err: Option<Error>) -> Result<(), Error> {
        // save the message result to the store
        let mut st = self.store.write().await;
        st.save_msg_result(mcid.clone(), err.clone()).await?;

        // inform listeners that the message has completed
        // TODO handle option err
        self.msg_listeners
            .fire_msg_complete(mcid, err.unwrap())
            .await;

        // the queue may have been waiting for msg completion to proceed, process the next queue item
        let req = self.funds_req_queue.read().await;
        if req.len() > 0 {
            // TODO ask if this should be blocking
            task::spawn(async {
                self.process_queue()
                    .await
                    .map_err(|e| Error::Other(e.to_string()))?;
            });
        }

        Ok(())
    }
}
