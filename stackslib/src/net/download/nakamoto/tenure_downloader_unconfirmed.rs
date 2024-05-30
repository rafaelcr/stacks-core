// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use rand::{thread_rng, RngCore};
use stacks_common::types::chainstate::{
    BlockHeaderHash, ConsensusHash, PoxId, SortitionId, StacksBlockId,
};
use stacks_common::types::net::{PeerAddress, PeerHost};
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::to_hex;
use stacks_common::util::secp256k1::{Secp256k1PrivateKey, Secp256k1PublicKey};
use stacks_common::util::{get_epoch_time_ms, get_epoch_time_secs, log};
use wsts::curve::point::Point;

use crate::burnchains::{Burnchain, BurnchainView, PoxConstants};
use crate::chainstate::burn::db::sortdb::{
    BlockHeaderCache, SortitionDB, SortitionDBConn, SortitionHandleConn,
};
use crate::chainstate::burn::BlockSnapshot;
use crate::chainstate::nakamoto::{
    NakamotoBlock, NakamotoBlockHeader, NakamotoChainState, NakamotoStagingBlocksConnRef,
};
use crate::chainstate::stacks::db::StacksChainState;
use crate::chainstate::stacks::{
    Error as chainstate_error, StacksBlockHeader, TenureChangePayload,
};
use crate::core::{
    EMPTY_MICROBLOCK_PARENT_HASH, FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH,
};
use crate::net::api::gettenureinfo::RPCGetTenureInfo;
use crate::net::chat::ConversationP2P;
use crate::net::db::{LocalPeer, PeerDB};
use crate::net::download::nakamoto::{
    AvailableTenures, NakamotoTenureDownloader, NakamotoTenureDownloaderSet, TenureStartEnd,
    WantedTenure,
};
use crate::net::http::HttpRequestContents;
use crate::net::httpcore::{StacksHttpRequest, StacksHttpResponse};
use crate::net::inv::epoch2x::InvState;
use crate::net::inv::nakamoto::{NakamotoInvStateMachine, NakamotoTenureInv};
use crate::net::neighbors::rpc::NeighborRPC;
use crate::net::neighbors::NeighborComms;
use crate::net::p2p::PeerNetwork;
use crate::net::server::HttpPeer;
use crate::net::{Error as NetError, Neighbor, NeighborAddress, NeighborKey};
use crate::util_lib::db::{DBConn, Error as DBError};

/// Download states for a unconfirmed tenures.  These include the ongoing tenure, as well as the
/// last complete tenure whose tenure-end block hash has not yet been written to the burnchain (but
/// the tenure-start hash has -- it was done so in the block-commit for the ongoing tenure).
#[derive(Debug, Clone, PartialEq)]
pub enum NakamotoUnconfirmedDownloadState {
    /// Getting the tenure tip information
    GetTenureInfo,
    /// Get the tenure start block for the ongoing tenure.
    /// The inner value is tenure-start block ID of the ongoing tenure.
    GetTenureStartBlock(StacksBlockId),
    /// Receiving unconfirmed tenure blocks.
    /// The inner value is the _last_ block on the ongoing tenure.  The ongoing tenure is fetched
    /// from highest block to lowest block.
    GetUnconfirmedTenureBlocks(StacksBlockId),
    /// We have gotten all the unconfirmed blocks for this tenure, and we now have the end block
    /// for the highest complete tenure (which can now be obtained via `NakamotoTenureDownloadState`).
    Done,
}

impl fmt::Display for NakamotoUnconfirmedDownloadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Download state machine for the unconfirmed tenures.  It operates in the following steps:
///
/// 1. Get /v3/tenures/info to learn the unconfirmed chain tip
/// 2. Get the tenure-start block for the unconfirmed chain tip
/// 3. Get the unconfirmed blocks, starting with the one identified by step (1) and ending with the
///    immediate child of the one obtained in (2)
///
/// Once this state-machine finishes execution, the tenure-start block is used to construct a
/// `NakamotoTenureDownloader` state machine for the highest-confirmed tenure.
///
#[derive(Debug, Clone, PartialEq)]
pub struct NakamotoUnconfirmedTenureDownloader {
    /// state of this machine
    pub state: NakamotoUnconfirmedDownloadState,
    /// Address of who we're asking
    pub naddr: NeighborAddress,
    /// Aggregate public key of the highest confirmed tenure
    pub confirmed_aggregate_public_key: Option<Point>,
    /// Aggregate public key of the unconfirmed (ongoing) tenure
    pub unconfirmed_aggregate_public_key: Option<Point>,
    /// Block ID of this node's highest-processed block.
    /// We will not download any blocks lower than this, if it's set.
    pub highest_processed_block_id: Option<StacksBlockId>,
    /// Highest processed block height  (which may not need to be loaded)
    pub highest_processed_block_height: Option<u64>,

    /// Tenure tip info we obtained for this peer
    pub tenure_tip: Option<RPCGetTenureInfo>,
    /// Tenure start block for the ongoing tip.
    /// This is also the tenure-end block for the highest-complete tip.
    pub unconfirmed_tenure_start_block: Option<NakamotoBlock>,
    /// Unconfirmed tenure blocks obtained
    pub unconfirmed_tenure_blocks: Option<Vec<NakamotoBlock>>,
}

impl NakamotoUnconfirmedTenureDownloader {
    /// Make a new downloader which will download blocks from the tip back down to the optional
    /// `highest_processed_block_id` (so we don't re-download the same blocks over and over).
    pub fn new(naddr: NeighborAddress, highest_processed_block_id: Option<StacksBlockId>) -> Self {
        Self {
            state: NakamotoUnconfirmedDownloadState::GetTenureInfo,
            naddr,
            confirmed_aggregate_public_key: None,
            unconfirmed_aggregate_public_key: None,
            highest_processed_block_id,
            highest_processed_block_height: None,
            tenure_tip: None,
            unconfirmed_tenure_start_block: None,
            unconfirmed_tenure_blocks: None,
        }
    }

    /// What's the tenure ID of the ongoing tenure?  This is learned from /v3/tenure/info, which is
    /// checked upon receipt against the burnchain state (so we're not blindly trusting the remote
    /// node).
    pub fn unconfirmed_tenure_id(&self) -> Option<&ConsensusHash> {
        self.tenure_tip.as_ref().map(|tt| &tt.consensus_hash)
    }

    /// Set the highest-processed block.
    /// This can be performed by the downloader itself in order to inform ongoing requests for
    /// unconfirmed tenures of newly-processed blocks, so they don't re-download blocks this node
    /// has already handled.
    pub fn set_highest_processed_block(
        &mut self,
        highest_processed_block_id: StacksBlockId,
        highest_processed_block_height: u64,
    ) {
        self.highest_processed_block_id = Some(highest_processed_block_id);
        self.highest_processed_block_height = Some(highest_processed_block_height);
    }

    /// Try and accept the tenure info.  It will be validated against the sortition DB and its tip.
    ///
    /// * tenure_tip.consensus_hash
    ///     This is the consensus hash of the remote node's ongoing tenure. It may not be the
    ///     sortition tip, e.g. if the tenure spans multiple sortitions.
    /// * tenure_tip.tenure_start_block_id
    ///     This is the first block ID of the ongoing unconfirmed tenure.
    /// * tenure_tip.parent_consensus_hash
    ///     This is the consensus hash of the parent of the ongoing tenure. It's the node's highest
    ///     complete tenure, for which we know the start and end block IDs.
    /// * tenure_tip.parent_tenure_start_block_id
    ///     This is the tenure start block for the highest complete tenure.  It should be equal to
    ///     the winning Stacks block hash of the snapshot for the ongoing tenure.
    ///  
    /// We may already have the tenure-start block for the unconfirmed tenure. If so, then don't go
    /// fetch it again; just get the new unconfirmed blocks.
    pub fn try_accept_tenure_info(
        &mut self,
        sortdb: &SortitionDB,
        local_sort_tip: &BlockSnapshot,
        chainstate: &StacksChainState,
        remote_tenure_tip: RPCGetTenureInfo,
        agg_pubkeys: &BTreeMap<u64, Option<Point>>,
    ) -> Result<(), NetError> {
        if self.state != NakamotoUnconfirmedDownloadState::GetTenureInfo {
            return Err(NetError::InvalidState);
        }
        if self.tenure_tip.is_some() {
            return Err(NetError::InvalidState);
        }

        // authenticate consensus hashes against canonical chain history
        let local_tenure_sn = SortitionDB::get_block_snapshot_consensus(
            sortdb.conn(),
            &remote_tenure_tip.consensus_hash,
        )?
        .ok_or(NetError::DBError(DBError::NotFoundError))?;
        let parent_local_tenure_sn = SortitionDB::get_block_snapshot_consensus(
            sortdb.conn(),
            &remote_tenure_tip.parent_consensus_hash,
        )?
        .ok_or(NetError::DBError(DBError::NotFoundError))?;

        let ih = sortdb.index_handle(&local_sort_tip.sortition_id);
        let ancestor_local_tenure_sn = ih
            .get_block_snapshot_by_height(local_tenure_sn.block_height)?
            .ok_or(NetError::DBError(DBError::NotFoundError))?;

        if ancestor_local_tenure_sn.sortition_id != local_tenure_sn.sortition_id {
            // .consensus_hash is not on the canonical fork
            warn!("Unconfirmed tenure consensus hash is not canonical";
                  "peer" => %self.naddr,
                  "consensus_hash" => %remote_tenure_tip.consensus_hash);
            return Err(DBError::NotFoundError.into());
        }
        let ancestor_parent_local_tenure_sn = ih
            .get_block_snapshot_by_height(parent_local_tenure_sn.block_height)?
            .ok_or(NetError::DBError(DBError::NotFoundError.into()))?;

        if ancestor_parent_local_tenure_sn.sortition_id != parent_local_tenure_sn.sortition_id {
            // .parent_consensus_hash is not on the canonical fork
            warn!("Parent unconfirmed tenure consensus hash is not canonical";
                  "peer" => %self.naddr,
                  "consensus_hash" => %remote_tenure_tip.parent_consensus_hash);
            return Err(DBError::NotFoundError.into());
        }

        // parent tenure sortition must precede the ongoing tenure sortition
        if local_tenure_sn.block_height <= parent_local_tenure_sn.block_height {
            warn!("Parent tenure snapshot is not an ancestor of the current tenure snapshot";
                  "peer" => %self.naddr,
                  "consensus_hash" => %remote_tenure_tip.consensus_hash,
                  "parent_consensus_hash" => %remote_tenure_tip.parent_consensus_hash);
            return Err(NetError::InvalidMessage);
        }

        // parent tenure start block ID must be the winning block hash for the ongoing tenure's
        // snapshot
        if local_tenure_sn.winning_stacks_block_hash.0
            != remote_tenure_tip.parent_tenure_start_block_id.0
        {
            warn!("Ongoing tenure does not commit to highest complete tenure's start block";
                  "remote_tenure_tip.tenure_start_block_id" => %remote_tenure_tip.tenure_start_block_id,
                  "local_tenure_sn.winning_stacks_block_hash" => %local_tenure_sn.winning_stacks_block_hash);
            return Err(NetError::InvalidMessage);
        }

        if let Some(highest_processed_block_id) = self.highest_processed_block_id.as_ref() {
            // we've synchronize this tenure before, so don't get anymore blocks before it.
            let highest_processed_block = chainstate
                .nakamoto_blocks_db()
                .get_nakamoto_block(highest_processed_block_id)?
                .ok_or(NetError::DBError(DBError::NotFoundError))?
                .0;

            let highest_processed_block_height = highest_processed_block.header.chain_length;
            self.highest_processed_block_height = Some(highest_processed_block_height);

            if &remote_tenure_tip.tip_block_id == highest_processed_block_id
                || highest_processed_block_height > remote_tenure_tip.tip_height
            {
                // nothing to do -- we're at or ahead of the remote peer, so finish up.
                // If we don't have the tenure-start block for the confirmed tenure that the remote
                // peer claims to have, then the remote peer has sent us invalid data and we should
                // treat it as such.
                let unconfirmed_tenure_start_block = chainstate
                    .nakamoto_blocks_db()
                    .get_nakamoto_block(&remote_tenure_tip.tenure_start_block_id)?
                    .ok_or(NetError::InvalidMessage)?
                    .0;
                self.unconfirmed_tenure_start_block = Some(unconfirmed_tenure_start_block);
                self.state = NakamotoUnconfirmedDownloadState::Done;
            }
        }

        if self.state == NakamotoUnconfirmedDownloadState::Done {
            // only need to remember the tenure tip
            self.tenure_tip = Some(remote_tenure_tip);
            return Ok(());
        }

        // we're not finished
        let tenure_rc = sortdb
            .pox_constants
            .block_height_to_reward_cycle(sortdb.first_block_height, local_tenure_sn.block_height)
            .expect("FATAL: sortition from before system start");
        let parent_tenure_rc = sortdb
            .pox_constants
            .block_height_to_reward_cycle(
                sortdb.first_block_height,
                parent_local_tenure_sn.block_height,
            )
            .expect("FATAL: sortition from before system start");

        // get aggregate public keys for the unconfirmed tenure and highest-complete tenure sortitions
        let Some(Some(confirmed_aggregate_public_key)) =
            agg_pubkeys.get(&parent_tenure_rc).cloned()
        else {
            warn!(
                "No aggregate public key for confirmed tenure {} (rc {})",
                &parent_local_tenure_sn.consensus_hash, parent_tenure_rc
            );
            return Err(NetError::InvalidState);
        };

        let Some(Some(unconfirmed_aggregate_public_key)) = agg_pubkeys.get(&tenure_rc).cloned()
        else {
            warn!(
                "No aggregate public key for unconfirmed tenure {} (rc {})",
                &local_tenure_sn.consensus_hash, tenure_rc
            );
            return Err(NetError::InvalidState);
        };

        if chainstate
            .nakamoto_blocks_db()
            .has_nakamoto_block(&remote_tenure_tip.tenure_start_block_id.clone())?
        {
            // proceed to get unconfirmed blocks. We already have the tenure-start block.
            let unconfirmed_tenure_start_block = chainstate
                .nakamoto_blocks_db()
                .get_nakamoto_block(&remote_tenure_tip.tenure_start_block_id)?
                .ok_or(NetError::DBError(DBError::NotFoundError))?
                .0;
            self.unconfirmed_tenure_start_block = Some(unconfirmed_tenure_start_block);
            self.state = NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(
                remote_tenure_tip.tip_block_id.clone(),
            );
        } else {
            // get the tenure-start block first
            self.state = NakamotoUnconfirmedDownloadState::GetTenureStartBlock(
                remote_tenure_tip.tenure_start_block_id.clone(),
            );
        }

        test_debug!(
            "Will validate unconfirmed blocks with ({},{}) and ({},{})",
            &confirmed_aggregate_public_key,
            parent_tenure_rc,
            &unconfirmed_aggregate_public_key,
            tenure_rc
        );
        self.confirmed_aggregate_public_key = Some(confirmed_aggregate_public_key);
        self.unconfirmed_aggregate_public_key = Some(unconfirmed_aggregate_public_key);
        self.tenure_tip = Some(remote_tenure_tip);

        Ok(())
    }

    /// Validate and accept the unconfirmed tenure-start block.  If accepted, then advance the state.
    /// Returns Ok(()) if the unconfirmed tenure start block was valid
    /// Returns Err(..) if it was not valid, or if this function was called out of sequence.
    pub fn try_accept_unconfirmed_tenure_start_block(
        &mut self,
        unconfirmed_tenure_start_block: NakamotoBlock,
    ) -> Result<(), NetError> {
        let NakamotoUnconfirmedDownloadState::GetTenureStartBlock(tenure_start_block_id) =
            &self.state
        else {
            warn!("Invalid state for this method";
                  "state" => %self.state);
            return Err(NetError::InvalidState);
        };
        let Some(tenure_tip) = self.tenure_tip.as_ref() else {
            return Err(NetError::InvalidState);
        };
        let Some(unconfirmed_aggregate_public_key) = self.unconfirmed_aggregate_public_key.as_ref()
        else {
            return Err(NetError::InvalidState);
        };

        // stacker signature has to match the current aggregate public key
        if !unconfirmed_tenure_start_block
            .header
            .verify_signer(unconfirmed_aggregate_public_key)
        {
            warn!("Invalid tenure-start block: bad signer signature";
                  "tenure_start_block.header.consensus_hash" => %unconfirmed_tenure_start_block.header.consensus_hash,
                  "tenure_start_block.header.block_id" => %unconfirmed_tenure_start_block.header.block_id(),
                  "unconfirmed_aggregate_public_key" => %unconfirmed_aggregate_public_key,
                  "state" => %self.state);
            return Err(NetError::InvalidMessage);
        }

        // block has to match the expected hash
        if tenure_start_block_id != &unconfirmed_tenure_start_block.header.block_id() {
            warn!("Invalid tenure-start block"; 
                  "tenure_id_start_block" => %tenure_start_block_id,
                  "unconfirmed_tenure_start_block.header.consensus_hash" => %unconfirmed_tenure_start_block.header.consensus_hash,
                  "unconfirmed_tenure_start_block ID" => %unconfirmed_tenure_start_block.header.block_id(),
                  "state" => %self.state);
            return Err(NetError::InvalidMessage);
        }

        // furthermore, the block has to match the expected tenure ID
        if unconfirmed_tenure_start_block.header.consensus_hash != tenure_tip.consensus_hash {
            warn!("Invalid tenure-start block or tenure-tip: consensus hash mismatch";
                  "tenure_start_block.header.consensus_hash" => %unconfirmed_tenure_start_block.header.consensus_hash,
                  "tenure_tip.consensus_hash" => %tenure_tip.consensus_hash);
            return Err(NetError::InvalidMessage);
        }

        self.unconfirmed_tenure_start_block = Some(unconfirmed_tenure_start_block);
        self.state = NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(
            tenure_tip.tip_block_id.clone(),
        );
        Ok(())
    }

    /// Add downloaded unconfirmed tenure blocks.
    /// If we have collected all tenure blocks, then return them.
    /// Returns Ok(Some(list-of-blocks)) on success, in which case, `list-of-blocks` is the
    /// height-ordered sequence of blocks in this tenure, and includes only the blocks that come
    /// after the highest-processed block (if set).
    /// Returns Ok(None) if there are still blocks to fetch, in which case, the caller should call
    /// `send_next_download_request()`
    /// Returns Err(..) on invalid state or invalid block.
    pub fn try_accept_unconfirmed_tenure_blocks(
        &mut self,
        mut tenure_blocks: Vec<NakamotoBlock>,
    ) -> Result<Option<Vec<NakamotoBlock>>, NetError> {
        let NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(last_block_id) =
            &self.state
        else {
            return Err(NetError::InvalidState);
        };

        let Some(tenure_tip) = self.tenure_tip.as_ref() else {
            return Err(NetError::InvalidState);
        };
        let Some(unconfirmed_aggregate_public_key) = self.unconfirmed_aggregate_public_key.as_ref()
        else {
            return Err(NetError::InvalidState);
        };

        if tenure_blocks.is_empty() {
            // nothing to do
            return Ok(None);
        }

        // blocks must be contiguous and in order from highest to lowest.
        // If there's a tenure-start block, it must be last.
        let mut expected_block_id = last_block_id;
        let mut finished_download = false;
        let mut last_block_index = None;
        for (cnt, block) in tenure_blocks.iter().enumerate() {
            if &block.header.block_id() != expected_block_id {
                warn!("Unexpected Nakamoto block -- not part of tenure";
                      "expected_block_id" => %expected_block_id,
                      "block_id" => %block.header.block_id());
                return Err(NetError::InvalidMessage);
            }
            if !block.header.verify_signer(unconfirmed_aggregate_public_key) {
                warn!("Invalid block: bad signer signature";
                      "tenure_id" => %tenure_tip.consensus_hash,
                      "block.header.block_id" => %block.header.block_id(),
                      "unconfirmed_aggregate_public_key" => %unconfirmed_aggregate_public_key,
                      "state" => %self.state);
                return Err(NetError::InvalidMessage);
            }

            // we may or may not need the tenure-start block for the unconfirmed tenure.  But if we
            // do, make sure it's valid, and it's the last block we receive.
            let Ok(is_tenure_start) = block.is_wellformed_tenure_start_block() else {
                warn!("Invalid tenure-start block";
                      "tenure_id" => %tenure_tip.consensus_hash,
                      "block.header.block_id" => %block.header.block_id(),
                      "state" => %self.state);
                return Err(NetError::InvalidMessage);
            };
            if is_tenure_start {
                // this is the tenure-start block, so make sure it matches our /v3/tenure/info
                if block.header.block_id() != tenure_tip.tenure_start_block_id {
                    warn!("Unexpected tenure-start block";
                          "tenure_id" => %tenure_tip.consensus_hash,
                          "block.header.block_id" => %block.header.block_id(),
                          "tenure_tip.tenure_start_block_id" => %tenure_tip.tenure_start_block_id);
                    return Err(NetError::InvalidMessage);
                }

                if cnt.saturating_add(1) != tenure_blocks.len() {
                    warn!("Invalid tenure stream -- got tenure-start before end of tenure";
                          "tenure_id" => %tenure_tip.consensus_hash,
                          "block.header.block_id" => %block.header.block_id(),
                          "cnt" => cnt,
                          "len" => tenure_blocks.len(),
                          "state" => %self.state);
                    return Err(NetError::InvalidMessage);
                }

                finished_download = true;
                last_block_index = Some(cnt);
                break;
            }

            // NOTE: this field can get updated by the downloader while this state-machine is in
            // this state.
            if let Some(highest_processed_block_id) = self.highest_processed_block_id.as_ref() {
                if expected_block_id == highest_processed_block_id {
                    // got all the blocks we asked for
                    debug!("Cancelling unconfirmed tenure download to {}: have processed block up to block {} already", &self.naddr, highest_processed_block_id);
                    finished_download = true;
                    last_block_index = Some(cnt);
                    break;
                }
            }

            // NOTE: this field can get updated by the downloader while this state-machine is in
            // this state.
            if let Some(highest_processed_block_height) =
                self.highest_processed_block_height.as_ref()
            {
                if &block.header.chain_length <= highest_processed_block_height {
                    // no need to continue this download
                    debug!("Cancelling unconfirmed tenure download to {}: have processed block at height {} already", &self.naddr, highest_processed_block_height);
                    finished_download = true;
                    last_block_index = Some(cnt);
                    break;
                }
            }

            expected_block_id = &block.header.parent_block_id;
            last_block_index = Some(cnt);
        }

        // blocks after the last_block_index were not processed, so should be dropped
        if let Some(last_block_index) = last_block_index {
            tenure_blocks.truncate(last_block_index + 1);
        }

        if let Some(blocks) = self.unconfirmed_tenure_blocks.as_mut() {
            blocks.append(&mut tenure_blocks);
        } else {
            self.unconfirmed_tenure_blocks = Some(tenure_blocks);
        }

        if finished_download {
            // we have all of the unconfirmed tenure blocks that were requested.
            // only return those newer than the highest block.
            self.state = NakamotoUnconfirmedDownloadState::Done;
            let highest_processed_block_height =
                *self.highest_processed_block_height.as_ref().unwrap_or(&0);
            return Ok(self.unconfirmed_tenure_blocks.take().map(|blocks| {
                blocks
                    .into_iter()
                    .filter(|block| block.header.chain_length > highest_processed_block_height)
                    .rev()
                    .collect()
            }));
        }

        let Some(blocks) = self.unconfirmed_tenure_blocks.as_ref() else {
            // unreachable but be defensive
            warn!("Invalid state: no blocks (infallible -- got empty vec)");
            return Err(NetError::InvalidState);
        };

        // still have more to get
        let Some(earliest_block) = blocks.last() else {
            // unreachable but be defensive
            warn!("Invalid state: no blocks (infallible -- got empty vec)");
            return Err(NetError::InvalidState);
        };
        let next_block_id = earliest_block.header.parent_block_id.clone();

        self.state = NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(next_block_id);
        Ok(None)
    }

    /// Once this machine runs to completion, examine its state to see if we still need to fetch
    /// the highest complete tenure.  We may not need to, especially if we're just polling for new
    /// unconfirmed blocks.
    ///
    /// Return Ok(true) if we need it still
    /// Return Ok(false) if we already have it
    /// Return Err(..) if we encounter a DB error or if this function was called out of sequence.
    pub fn need_highest_complete_tenure(
        &self,
        chainstate: &StacksChainState,
    ) -> Result<bool, NetError> {
        if self.state != NakamotoUnconfirmedDownloadState::Done {
            return Err(NetError::InvalidState);
        }
        let Some(unconfirmed_tenure_start_block) = self.unconfirmed_tenure_start_block.as_ref()
        else {
            return Err(NetError::InvalidState);
        };

        // if we've processed the unconfirmed tenure-start block already, then we've necessarily
        // downloaded and processed the highest-complete tenure already.
        Ok(!NakamotoChainState::has_block_header(
            chainstate.db(),
            &unconfirmed_tenure_start_block.header.block_id(),
            false,
        )?)
    }

    /// Create a NakamotoTenureDownloader for the highest complete tenure.  We already have the
    /// tenure-end block (which will be supplied to the downloader), but we'll still want to go get
    /// its tenure-start block.
    ///
    /// Returns Ok(downloader) on success
    /// Returns Err(..) if we call this function out of sequence.
    pub fn make_highest_complete_tenure_downloader(
        &self,
        highest_tenure: &WantedTenure,
        unconfirmed_tenure: &WantedTenure,
    ) -> Result<NakamotoTenureDownloader, NetError> {
        if self.state != NakamotoUnconfirmedDownloadState::Done {
            return Err(NetError::InvalidState);
        }
        let Some(unconfirmed_tenure_start_block) = self.unconfirmed_tenure_start_block.as_ref()
        else {
            return Err(NetError::InvalidState);
        };
        let Some(confirmed_aggregate_public_key) = self.confirmed_aggregate_public_key.as_ref()
        else {
            return Err(NetError::InvalidState);
        };
        let Some(unconfirmed_aggregate_public_key) = self.unconfirmed_aggregate_public_key.as_ref()
        else {
            return Err(NetError::InvalidState);
        };

        test_debug!(
            "Create highest complete tenure downloader for {}",
            &highest_tenure.tenure_id_consensus_hash
        );
        let ntd = NakamotoTenureDownloader::new(
            highest_tenure.tenure_id_consensus_hash.clone(),
            unconfirmed_tenure.winning_block_id.clone(),
            unconfirmed_tenure_start_block.header.block_id(),
            self.naddr.clone(),
            confirmed_aggregate_public_key.clone(),
            unconfirmed_aggregate_public_key.clone(),
        )
        .with_tenure_end_block(unconfirmed_tenure_start_block.clone());

        Ok(ntd)
    }

    /// Produce the next HTTP request that, when successfully executed, will advance this state
    /// machine.
    ///
    /// Returns Some(request) if a request must be sent.
    /// Returns None if we're done
    pub fn make_next_download_request(&self, peerhost: PeerHost) -> Option<StacksHttpRequest> {
        match &self.state {
            NakamotoUnconfirmedDownloadState::GetTenureInfo => {
                // need to get the tenure tip
                return Some(StacksHttpRequest::new_get_nakamoto_tenure_info(peerhost));
            }
            NakamotoUnconfirmedDownloadState::GetTenureStartBlock(block_id) => {
                return Some(StacksHttpRequest::new_get_nakamoto_block(
                    peerhost,
                    block_id.clone(),
                ));
            }
            NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(tip_block_id) => {
                return Some(StacksHttpRequest::new_get_nakamoto_tenure(
                    peerhost,
                    tip_block_id.clone(),
                    self.highest_processed_block_id.clone(),
                ));
            }
            NakamotoUnconfirmedDownloadState::Done => {
                // got all unconfirmed blocks!  Next step is to turn this downloader into a confirmed
                // tenure downloader using the earliest unconfirmed tenure block.
                return None;
            }
        }
    }

    /// Begin the next download request for this state machine.
    /// Returns Ok(()) if we sent the request, or there's already an in-flight request.  The
    /// caller should try this again until it gets one of the other possible return values.  It's
    /// up to the caller to determine when it's appropriate to convert this state machine into a
    /// `NakamotoTenureDownloader`.
    /// Returns Err(..) if the neighbor is dead or broken.
    pub fn send_next_download_request(
        &self,
        network: &mut PeerNetwork,
        neighbor_rpc: &mut NeighborRPC,
    ) -> Result<(), NetError> {
        if neighbor_rpc.has_inflight(&self.naddr) {
            test_debug!("Peer {} has an inflight request", &self.naddr);
            return Ok(());
        }
        if neighbor_rpc.is_dead_or_broken(network, &self.naddr) {
            return Err(NetError::PeerNotConnected);
        }

        let Some(peerhost) = NeighborRPC::get_peer_host(network, &self.naddr) else {
            // no conversation open to this neighbor
            neighbor_rpc.add_dead(network, &self.naddr);
            return Err(NetError::PeerNotConnected);
        };

        let Some(request) = self.make_next_download_request(peerhost) else {
            // treat this downloader as still in-flight since the overall state machine will need
            // to keep it around long enough to convert it into a tenure downloader for the highest
            // complete tenure.
            return Ok(());
        };

        neighbor_rpc.send_request(network, self.naddr.clone(), request)?;
        Ok(())
    }

    /// Handle a received StacksHttpResponse and advance this machine's state
    /// If we get the full tenure, return it.
    ///
    /// Returns Ok(Some(blocks)) if we finished downloading the unconfirmed tenure
    /// Returns Ok(None) if we're still working, in which case the caller should call
    /// `send_next_download_request()`
    /// Returns Err(..) on unrecoverable failure to advance state
    pub fn handle_next_download_response(
        &mut self,
        response: StacksHttpResponse,
        sortdb: &SortitionDB,
        local_sort_tip: &BlockSnapshot,
        chainstate: &StacksChainState,
        agg_pubkeys: &BTreeMap<u64, Option<Point>>,
    ) -> Result<Option<Vec<NakamotoBlock>>, NetError> {
        match &self.state {
            NakamotoUnconfirmedDownloadState::GetTenureInfo => {
                test_debug!("Got tenure-info response");
                let remote_tenure_info = response.decode_nakamoto_tenure_info()?;
                test_debug!("Got tenure-info response: {:?}", &remote_tenure_info);
                self.try_accept_tenure_info(
                    sortdb,
                    local_sort_tip,
                    chainstate,
                    remote_tenure_info,
                    agg_pubkeys,
                )?;
                Ok(None)
            }
            NakamotoUnconfirmedDownloadState::GetTenureStartBlock(..) => {
                test_debug!("Got tenure start-block response");
                let block = response.decode_nakamoto_block()?;
                self.try_accept_unconfirmed_tenure_start_block(block)?;
                Ok(None)
            }
            NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(..) => {
                test_debug!("Got unconfirmed tenure blocks response");
                let blocks = response.decode_nakamoto_tenure()?;
                self.try_accept_unconfirmed_tenure_blocks(blocks)
            }
            NakamotoUnconfirmedDownloadState::Done => {
                return Err(NetError::InvalidState);
            }
        }
    }

    /// Is this machine finished?
    pub fn is_done(&self) -> bool {
        self.state == NakamotoUnconfirmedDownloadState::Done
    }
}
