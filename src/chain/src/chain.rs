/*
  Copyright 2018 The Purple Library Authors
  This file is part of the Purple Library.

  The Purple Library is free software: you can redistribute it and/or modify
  it under the terms of the GNU General Public License as published by
  the Free Software Foundation, either version 3 of the License, or
  (at your option) any later version.

  The Purple Library is distributed in the hope that it will be useful,
  but WITHOUT ANY WARRANTY; without even the implied warranty of
  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
  GNU General Public License for more details.

  You should have received a copy of the GNU General Public License
  along with the Purple Library. If not, see <http://www.gnu.org/licenses/>.
*/

use crate::block::Block;
use crate::orphan_type::OrphanType;
use common::checkpointable::*;
use bin_tools::*;
use crypto::Hash;
use elastic_array::ElasticArray128;
use hashbrown::{HashMap, HashSet};
use hashdb::HashDB;
use lazy_static::*;
use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use persistence::PersistentDb;
use std::collections::VecDeque;
use std::hash::Hash as HashTrait;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub enum ChainErr {
    /// The block already exists in the chain.
    AlreadyInChain,

    /// The parent of the given block is invalid
    InvalidParent,

    /// The given block does not have a parent hash
    NoParentHash,

    /// Bad block height
    BadHeight,

    /// The block with the given hash is not written in the ledger
    NoSuchBlock,

    /// The orphan pool is full.
    TooManyOrphans,

    /// The append condition returned false
    BadAppendCondition,

    /// Could not find a matching checkpoint
    NoCheckpointFound,
}

lazy_static! {
    /// Canonical tip block key
    static ref TIP_KEY: Hash = { crypto::hash_slice(b"canonical_tip") };

    /// The key to the canonical height of the chain
    static ref CANONICAL_HEIGHT_KEY: Hash = { crypto::hash_slice(b"canonical_height") };

    /// The key of the height that has the earliest checkpoint
    static ref EARLIEST_CHECKPOINT_KEY: Hash = { crypto::hash_slice(b"earliest_checkpoint") };

    /// The key of the height that has the last checkpoint
    static ref LAST_CHECKPOINT_KEY: Hash = { crypto::hash_slice(b"last_checkpoint") };
}

#[derive(Clone)]
/// Thread-safe reference to a chain and its block cache.
pub struct ChainRef<'a, B: Block<'a>> {
    /// Atomic reference to the chain.
    pub chain: Arc<RwLock<Chain<'a, B>>>,

    /// Block lookup cache.
    block_cache: Arc<Mutex<LruCache<Hash, Arc<B>>>>,
}

impl<'a, B: Block<'a>> ChainRef<'a, B> {
    pub fn new(chain: Arc<RwLock<Chain<'a, B>>>) -> ChainRef<'a, B> {
        ChainRef {
            chain,
            block_cache: Arc::new(Mutex::new(LruCache::new(B::BLOCK_CACHE_SIZE))),
        }
    }

    /// Attempts to fetch a block by its hash from the cache
    /// and if it doesn't succeed it then attempts to retrieve
    /// it from the database.
    pub fn query(&self, hash: &Hash) -> Option<Arc<B>> {
        let cache_result = {
            let mut cache = self.block_cache.lock();

            if let Some(result) = cache.get(hash) {
                Some(result.clone())
            } else {
                None
            }
        };

        if let Some(result) = cache_result {
            Some(result)
        } else {
            let chain_result = {
                let chain = self.chain.read();

                if let Some(result) = chain.query(hash) {
                    Some(result)
                } else {
                    None
                }
            };

            if let Some(result) = chain_result {
                let mut cache = self.block_cache.lock();

                if cache.get(hash).is_none() {
                    // Cache result and then return it
                    cache.put(hash.clone(), result.clone());
                }

                Some(result)
            } else {
                None
            }
        }
    }
}

#[derive(Debug)]
/// Generic chain
pub struct Chain<'a, B: Block<'a>> {
    /// Reference to the database storing the chain.
    db: PersistentDb<'a>,

    /// The current height of the chain.
    height: u64,

    /// The tip block of the canonical chain.
    canonical_tip: Arc<B>,

    /// The state associated with the canonical tip
    canonical_tip_state: B::ChainState,

    /// Memory pool of blocks that are not in the canonical chain.
    orphan_pool: HashMap<Hash, Arc<B>>,

    /// The biggest height of all orphans
    max_orphan_height: Option<u64>,

    /// Mapping between heights and their sets of
    /// orphans mapped to their inverse height.
    heights_mapping: HashMap<u64, HashMap<Hash, u64>>,

    /// Mapping between heights that have checkpoints 
    /// and the corresponding checkpoint id.
    disk_heights_checkpoints: HashMap<u64, u64>,

    /// Height of the last state that has a checkpoint
    last_checkpoint_height: Option<u64>,

    /// Earliest height to have a checkpoint
    earliest_checkpoint_height: Option<u64>,

    /// Mapping between orphans and their orphan types/validation statuses.
    validations_mapping: HashMap<Hash, OrphanType>,

    /// Mapping between disconnected chains heads and tips.
    disconnected_heads_mapping: HashMap<Hash, HashSet<Hash>>,

    /// Mapping between disconnected heads and the largest
    /// height of any associated tip along with its hash.
    disconnected_heads_heights: HashMap<Hash, (u64, Hash)>,

    /// Mapping between disconnected chains tips and heads.
    disconnected_tips_mapping: HashMap<Hash, Hash>,

    /// Set containing tips of valid chains that descend
    /// from the canonical chain.
    valid_tips: HashSet<Hash>,

    /// Validation states associated with the valid tips
    valid_tips_states: HashMap<Hash, B::ChainState>,

    /// Whether the chain is in archival mode or not
    archival_mode: bool,
}

impl<'a, B: Block<'a>> Chain<'a, B> {
    pub fn new(mut db_ref: PersistentDb<'a>, canonical_tip_state: B::ChainState, archival_mode: bool) -> Chain<'a, B> {
        let tip_db_res = db_ref.get(&TIP_KEY);
        let canonical_tip = match tip_db_res.clone() {
            Some(tip) => {
                let mut buf = [0; 32];
                buf.copy_from_slice(&tip);

                let block_bytes = db_ref.get(&Hash(buf)).unwrap();
                B::from_bytes(&block_bytes).unwrap()
            }
            None => B::genesis(),
        };

        let height = match db_ref.get(&CANONICAL_HEIGHT_KEY) {
            Some(height) => decode_be_u64!(&height).unwrap(),
            None => {
                if tip_db_res.is_none() {
                    // Set 0 height
                    db_ref.emplace(
                        CANONICAL_HEIGHT_KEY.clone(),
                        ElasticArray128::<u8>::from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]),
                    );
                }

                0
            }
        };

        let earliest_checkpoint_height = match db_ref.get(&EARLIEST_CHECKPOINT_KEY) {
            Some(height) => Some(decode_be_u64!(&height).unwrap()),
            None => None
        };

        let last_checkpoint_height = match db_ref.get(&LAST_CHECKPOINT_KEY) {
            Some(height) => Some(decode_be_u64!(&height).unwrap()),
            None => None
        };

        let height = height;

        Chain {
            canonical_tip,
            canonical_tip_state,
            orphan_pool: HashMap::with_capacity(B::MAX_ORPHANS),
            heights_mapping: HashMap::with_capacity(B::MAX_ORPHANS),
            validations_mapping: HashMap::with_capacity(B::MAX_ORPHANS),
            disconnected_heads_mapping: HashMap::with_capacity(B::MAX_ORPHANS),
            disconnected_heads_heights: HashMap::with_capacity(B::MAX_ORPHANS),
            disconnected_tips_mapping: HashMap::with_capacity(B::MAX_ORPHANS),
            valid_tips: HashSet::with_capacity(B::MAX_ORPHANS),
            valid_tips_states: HashMap::with_capacity(B::MAX_ORPHANS),
            disk_heights_checkpoints: HashMap::with_capacity(B::MAX_CHECKPOINTS),
            last_checkpoint_height,
            earliest_checkpoint_height,
            max_orphan_height: None,
            archival_mode,
            height,
            db: db_ref,
        }
    }

    /// Rewinds the canonical chain to the block with the given hash.
    ///
    /// Returns `Err(ChainErr::NoSuchBlock)` if there is no block with
    /// the given hash in the canonical chain.
    pub fn rewind<'b: 'a>(&'b mut self, block_hash: &Hash) -> Result<(), ChainErr> {
        let genesis = B::genesis();
        let new_tip = if *block_hash == genesis.block_hash().unwrap() {
            genesis
        } else if let Some(new_tip) = self.db.get(block_hash) {
            B::from_bytes(&new_tip).unwrap()
        } else {
            return Err(ChainErr::NoSuchBlock);
        };

        let canonical_state = self.canonical_tip_state.duplicate();
        let mut current = self.canonical_tip.clone();
        let mut inverse_height = 1;

        // Remove canonical tip from the chain
        // and mark it as a valid chain tip.
        self.db.remove(&current.block_hash().unwrap());

        // Remove current height entry
        let current_height_key = crypto::hash_slice(&encode_be_u64!(current.height()));
        self.db.remove(&current_height_key);

        // Add the old tip to the orphan pool
        self.orphan_pool
            .insert(current.block_hash().unwrap(), current.clone());

        // Mark old tip as a valid chain tip
        self.validations_mapping.insert(current.block_hash().unwrap(), OrphanType::ValidChainTip);
        self.valid_tips.insert(current.block_hash().unwrap());
        self.valid_tips_states.insert(current.block_hash().unwrap(), canonical_state);

        let cur_height = current.height();

        // Insert to heights mapping
        if let Some(entries) = self.heights_mapping.get_mut(&cur_height) {
            entries.insert(current.block_hash().unwrap(), 0);
        } else {
            let mut hm = HashMap::new();
            hm.insert(current.block_hash().unwrap(), 0);
            self.heights_mapping.insert(cur_height, hm);
        }

        // Try to update the maximum orphan height with
        // the previous canonical tip's height.
        self.update_max_orphan_height(current.height());

        // Traverse parents and remove them until we
        // reach the block with the given hash.
        loop {
            let parent_hash = current.parent_hash().unwrap();

            if parent_hash == *block_hash {
                break;
            } else {
                // Remove past checkpoints when we reach their height.
                //
                // TODO: Maybe we should soft-delete checkpoints
                // instead on each rewind in case we switch often.
                if let (Some(last_checkpoint_height), Some(earliest_checkpoint_height)) = (self.last_checkpoint_height, self.earliest_checkpoint_height) {
                    if current.height() - 1 == last_checkpoint_height {
                        let checkpoint_id = self.disk_heights_checkpoints.remove(&last_checkpoint_height).unwrap();
                        B::ChainState::delete_checkpoint(checkpoint_id).unwrap();
                    }

                    // Remove checkpoint entries if we only have one checkpoint
                    if last_checkpoint_height == earliest_checkpoint_height {
                        self.earliest_checkpoint_height = None;
                        self.last_checkpoint_height = None;
                    }
                }

                let parent = B::from_bytes(&self.db.get(&parent_hash).unwrap()).unwrap();
                let cur_height = parent.height();

                // Remove parent from db
                self.db.remove(&parent_hash);

                // Remove current height entry
                let current_height_key = crypto::hash_slice(&encode_be_u64!(cur_height));
                self.db.remove(&current_height_key);

                // Add the parent to the orphan pool
                self.orphan_pool
                    .insert(parent.block_hash().unwrap(), parent.clone());

                // Mark parent as belonging to a valid chain
                self.validations_mapping.insert(
                    parent.block_hash().unwrap(),
                    OrphanType::BelongsToValidChain,
                );

                // Insert to heights mapping
                if let Some(entries) = self.heights_mapping.get_mut(&cur_height) {
                    entries.insert(parent.block_hash().unwrap(), inverse_height);
                } else {
                    let mut hm = HashMap::new();
                    hm.insert(parent.block_hash().unwrap(), inverse_height);
                    self.heights_mapping.insert(cur_height, hm);
                }

                // Update max orphan height
                self.update_max_orphan_height(parent.height());

                current = parent;
                inverse_height += 1;
            }
        }

        self.height = new_tip.height();
        self.write_canonical_height(new_tip.height());
        self.canonical_tip_state = self.search_fetch_next_state(new_tip.height());
        self.canonical_tip = new_tip;
        
        // Flush changes
        self.db.flush();

        Ok(())
    }

    fn update_max_orphan_height(&mut self, new_height: u64) {
        if self.max_orphan_height.is_none() {
            self.max_orphan_height = Some(new_height);
        } else {
            let cur_height = self.max_orphan_height.unwrap();

            if new_height > cur_height {
                self.max_orphan_height = Some(new_height);
            }
        }
    }

    fn write_block(&mut self, block: Arc<B>) {
        let block_hash = block.block_hash().unwrap();
        //println!("DEBUG WRITING BLOCK: {:?}", block_hash);
        assert!(self.disconnected_heads_mapping.get(&block_hash).is_none());
        assert!(self.disconnected_tips_mapping.get(&block_hash).is_none());

        // We can only write a block whose parent
        // hash is the hash of the current canonical
        // tip block.
        assert_eq!(
            block.parent_hash().unwrap(),
            self.canonical_tip.block_hash().unwrap()
        );

        // Place block in the ledger
        self.db.emplace(
            block_hash.clone(),
            ElasticArray128::<u8>::from_slice(&block.to_bytes()),
        );

        // Set new tip block
        self.canonical_tip = block.clone();
        let mut height = decode_be_u64!(self.db.get(&CANONICAL_HEIGHT_KEY).unwrap()).unwrap();

        // Increment height
        height += 1;

        // Set new height
        self.height = height;

        let encoded_height = encode_be_u64!(height);

        // Write new height
        self.write_canonical_height(height);

        // Write block height
        let block_height_key = Self::compute_height_key(&block_hash);

        self.db.emplace(
            block_height_key,
            ElasticArray128::<u8>::from_slice(&encoded_height),
        );

        // Write height mapping
        self.db.emplace(
            crypto::hash_slice(&encoded_height),
            ElasticArray128::<u8>::from_slice(&block_hash.0)
        );

        self.orphan_pool.remove(&block_hash);
        self.validations_mapping.remove(&block_hash);

        // Remove from height mappings
        if let Some(orphans) = self.heights_mapping.get_mut(&block.height()) {
            orphans.remove(&block_hash);
        }

        // Remove from valid tips
        self.valid_tips.remove(&block_hash);
        self.valid_tips_states.remove(&block_hash);

        // Update max orphan height if this is the case
        if let Some(max_height) = self.max_orphan_height {
            if block.height() == max_height {
                // Traverse heights backwards until we have
                // an entry. We then set that as the new max orphan height.
                let mut current = max_height - 1;

                loop {
                    if current == 0 {
                        self.max_orphan_height = None;
                        break;
                    }

                    if self.heights_mapping.get(&current).is_some() {
                        self.max_orphan_height = Some(current);
                        break;
                    }

                    current -= 1;
                }
            }
        }

        self.db.flush();

        // Execute after write callback
        if let Some(mut cb) = B::after_write() {
            cb(block);
        }
    }

    fn write_canonical_height(&mut self, height: u64) {
        let encoded_height = encode_be_u64!(height);
        self.db.emplace(
            CANONICAL_HEIGHT_KEY.clone(),
            ElasticArray128::<u8>::from_slice(&encoded_height),
        );
    }

    fn write_orphan(&mut self, orphan: Arc<B>, orphan_type: OrphanType, inverse_height: u64) {
        let orphan_hash = orphan.block_hash().unwrap();
        let height = orphan.height();

        match orphan_type {
            OrphanType::ValidChainTip => {
                self.valid_tips.insert(orphan.block_hash().unwrap());
            }
            _ => {
                // Do nothing
            }
        }

        // Write height mapping
        if let Some(height_entry) = self.heights_mapping.get_mut(&height) {
            if height_entry.get(&orphan_hash).is_none() {
                height_entry.insert(orphan_hash.clone(), inverse_height);
            }
        } else {
            let mut map = HashMap::new();
            map.insert(orphan_hash.clone(), inverse_height);

            self.heights_mapping.insert(height, map);
        }

        // Write to orphan pool
        self.orphan_pool.insert(orphan_hash.clone(), orphan.clone());

        // Set max orphan height if this is the case
        self.update_max_orphan_height(height);

        // Write to validations mappings
        self.validations_mapping.insert(orphan_hash, orphan_type);
    }

    fn compute_height_key(hash: &Hash) -> Hash {
        let block_height_key = format!("{}.height", hex::encode(hash.to_vec()));
        crypto::hash_slice(block_height_key.as_bytes())
    }

    /// Attempts to attach orphans to the canonical chain
    /// starting with the given height.
    fn process_orphans(&'a mut self, start_height: u64) {
        if let Some(max_orphan_height) = self.max_orphan_height {
            let mut h = start_height;
            let mut done = false;
            let mut prev_valid_tips = HashSet::new();

            loop {
                if h > max_orphan_height {
                    break;
                }

                if let Some(orphans) = self.heights_mapping.get(&h) {
                    if orphans.len() == 1 {
                        // HACK: Maybe we can find a better/faster way to get the only item of a set?
                        let (orphan_hash, _) = orphans.iter().find(|_| true).unwrap();
                        let orphan = self.orphan_pool.get(orphan_hash).unwrap().clone();
                        let block_hash = orphan.block_hash().unwrap();

                        // If the orphan directly follows the canonical
                        // tip, write it to the chain.
                        if orphan.parent_hash().unwrap() == self.canonical_tip.block_hash().unwrap() 
                        {
                            // Verify append condition
                            let append_condition = match B::append_condition(orphan.clone(), self.canonical_tip_state.duplicate()) {
                                // Set new tip state if the append can proceed
                                Ok(new_tip_state) => Some(new_tip_state),
                                _ => None
                            };

                            if !done {
                                // Verify append condition
                                if let Some(new_tip_state) = append_condition {
                                    self.make_valid_tips(&block_hash, new_tip_state.duplicate());
                                    self.write_block(orphan.clone());

                                    // Perform checkpoint
                                    {
                                        let height = self.height;
                                        let last_checkpoint_height = self.last_checkpoint_height.unwrap_or(0);

                                        // Checkpoint state if we have reached the quota
                                        if height - last_checkpoint_height == B::CHECKPOINT_INTERVAL as u64 {
                                            if let None = self.earliest_checkpoint_height {
                                                self.earliest_checkpoint_height = Some(height);
                                            }

                                            self.last_checkpoint_height = Some(height);
                                            let checkpoint_id = new_tip_state.checkpoint();

                                            // Store checkpoint id
                                            self.disk_heights_checkpoints.insert(height, checkpoint_id);
                                        }
                                    }

                                    self.canonical_tip_state = new_tip_state;
                                } else {
                                    done = true;
                                }
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    } else if orphans.is_empty() {
                        if prev_valid_tips.is_empty() {
                            break;
                        } else {
                            // Mark processing as done but continue so we can
                            // update the current valid chains.
                            if !done {
                                done = true;
                            } else {
                                break;
                            }
                        }
                    } else {
                        let mut new_prev_valid_tips = prev_valid_tips.clone();
                        let mut obsolete = HashSet::new();
                        let mut buf: Vec<(Hash, u64, B::ChainState)> = Vec::with_capacity(orphans.len());

                        for (o, i_h) in orphans.iter() {
                            // Filter out orphans that do not follow
                            // the canonical tip.
                            let orphan = self.orphan_pool.get(o).unwrap();
                            let orphan_parent = orphan.parent_hash().unwrap();
                            let canonical_tip = self.canonical_tip.block_hash().unwrap();

                            if orphan_parent == canonical_tip {
                                let new_state = {
                                    match B::append_condition(orphan.clone(), self.canonical_tip_state.duplicate()) {
                                        // Set new tip state if the append can proceed
                                        Ok(new_tip_state) => Some(new_tip_state),
                                        _ => None
                                    }
                                };

                                if let Some(new_state) = new_state {
                                    buf.push((o.clone(), i_h.clone(), new_state));
                                } else {
                                    // TODO: Maybe cleanup here? Issue #109
                                }
                            } else if prev_valid_tips.contains(&orphan_parent) {
                                let append_condition = {
                                    let parent_state = self.valid_tips_states.get(&orphan_parent).unwrap();

                                    match B::append_condition(orphan.clone(), parent_state.duplicate()) {
                                        // Set new tip state if the append can proceed
                                        Ok(new_tip_state) => Some(new_tip_state),
                                        _ => None
                                    }
                                };

                                if let Some(new_tip_state) = append_condition {
                                    // Mark old tip as belonging to valid chain
                                    let parent_status =
                                        self.validations_mapping.get_mut(&orphan_parent).unwrap();
                                    *parent_status = OrphanType::BelongsToValidChain;

                                    // Mark new tip
                                    let status = self.validations_mapping.get_mut(&o).unwrap();
                                    *status = OrphanType::ValidChainTip;

                                    obsolete.insert(orphan_parent.clone());
                                    self.valid_tips_states.insert(o.clone(), new_tip_state);

                                    // Add to valid tips sets
                                    self.valid_tips.insert(o.clone());
                                    new_prev_valid_tips.remove(&orphan_parent);
                                    new_prev_valid_tips.insert(o.clone());
                                } else {
                                    // TODO: Maybe cleanup here? Issue #109
                                }
                            } else {
                            }
                        }

                        // Remove obsolete tips
                        for b in obsolete.iter() {
                            self.valid_tips.remove(&b);
                            self.valid_tips_states.remove(&b);
                        }

                        prev_valid_tips = new_prev_valid_tips;

                        if buf.is_empty() {
                            if prev_valid_tips.is_empty() {
                                break;
                            } else {
                                // Mark processing as done but continue so we can
                                // update tips information.
                                if !done {
                                    done = true;
                                    continue;
                                } else {
                                    break;
                                }
                            }
                        }

                        // Write the orphan with the greatest inverse height
                        buf.sort_unstable_by(|(_, a, _), (_, b, _)| a.cmp(&b));

                        if !done {
                            if let Some((to_write, _, state)) = buf.pop() {
                                let to_write = self.orphan_pool.get(&to_write).unwrap().clone();
                                let block_hash = to_write.block_hash().unwrap();
                                let height = to_write.height();
                                let last_checkpoint_height = self.last_checkpoint_height.unwrap_or(0);

                                // Checkpoint state if we have reached the quota
                                if height - last_checkpoint_height == B::CHECKPOINT_INTERVAL as u64 {
                                    if let None = self.earliest_checkpoint_height {
                                        self.earliest_checkpoint_height = Some(height);
                                    }

                                    self.last_checkpoint_height = Some(height);
                                    let checkpoint_id = state.checkpoint();

                                    // Store checkpoint id
                                    self.disk_heights_checkpoints.insert(height, checkpoint_id);
                                    self.last_checkpoint_height = Some(height);
                                }

                                self.make_valid_tips(&block_hash, state.duplicate());
                                self.canonical_tip_state = state;
                                self.write_block(to_write.clone());
                            }
                        }

                        // Place remaining tips in valid tips set
                        // and mark them as valid chain tips.
                        for (o, _, state) in buf {
                            let status = self.validations_mapping.get_mut(&o).unwrap();
                            *status = OrphanType::ValidChainTip;
                            prev_valid_tips.insert(o);
                            self.valid_tips.insert(o.clone());
                            self.valid_tips_states.insert(o.clone(), state);
                        }
                    }
                } 


                h += 1;
            }
        }
    }

    /// Attempts to switch the canonical chain to the valid chain
    /// which has the given canidate tip. Do nothing if this is not
    /// possible.
    fn attempt_switch(&'a mut self, candidate_tip: Arc<B>) {
        let candidate_hash = candidate_tip.block_hash().unwrap();
        assert!(self.valid_tips.contains(&candidate_hash));
        assert!(self.disconnected_heads_mapping.get(&candidate_hash).is_none());
        assert!(self.disconnected_tips_mapping.get(&candidate_hash).is_none());

        if candidate_tip.height() > self.height + B::SWITCH_OFFSET as u64 {
            let mut to_write: VecDeque<Arc<B>> = VecDeque::new();
            to_write.push_front(candidate_tip.clone());

            // Find the horizon block i.e. the common
            // ancestor of both the candidate tip and
            // the canonical tip.
            let horizon = {
                let mut current = candidate_tip.parent_hash().unwrap();

                // Traverse parents until we find a canonical block
                loop {
                    if self.db.get(&current).is_some() {
                        break;
                    }

                    let cur = self.orphan_pool.get(&current).unwrap();
                    to_write.push_front(cur.clone());

                    current = cur.parent_hash().unwrap();
                }

                current
            };

            // Rewind to horizon
            self.rewind(&horizon).unwrap();

            // Set the canonical tip state as the one belonging to the new tip
            self.canonical_tip_state = self.valid_tips_states.remove(&candidate_hash).unwrap();

            // Write the blocks from the candidate chain
            for block in to_write {
                let block_hash = block.block_hash().unwrap();
                // Don't write the horizon
                if block_hash == horizon {
                    continue;
                }

                self.disconnected_heads_mapping.remove(&block_hash);
                self.disconnected_tips_mapping.remove(&block_hash);
                self.disconnected_heads_heights.remove(&block_hash);

                self.write_block(block);
            }
        }
    }

    /// Attempts to attach a disconnected chain tip to other
    /// disconnected chains. Returns the final status of the tip.
    fn attempt_attach(&mut self, tip_hash: &Hash, initial_status: OrphanType) -> OrphanType {
        let mut status = initial_status;
        let mut to_attach = Vec::with_capacity(B::MAX_ORPHANS);
        let our_head_hash = self.disconnected_tips_mapping.get(tip_hash).unwrap();

        // Find a matching disconnected chain head
        for (head_hash, _) in self.disconnected_heads_mapping.iter() {
            // Skip our tip
            if head_hash == our_head_hash || head_hash == tip_hash {
                continue;
            }

            let head = self.orphan_pool.get(head_hash).unwrap();

            // Attach chain to our tip
            if head.parent_hash().unwrap() == *tip_hash {
                to_attach.push(head_hash.clone());
                status = OrphanType::BelongsToDisconnected;
            }
        }

        let cur_head = self
            .disconnected_tips_mapping
            .get(tip_hash)
            .unwrap()
            .clone();

        // Attach heads
        for head in to_attach.iter() {
            let tips = self.disconnected_heads_mapping.remove(head).unwrap();
            self.disconnected_heads_heights.remove(head).unwrap();

            let cur_tips =
                if let Some(cur_tips) = self.disconnected_heads_mapping.get_mut(&cur_head) {
                    cur_tips
                } else {
                    self.disconnected_heads_mapping
                        .insert(cur_head.clone(), HashSet::new());
                    self.disconnected_heads_mapping.get_mut(&cur_head).unwrap()
                };

            let mut to_traverse = Vec::with_capacity(tips.len());

            // Clear our the head from tips set if it exists
            cur_tips.remove(&cur_head);
            self.disconnected_tips_mapping.remove(&cur_head);

            // Merge tips
            for tip_hash in tips.iter() {
                let tip = self.orphan_pool.get(tip_hash).unwrap();
                let (largest_height, _) = self.disconnected_heads_heights.get(&cur_head).unwrap();

                if let Some(head_mapping) = self.disconnected_tips_mapping.get_mut(tip_hash) {
                    *head_mapping = cur_head.clone();
                } else {
                    self.disconnected_tips_mapping
                        .insert(tip_hash.clone(), cur_head.clone());
                }

                // Update heights entry if new tip height is larger
                if tip.height() > *largest_height {
                    self.disconnected_heads_heights
                        .insert(cur_head.clone(), (tip.height(), tip.block_hash().unwrap()));
                }

                to_traverse.push(tip.clone());
                cur_tips.insert(tip_hash.clone());
            }

            // Update inverse heights starting from pushed tips
            for tip in to_traverse {
                self.traverse_inverse(tip, 0, false);
            }
        }

        status
    }

    /// Attempts to attach a canonical chain tip to other
    /// disconnected chains. Returns the final status of the
    /// old tip, its inverse height and the new tip.
    fn attempt_attach_valid(
        &'a mut self,
        tip: &mut Arc<B>,
        tip_state: B::ChainState,
        inverse_height: &mut u64,
        status: &mut OrphanType,
    ) {
        let block_hash = tip.block_hash().unwrap();
        assert!(self.valid_tips.contains(&block_hash));
        assert!(self.disconnected_heads_mapping.get(&block_hash).is_none());
        assert!(self.disconnected_tips_mapping.get(&block_hash).is_none());

        // Init validations mappings for tip
        if self.validations_mapping.get(&block_hash).is_none() {
            self.validations_mapping.insert(block_hash.clone(), status.clone());
        }

        let tip_clone = tip.clone();
        let iterable = self
            .disconnected_heads_heights
            .iter()
            .filter(|(h, (_, largest_tip))| {
                let tips = self.disconnected_heads_mapping.get(h).unwrap();
                assert!(tips.contains(&largest_tip));

                let head = self.orphan_pool.get(h).unwrap();
                let parent_hash = head.parent_hash().unwrap();

                parent_hash == tip_clone.block_hash().unwrap()
            });

        let mut to_write: Vec<(Hash, Arc<B>, u64, B::ChainState)> = Vec::new();

        // For each matching head, make the descending
        // branches, valid chains.
        for (head_hash, (largest_height, largest_tip)) in iterable {
            let head = self.orphan_pool.get(head_hash).unwrap();
            let tip_state = B::append_condition(head.clone(), tip_state.duplicate());

            if let Ok(tip_state) = tip_state {
                let largest_tip = self.orphan_pool.get(&largest_tip).unwrap().clone();
                let tip_height = tip.height();
                let inverse_h = largest_height - tip_height;
                to_write.push((head_hash.clone(), largest_tip, inverse_h, tip_state));
            } else {
                // TODO: Maybe cleanup here? Issue #109
            }
        }

        for (head_hash, new_tip, inverse_h, tip_state) in to_write {
            *status = OrphanType::BelongsToValidChain;
            
            if inverse_h > *inverse_height {
                *inverse_height = inverse_h;
                *tip = new_tip;
            }

            // Remove old tip if we found a match
            self.valid_tips.remove(&block_hash);
            self.valid_tips_states.remove(&block_hash);
            let old_tip_status = self.validations_mapping.get_mut(&block_hash).unwrap();
            *old_tip_status = OrphanType::BelongsToValidChain;

            self.make_valid_tips(&head_hash.clone(), tip_state);
        }

        // Update inverse heights
        self.traverse_inverse(tip.clone(), 0, true);
    }

    /// Recursively changes the validation status of the tips
    /// of the given head to `OrphanType::ValidChainTip`
    /// and of their parents to `OrphanType::BelongsToValid`.
    ///
    /// Also removes all the disconnected mappings related to the head.
    /// 
    /// This function will short-circuit paths that have an invalid chain
    /// state transition.
    fn make_valid_tips(&'a mut self, head: &Hash, head_state: B::ChainState) {
        if self.disconnected_heads_mapping.remove(head).is_some() {
            let head_block = self.orphan_pool.get(head).unwrap();
            let mut cur_height = head_block.height() + 1;
            let mut previous: HashMap<Hash, B::ChainState> = HashMap::new();

            self.valid_tips.insert(head.clone());
            self.valid_tips_states.insert(head.clone(), head_state.duplicate());

            previous.insert(head.clone(), head_state);
            self.disconnected_heads_heights.remove(head);
            self.disconnected_tips_mapping.remove(head);

            // Update status of head, initially to being a valid chain tip
            let status = self
                .validations_mapping
                .get_mut(head)
                .unwrap();

            *status = OrphanType::ValidChainTip;

            // Traverse paths starting from the head vertex,
            // updating along the way the validation statuses
            // of encountered events.
            loop {
                let heights_entry = self.heights_mapping.get(&cur_height);

                if let Some(heights_entry) = heights_entry {
                    let mut matched_set = HashSet::new();
                    let mut new_previous_set = HashMap::with_capacity(previous.len());

                    // Find entries in the next height that have
                    // parents in the previous events set.
                    let iter = heights_entry
                        .iter()
                        .map(|(h, _)| h);

                    // Update status of all matches
                    for m in iter {
                        let e = self.orphan_pool.get(m).unwrap();
                        let parent_hash = e.parent_hash().unwrap();
                            
                        if let Some(state) = previous.get(&parent_hash) {
                            // TODO: Reduce number of state clones
                            if let Ok(state) = B::append_condition(e.clone(), state.duplicate()) {
                                // Change head status if we have a match
                                let status = self
                                    .validations_mapping
                                    .get_mut(head)
                                    .unwrap();

                                *status = OrphanType::BelongsToValidChain;

                                let block_hash = e.block_hash().unwrap();
                                self.valid_tips_states.remove(&parent_hash);
                                self.valid_tips.remove(&parent_hash);
                                self.disconnected_heads_mapping.remove(&parent_hash);
                                self.disconnected_tips_mapping.remove(&parent_hash);
                                new_previous_set.insert(block_hash, state);
                                matched_set.insert(parent_hash);

                                let status = self
                                    .validations_mapping
                                    .get_mut(m)
                                    .unwrap();

                                *status = OrphanType::BelongsToValidChain;
                            } else {
                                // TODO: Maybe cleanup here? Issue #109
                            }
                        } 
                    }

                    let previous_keys: HashSet<Hash> = previous.keys().cloned().collect();

                    // Make non matched valid tips
                    for tip_hash in previous_keys.difference(&matched_set) {
                        let state = previous.get(tip_hash).unwrap();

                        // Update status
                        let status = self.validations_mapping.get_mut(tip_hash).unwrap();
                        *status = OrphanType::ValidChainTip;

                        // Update mappings
                        self.disconnected_tips_mapping.remove(&tip_hash);
                        self.disconnected_heads_mapping.remove(&tip_hash);
                        self.valid_tips.insert(tip_hash.clone());
                        self.valid_tips_states.insert(tip_hash.clone(), state.duplicate());
                    }

                    previous = new_previous_set;
                    cur_height += 1;
                } else {
                    for (tip_hash, state) in previous {
                        // Update status
                        let status = self.validations_mapping.get_mut(&tip_hash).unwrap();
                        *status = OrphanType::ValidChainTip;

                        // Update mappings
                        self.disconnected_tips_mapping.remove(&tip_hash);
                        self.disconnected_heads_mapping.remove(&tip_hash);
                        self.valid_tips.insert(tip_hash.clone());
                        self.valid_tips_states.insert(tip_hash, state);
                    }

                    break;
                }
            }
        }
    }

    /// Traverses the parents of the orphan and updates their
    /// inverse heights according to the provided start height
    /// of the orphan. The third argument specifies if we should
    /// mark the traversed chain as a valid canonical chain.
    fn traverse_inverse(&mut self, orphan: Arc<B>, start_height: u64, make_valid: bool) {
        let mut cur_inverse = start_height;
        let mut current = orphan.clone();
        let mut branch_stack = Vec::new();

        // This flag only makes sense when the
        // starting inverse height is 0.
        if make_valid {
            assert_eq!(start_height, 0);
            let key = orphan.block_hash().unwrap();

            // Mark orphan as being tip of a valid chain
            if let Some(validation) = self.validations_mapping.get_mut(&key) {
                *validation = OrphanType::ValidChainTip;
            } else {
                self.validations_mapping
                    .insert(key, OrphanType::ValidChainTip);
            }
        }

        // Traverse parents and update inverse height
        // until we reach a missing block or the
        // canonical chain.
        while let Some(parent) = self.orphan_pool.get(&current.parent_hash().unwrap()) {
            let par_height = parent.height();
            let orphans = self.heights_mapping.get_mut(&par_height).unwrap();
            let inverse_h_entry = orphans.get_mut(&parent.block_hash().unwrap()).unwrap();

            if *inverse_h_entry < cur_inverse + 1 {
                *inverse_h_entry = cur_inverse + 1;
            }

            // Mark as belonging to valid chain
            if make_valid {
                let key = parent.block_hash().unwrap();

                if let Some(validation) = self.validations_mapping.get_mut(&key) {
                    *validation = OrphanType::BelongsToValidChain;
                } else {
                    self.validations_mapping
                        .insert(key, OrphanType::BelongsToValidChain);
                }
            }

            branch_stack.push(parent.clone());
            current = parent.clone();
            cur_inverse += 1;
        }
    }

    /// Returns an atomic reference to the genesis block in the chain.
    pub fn genesis() -> Arc<B> {
        B::genesis()
    }

    pub fn query(&self, hash: &Hash) -> Option<Arc<B>> {
        if let Some(stored) = self.db.get(hash) {
            Some(B::from_bytes(&stored).unwrap())
        } else {
            None
        }
    }

    pub fn query_by_height(&self, height: u64) -> Option<Arc<B>> {
        unimplemented!();
    }

    pub fn block_height(&self, hash: &Hash) -> Option<u64> {
        unimplemented!();
    }

    pub fn append_block(&'a mut self, block: Arc<B>) -> Result<(), ChainErr> {
        //println!("DEBUG PUSHED BLOCK HASH: {:?}", block.block_hash().unwrap());
        let min_height = if self.height > B::MIN_HEIGHT {
            self.height - B::MIN_HEIGHT
        } else {
            1
        };

        if block.height() > self.height + B::MAX_HEIGHT || block.height() < min_height {
            return Err(ChainErr::BadHeight);
        }

        let block_hash = block.block_hash().unwrap();

        // Check for existence
        if self.orphan_pool.get(&block_hash).is_some() || self.db.get(&block_hash).is_some() {
            return Err(ChainErr::AlreadyInChain);
        }

        let tip = &self.canonical_tip;

        if let Some(parent_hash) = block.parent_hash() {
            // First attempt to place the block after the
            // tip canonical block.
            if parent_hash == tip.block_hash().unwrap() {
                let height = block.height();
                
                // The height must be equal to that of the parent plus one
                if height != self.height + 1 {
                    return Err(ChainErr::BadHeight);
                }

                let append_condition = {
                    match B::append_condition(block.clone(), self.canonical_tip_state.duplicate()) {
                        // Set new tip state if the append can proceed
                        Ok(new_tip_state) => Some(new_tip_state),
                        _ => None
                    }
                };

                if let Some(new_tip_state) = append_condition {
                    // Write block to the chain
                    self.write_block(block);

                    // Perform checkpoint
                    {
                        let height = self.height;
                        let last_checkpoint_height = self.last_checkpoint_height.unwrap_or(0);

                        // Checkpoint state if we have reached the quota
                        if height - last_checkpoint_height == B::CHECKPOINT_INTERVAL as u64 {
                            if let None = self.earliest_checkpoint_height {
                                self.earliest_checkpoint_height = Some(height);
                            }

                            self.last_checkpoint_height = Some(height);
                            let checkpoint_id = new_tip_state.checkpoint();

                            // Store checkpoint id
                            self.disk_heights_checkpoints.insert(height, checkpoint_id);
                        }
                    }

                    self.canonical_tip_state = new_tip_state;

                    // Process orphans
                    self.process_orphans(height + 1);

                    Ok(())
                } else {
                    Err(ChainErr::BadAppendCondition)
                }
            } else {
                if self.orphan_pool.len() >= B::MAX_ORPHANS {
                    return Err(ChainErr::TooManyOrphans);
                }

                // If the parent exists and it is not the canonical
                // tip this means that this block is represents a
                // potential fork in the chain so we add it to the
                // orphan pool.
                match self.db.get(&parent_hash) {
                    Some(parent_block) => {
                        let height = block.height();
                        let parent_height = B::from_bytes(&parent_block).unwrap().height();

                        // The height must be equal to that of the parent plus one
                        if height != parent_height + 1 {
                            return Err(ChainErr::BadHeight);
                        }

                        let parent_state: B::ChainState = if let (Some(earliest_checkpoint_height), Some(last_checkpoint_height)) = (self.earliest_checkpoint_height, self.last_checkpoint_height) {
                            if parent_height < earliest_checkpoint_height {
                                return Err(ChainErr::NoCheckpointFound);
                            }

                            if parent_height == last_checkpoint_height {
                                // Simply retrieve checkpointed state in this case
                                let id = self.disk_heights_checkpoints.get(&parent_height).unwrap();
                                
                                // Load disk state
                                if let Ok(state) = B::ChainState::load_from_disk(*id) {
                                    state
                                } else {
                                    panic!("Could not find disk state!");
                                }
                            } else if parent_height > last_checkpoint_height {
                                self.fetch_next_state(last_checkpoint_height, parent_height)
                            } else {
                                self.search_fetch_next_state(parent_height)
                            }
                        } else {
                            self.search_fetch_next_state(parent_height)
                        };

                        let tip_state = match B::append_condition(block.clone(), parent_state) {
                            Ok(new_tip_state) => Some(new_tip_state),
                            Err(_) => None
                        };

                        if let Some(tip_state) = tip_state {
                            // Insert new state to valid tips mapping
                            self.valid_tips_states.insert(block.block_hash().unwrap(), tip_state.duplicate());
                            
                            let mut status = OrphanType::ValidChainTip;
                            let mut tip = block.clone();
                            let mut _inverse_height = 0;

                            self.write_orphan(block, OrphanType::ValidChainTip, 0);
                            self.attempt_attach_valid(&mut tip, tip_state, &mut _inverse_height, &mut status);

                            if let OrphanType::ValidChainTip = status {
                                // Do nothing
                            } else {
                                self.attempt_switch(tip);
                            }

                            Ok(())
                        } else {
                            Err(ChainErr::BadAppendCondition)
                        }
                    }
                    None => {
                        // The parent is an orphan
                        if let Some(parent_block) = self.orphan_pool.get(&parent_hash) {
                            let height = block.height();

                            // The height must be equal to that of the parent plus one
                            if height != parent_block.height() + 1 {
                                return Err(ChainErr::BadHeight);
                            }

                            let parent_status =
                                self.validations_mapping.get_mut(&parent_hash).unwrap();

                            match parent_status {
                                OrphanType::DisconnectedTip => {
                                    let head = self
                                        .disconnected_tips_mapping
                                        .get(&parent_hash)
                                        .unwrap()
                                        .clone();
                                    let tips =
                                        self.disconnected_heads_mapping.get_mut(&head).unwrap();
                                    let (largest_height, _) =
                                        self.disconnected_heads_heights.get(&head).unwrap();

                                    // Change the status of the old tip
                                    *parent_status = OrphanType::BelongsToDisconnected;

                                    // Replace old tip in mappings
                                    tips.remove(&parent_hash);
                                    tips.insert(block_hash.clone());

                                    self.disconnected_tips_mapping.remove(&parent_hash);

                                    // Replace largest height if this is the case
                                    if block.height() > *largest_height {
                                        self.disconnected_heads_heights.insert(
                                            head.clone(),
                                            (block.height(), block_hash.clone()),
                                        );
                                    }

                                    self.write_orphan(
                                        block.clone(),
                                        OrphanType::DisconnectedTip,
                                        0,
                                    );

                                    self.disconnected_tips_mapping
                                        .insert(block_hash.clone(), head.clone());
                                    let status = self
                                        .attempt_attach(&block_hash, OrphanType::DisconnectedTip);

                                    if let OrphanType::DisconnectedTip = status {
                                        self.traverse_inverse(block, 0, false);
                                    } else {
                                        // Write final status
                                        self.validations_mapping.insert(block_hash.clone(), status);

                                        // Make sure head tips don't contain pushed block's hash
                                        let tips =
                                            self.disconnected_heads_mapping.get_mut(&head).unwrap();
                                        tips.remove(&block_hash);
                                        self.disconnected_tips_mapping.remove(&block_hash);
                                    }
                                }
                                OrphanType::ValidChainTip => {
                                    let append_condition = {
                                        let tip_state = self.valid_tips_states.get_mut(&parent_hash).unwrap();

                                        match B::append_condition(block.clone(), tip_state.duplicate()) {
                                            // Set new tip state if the append can proceed
                                            Ok(new_tip_state) => {
                                                *tip_state = new_tip_state.duplicate();
                                                Some(new_tip_state)
                                            },
                                            _ => None
                                        }
                                    };

                                    if let Some(tip_state) = append_condition {
                                        // Change status of old tip
                                        *parent_status = OrphanType::BelongsToValidChain;

                                        let mut status = OrphanType::ValidChainTip;
                                        let mut tip = block.clone();
                                        let mut inverse_height = 0;

                                        // Mark orphan as the new tip
                                        self.write_orphan(block.clone(), status, inverse_height);


                                        // Attempt to attach to disconnected chains
                                        self.attempt_attach_valid(
                                            &mut tip,
                                            tip_state.duplicate(),
                                            &mut inverse_height,
                                            &mut status,
                                        );


                                        // Traverse parents and modify their inverse heights
                                        self.traverse_inverse(
                                            block.clone(),
                                            inverse_height,
                                            inverse_height == 0,
                                        );

                                        // Update tips set
                                        self.valid_tips.remove(&parent_hash);

                                        if let OrphanType::ValidChainTip = status {
                                            self.valid_tips.insert(tip.block_hash().unwrap());
                                            self.valid_tips_states.insert(tip.block_hash().unwrap(), tip_state);
                                        }

                                        // Check if the new tip's height is greater than
                                        // the canonical chain, and if so, switch chains.
                                        self.attempt_switch(tip);
                                    } else {
                                        return Err(ChainErr::BadAppendCondition);
                                    }
                                }
                                OrphanType::BelongsToDisconnected => {
                                    self.write_orphan(
                                        block.clone(),
                                        OrphanType::DisconnectedTip,
                                        0,
                                    );

                                    let head = {
                                        // Traverse parents until we find the head block
                                        let mut current = parent_hash.clone();
                                        let mut result = None;

                                        loop {
                                            if self
                                                .disconnected_heads_mapping
                                                .get(&current)
                                                .is_some()
                                            {
                                                result = Some(current);
                                                break;
                                            }

                                            if let Some(orphan) = self.orphan_pool.get(&current) {
                                                current = orphan.parent_hash().unwrap();
                                            } else {
                                                unreachable!();
                                            }
                                        }

                                        result.unwrap()
                                    };

                                    // Add to disconnected mappings
                                    let tips =
                                        self.disconnected_heads_mapping.get_mut(&head).unwrap();

                                    tips.insert(block_hash.clone());
                                    self.disconnected_tips_mapping
                                        .insert(block_hash.clone(), head.clone());

                                    let status = self
                                        .attempt_attach(&block_hash, OrphanType::DisconnectedTip);

                                    if let OrphanType::DisconnectedTip = status {
                                        self.disconnected_tips_mapping
                                            .insert(block_hash.clone(), head);
                                        self.traverse_inverse(block.clone(), 0, false);
                                    } else {
                                        // Write final status
                                        self.validations_mapping.insert(block_hash.clone(), status);

                                        // Make sure head tips don't contain pushed block's hash
                                        let tips =
                                            self.disconnected_heads_mapping.get_mut(&head).unwrap();
                                        tips.remove(&block_hash);
                                        self.disconnected_tips_mapping.remove(&block_hash);
                                    }
                                }
                                OrphanType::BelongsToValidChain => {
                                    // TODO: Maybe cache intermediate states? 
                                    let tip_state = {
                                        let mut visited_stack = Vec::new();
                                        let head = {
                                            let mut current = parent_hash.clone();

                                            // Traverse parents until we find a canonical block
                                            loop {
                                                let cur = self.orphan_pool.get(&current).unwrap();
                                                let parent_hash = cur.parent_hash().unwrap();

                                                visited_stack.push(cur.clone());

                                                if self.db.get(&parent_hash).is_some() {
                                                    current = parent_hash;
                                                    break;
                                                }

                                                current = parent_hash;
                                            }

                                            B::from_bytes(&self.db.get(&current).unwrap()).unwrap()
                                        };

                                        // Retrieve state associated with the head's parent
                                        let state = self.search_fetch_next_state(head.height() - 1);
                                        let mut state = B::append_condition(head.clone(), state)?;

                                        // Compute tip state
                                        while let Some(b) = visited_stack.pop() {
                                            state = B::append_condition(b, state)?;
                                        }
 
                                        B::append_condition(block.clone(), state)?
                                    };

                                    let mut status = OrphanType::ValidChainTip;
                                    let mut tip = block.clone();
                                    let mut inverse_height = 0;

                                    let tip_hash = tip.block_hash().unwrap();

                                    // Write tip to valid tips set
                                    self.valid_tips.insert(tip_hash.clone());
                                    self.valid_tips_states.insert(tip_hash, tip_state.duplicate());

                                    // Attempt to attach disconnected chains
                                    // to the new valid tip.
                                    self.attempt_attach_valid(
                                        &mut tip,
                                        tip_state,
                                        &mut inverse_height,
                                        &mut status,
                                    );

                                    // Write orphan, traverse and update inverse heights,
                                    // then attempt to switch the canonical chain.
                                    self.write_orphan(block, status, inverse_height);
                                    self.traverse_inverse(
                                        tip.clone(),
                                        inverse_height,
                                        inverse_height == 0,
                                    );
                                    self.attempt_switch(tip);
                                }
                            }

                            Ok(())
                        } else {
                            // Add first to disconnected mappings
                            let mut set = HashSet::new();
                            set.insert(block_hash.clone());

                            // Init disconnected mappings
                            self.disconnected_heads_mapping
                                .insert(block_hash.clone(), set);
                            self.disconnected_tips_mapping
                                .insert(block_hash.clone(), block_hash.clone());
                            self.disconnected_heads_heights
                                .insert(block_hash.clone(), (block.height(), block_hash.clone()));

                            // Init heights mappings
                            if let Some(entry) = self.heights_mapping.get_mut(&block.height()) {
                                entry.insert(block_hash.clone(), 0);
                            } else {
                                let mut hm = HashMap::new();
                                hm.insert(block_hash.clone(), 0);

                                self.heights_mapping.insert(block.height(), hm);
                            }

                            // Add block to orphan pool
                            self.orphan_pool.insert(block_hash.clone(), block.clone());

                            let status =
                                self.attempt_attach(&block_hash, OrphanType::DisconnectedTip);
                            let mut found_match = None;

                            // Attempt to attach the new disconnected
                            // chain to any valid chain.
                            for tip_hash in self.valid_tips.iter() {
                                let tip = self.orphan_pool.get(tip_hash).unwrap();

                                if parent_hash == tip.block_hash().unwrap() {
                                    found_match = Some(tip);
                                    break;
                                }
                            }

                            if let Some(tip) = found_match {
                                let tip_state = self.valid_tips_states.get(&tip.block_hash().unwrap()).unwrap().duplicate();
                                let mut _status = OrphanType::ValidChainTip;
                                let mut _tip = tip.clone();
                                let mut _inverse_height = 0;

                                self.write_orphan(block, status, 0);
                                self.attempt_attach_valid(
                                    &mut _tip,
                                    tip_state,
                                    &mut _inverse_height,
                                    &mut _status,
                                );

                                Ok(())
                            } else {
                                self.write_orphan(block, status, 0);
                                Ok(())
                            }
                        }
                    }
                }
            }
        } else {
            Err(ChainErr::NoParentHash)
        }
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn canonical_tip(&self) -> Arc<B> {
        self.canonical_tip.clone()
    }

    fn search_fetch_next_state(&'a self, target_height: u64) -> B::ChainState {
        // Find a height with an earlier checkpoint
        // than the target height.
        let height = {
            if let Some(last_checkpoint_height) = self.last_checkpoint_height {
                let mut current = last_checkpoint_height;
                let interval = B::CHECKPOINT_INTERVAL as u64;

                loop {
                    if current - interval < target_height {
                        break;
                    }

                    current -= interval;
                }

                current
            } else {
                target_height
            }
        };

        self.fetch_next_state(height, target_height)
    }

    /// Fetches the matching state of target height, starting from height.
    fn fetch_next_state(&'a self, mut height: u64, target_height: u64) -> B::ChainState {
        assert!(target_height <= self.height);
        assert!(height <= target_height);

        let mut state = {
            if height == 0 {
                return B::genesis_state();
            } else if height < B::CHECKPOINT_INTERVAL as u64 {
                height = 0;
                B::genesis_state()
            } else {
                // Load disk state
                let id = self.disk_heights_checkpoints.get(&height).unwrap();

                if let Ok(state) = B::ChainState::load_from_disk(*id) {
                    state
                } else {
                    panic!("Could not find disk state!");
                }
            }
        };

        if height == target_height {
            state
        } else {
            let mut cur_height = height + 1;
            
            loop {
                // Retrieve block key via height
                let height_key = crypto::hash_slice(&encode_be_u64!(cur_height));
                let block_hash = self.db.get(&height_key).unwrap();
                let mut hash = [0; 32];
                hash.copy_from_slice(&block_hash);

                // Retrieve block
                let hash = Hash(hash);
                let block = B::from_bytes(&self.db.get(&hash).unwrap()).unwrap();

                // Compute next state
                state = B::append_condition(block, state.duplicate()).unwrap();

                if cur_height == target_height {
                    break;
                }

                cur_height += 1;
            }

            state
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::easy_chain::block::EasyBlock;
    use chrono::prelude::*;
    use quickcheck::*;
    use rand::*;
    use byteorder::WriteBytesExt;
    use std::str::FromStr;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    macro_rules! count {
        () => (0);
        ($fst:expr) => (1);
        ($fst:expr, $snd:expr) => (2);
        ($fst:expr, $snd:expr $(, $v:expr)*) => (1 + count!($snd $(, $v)*));
    }

    macro_rules! set {
        ($fst:expr $(, $v:expr)*) => ({
            let mut set = HashSet::with_capacity(count!($fst $(, $v)*));

            set.insert($fst);
            $(set.insert($v);)*

            set
        });
    }

    use std::hash::Hasher;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Nonce used for creating unique `DummyBlock` hashes
    static NONCE: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone, Debug)]
    /// Dummy block used for testing
    struct DummyBlock {
        hash: Hash,
        parent_hash: Hash,
        height: u64,
        ip: SocketAddr,
    }

    impl DummyBlock {
        pub fn new(parent_hash: Option<Hash>, ip: SocketAddr, height: u64) -> DummyBlock {
            let hash =
                crypto::hash_slice(&format!("block-{}", NONCE.load(Ordering::Relaxed)).as_bytes());
            NONCE.fetch_add(1, Ordering::Relaxed);
            let parent_hash = parent_hash.unwrap();

            DummyBlock {
                hash,
                parent_hash,
                height,
                ip,
            }
        }
    }

    impl PartialEq for DummyBlock {
        fn eq(&self, other: &DummyBlock) -> bool {
            self.block_hash().unwrap() == other.block_hash().unwrap()
        }
    }

    impl Eq for DummyBlock {}

    impl HashTrait for DummyBlock {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.block_hash().unwrap().hash(state);
        }
    }

    impl<'a> Block<'a> for DummyBlock {
        type ChainState = DummyCheckpoint;

        fn genesis() -> Arc<Self> {
            let genesis = DummyBlock {
                hash: Hash::NULL,
                parent_hash: Hash::NULL,
                ip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 44034),
                height: 0,
            };

            Arc::new(genesis)
        }

        fn genesis_state() -> DummyCheckpoint {
            DummyCheckpoint::genesis()
        }

        fn parent_hash(&self) -> Option<Hash> {
            Some(self.parent_hash.clone())
        }

        fn block_hash(&self) -> Option<Hash> {
            Some(self.hash.clone())
        }

        fn timestamp(&self) -> DateTime<Utc> {
            unimplemented!();
        }

        fn height(&self) -> u64 {
            self.height
        }

        fn address(&self) -> Option<&SocketAddr> {
            Some(&self.ip)
        }

        fn after_write() -> Option<Box<FnMut(Arc<Self>)>> {
            None
        }

        fn append_condition(block: Arc<DummyBlock>, mut chain_state: Self::ChainState) -> Result<Self::ChainState, ChainErr> {
            let valid = chain_state.height() == block.height() - 1;
            

            if valid {
                chain_state.increment();
                Ok(chain_state)
            } else {
                Err(ChainErr::BadAppendCondition)
            }
        }

        fn to_bytes(&self) -> Vec<u8> {
            let mut buf = Vec::new();
            let height = encode_be_u64!(self.height);
            let ip = format!("{}", self.ip);
            let ip = ip.as_bytes();

            buf.write_u8(ip.len() as u8).unwrap();
            buf.extend_from_slice(&height);
            buf.extend_from_slice(&self.hash.0.to_vec());
            buf.extend_from_slice(&self.parent_hash.0.to_vec());
            buf.extend_from_slice(ip);

            buf
        }

        fn from_bytes(bytes: &[u8]) -> Result<Arc<Self>, &'static str> {
            let mut buf = bytes.to_vec();
            let ip_len = buf[0];
            let _: Vec<u8> = buf.drain(..1).collect();
            let height_bytes: Vec<u8> = buf.drain(..8).collect();
            let height = decode_be_u64!(&height_bytes).unwrap();
            let hash_bytes: Vec<u8> = buf.drain(..32).collect();
            let parent_hash_bytes: Vec<u8> = buf.drain(..32).collect();
            let ip: Vec<u8> = buf.drain(..ip_len as usize).collect();
            let ip = std::str::from_utf8(&ip).unwrap();
            let ip = SocketAddr::from_str(&ip).unwrap();
            let mut hash = [0; 32];
            let mut parent_hash = [0; 32];

            hash.copy_from_slice(&hash_bytes);
            parent_hash.copy_from_slice(&parent_hash_bytes);

            let hash = Hash(hash);
            let parent_hash = Hash(parent_hash);

            Ok(Arc::new(DummyBlock {
                height,
                hash,
                ip,
                parent_hash,
            }))
        }
    }

    #[test]
    fn it_rewinds_to_genesis() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let height_1_key = crypto::hash_slice(&vec![0, 0, 0, 0, 0, 0, 0, 1]);
        let height_2_key = crypto::hash_slice(&vec![0, 0, 0, 0, 0, 0, 0, 2]);
        let height_3_key = crypto::hash_slice(&vec![0, 0, 0, 0, 0, 0, 0, 3]);
        let height_4_key = crypto::hash_slice(&vec![0, 0, 0, 0, 0, 0, 0, 4]);
        let height_5_key = crypto::hash_slice(&vec![0, 0, 0, 0, 0, 0, 0, 5]);
        let height_6_key = crypto::hash_slice(&vec![0, 0, 0, 0, 0, 0, 0, 6]);
        let height_7_key = crypto::hash_slice(&vec![0, 0, 0, 0, 0, 0, 0, 7]);

        hard_chain.append_block(A.clone()).unwrap();
        hard_chain.append_block(B.clone()).unwrap();
        hard_chain.append_block(C.clone()).unwrap();
        hard_chain.append_block(D.clone()).unwrap();
        hard_chain.append_block(E.clone()).unwrap();
        hard_chain.append_block(F.clone()).unwrap();
        hard_chain.append_block(G.clone()).unwrap();

        assert_eq!(hard_chain.db.get(&height_1_key).unwrap().to_vec(), A.block_hash().unwrap().0.to_vec());
        assert_eq!(hard_chain.db.get(&height_2_key).unwrap().to_vec(), B.block_hash().unwrap().0.to_vec());
        assert_eq!(hard_chain.db.get(&height_3_key).unwrap().to_vec(), C.block_hash().unwrap().0.to_vec());
        assert_eq!(hard_chain.db.get(&height_4_key).unwrap().to_vec(), D.block_hash().unwrap().0.to_vec());
        assert_eq!(hard_chain.db.get(&height_5_key).unwrap().to_vec(), E.block_hash().unwrap().0.to_vec());
        assert_eq!(hard_chain.db.get(&height_6_key).unwrap().to_vec(), F.block_hash().unwrap().0.to_vec());
        assert_eq!(hard_chain.db.get(&height_7_key).unwrap().to_vec(), G.block_hash().unwrap().0.to_vec());

        hard_chain.rewind(&DummyBlock::genesis().block_hash().unwrap()).unwrap();

        assert!(hard_chain.orphan_pool.get(&A.block_hash().unwrap()).is_some());
        assert!(hard_chain.orphan_pool.get(&B.block_hash().unwrap()).is_some());
        assert!(hard_chain.orphan_pool.get(&C.block_hash().unwrap()).is_some());
        assert!(hard_chain.orphan_pool.get(&D.block_hash().unwrap()).is_some());
        assert!(hard_chain.orphan_pool.get(&E.block_hash().unwrap()).is_some());
        assert!(hard_chain.orphan_pool.get(&F.block_hash().unwrap()).is_some());
        assert!(hard_chain.orphan_pool.get(&G.block_hash().unwrap()).is_some());

        // Check for heights cleanup
        assert!(hard_chain.db.get(&height_1_key).is_none());
        assert!(hard_chain.db.get(&height_2_key).is_none());
        assert!(hard_chain.db.get(&height_3_key).is_none());
        assert!(hard_chain.db.get(&height_4_key).is_none());
        assert!(hard_chain.db.get(&height_5_key).is_none());
        assert!(hard_chain.db.get(&height_6_key).is_none());
        assert!(hard_chain.db.get(&height_7_key).is_none());
    }

    #[test]
    fn stages_append_test1() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_tertiary = Arc::new(D_tertiary);

        hard_chain.append_block(E_second.clone()).unwrap();
        hard_chain.append_block(F_second.clone()).unwrap();

        assert_eq!(hard_chain.height(), 0);

        // We should have a disconnected chain of `E''` and `F''`
        // with the tip of `E''` pointing to `F''`.
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            E_second.block_hash().unwrap()
        );
        let heads_mapping = hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let (largest_height, largest_tip) = hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        assert!(heads_mapping.contains(&F_second.block_hash().unwrap()));
        assert_eq!(*largest_height, F_second.height());
        assert_eq!(largest_tip, &F_second.block_hash().unwrap());

        hard_chain.append_block(A.clone()).unwrap();
        hard_chain.append_block(B.clone()).unwrap();

        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B);

        hard_chain.append_block(F.clone()).unwrap();
        hard_chain.append_block(G.clone()).unwrap();

        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B);

        // We should have a disconnected chain of `F` and `G`
        // with the tip of `G` pointing to `F`.
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        let heads_mapping = hard_chain
            .disconnected_heads_mapping
            .get(&F.block_hash().unwrap())
            .unwrap();
        let (largest_height, largest_tip) = hard_chain
            .disconnected_heads_heights
            .get(&F.block_hash().unwrap())
            .unwrap();
        assert!(heads_mapping.contains(&G.block_hash().unwrap()));
        assert_eq!(*largest_height, G.height());
        assert_eq!(largest_tip, &G.block_hash().unwrap());
        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B);

        // We now append `B'` and the canonical tip should still be `B`
        hard_chain.append_block(B_prime.clone()).unwrap();

        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B);

        hard_chain.append_block(C_prime.clone()).unwrap();

        assert_eq!(hard_chain.height(), 3);
        assert_eq!(hard_chain.canonical_tip(), C_prime);

        hard_chain.append_block(C.clone()).unwrap();
        assert_eq!(hard_chain.height(), 3);
        assert_eq!(hard_chain.canonical_tip(), C_prime);

        hard_chain.append_block(D.clone()).unwrap();

        assert_eq!(hard_chain.height(), 4);
        assert_eq!(hard_chain.canonical_tip(), D);

        // After appending `E` the chain should connect the old tip
        // which is `D` to our previous disconnected chain of `F` -> `G`.
        hard_chain.append_block(E.clone()).unwrap();

        assert_eq!(hard_chain.height(), 7);
        assert_eq!(hard_chain.canonical_tip(), G);
    }

    #[test]
    fn stages_append_test2() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_tertiary = Arc::new(D_tertiary);

        hard_chain.append_block(A.clone()).unwrap();

        assert_eq!(hard_chain.height(), 1);

        hard_chain.append_block(E_second.clone()).unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        hard_chain.append_block(D_second.clone()).unwrap();

        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );

        hard_chain.append_block(F_second.clone()).unwrap();

        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // We should have a disconnected chain of `E''` and `F''`
        // with the tip of `D''` pointing to `F''`.
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            D_second.block_hash().unwrap()
        );
        let heads_mapping = hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let (largest_height, largest_tip) = hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        assert!(heads_mapping.contains(&F_second.block_hash().unwrap()));
        assert_eq!(*largest_height, F_second.height());
        assert_eq!(largest_tip, &F_second.block_hash().unwrap());

        assert_eq!(hard_chain.height(), 1);
        assert_eq!(hard_chain.canonical_tip(), A);

        hard_chain.append_block(C.clone()).unwrap();
        hard_chain.append_block(D.clone()).unwrap();
        hard_chain.append_block(F.clone()).unwrap();
        hard_chain.append_block(E.clone()).unwrap();
        hard_chain.append_block(G.clone()).unwrap();

        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        assert_eq!(hard_chain.height(), 1);
        assert_eq!(hard_chain.canonical_tip(), A);

        // We now append `B'` and the canonical tip should be `B'`
        hard_chain.append_block(B_prime.clone()).unwrap();
        assert_eq!(hard_chain.valid_tips_states.len(), 0);

        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B_prime);

        hard_chain.append_block(C_second.clone()).unwrap();

        // The chain should now be pointing to `F''` as being the canonical tip
        assert_eq!(hard_chain.height(), 6);
        assert_eq!(hard_chain.canonical_tip(), F_second);

        // We now append `B` and the chain should switch to `G` as the canonical tip
        hard_chain.append_block(B.clone()).unwrap();

        assert_eq!(hard_chain.height(), 7);
        assert_eq!(hard_chain.canonical_tip(), G);
    }

    #[test]
    /// Assertions in stages on random order
    /// of appended blocks.
    fn stages_append_test3() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_tertiary = Arc::new(D_tertiary);

        hard_chain.append_block(C_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(3));

        hard_chain.append_block(D_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(*C_second_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(hard_chain.max_orphan_height, Some(4));

        hard_chain.append_block(F.clone()).unwrap();
        assert_eq!(hard_chain.max_orphan_height, Some(6));
        hard_chain.append_block(D_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(*C_second_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 0);
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        hard_chain.append_block(C_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 0);

        hard_chain.append_block(D.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 0);
        assert_eq!(*D_ih, 0);

        hard_chain.append_block(G.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_ih, 0);

        hard_chain.append_block(B_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 2);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_ih, 0);

        hard_chain.append_block(D_tertiary.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 2);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        hard_chain.append_block(C.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 2);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        hard_chain.append_block(E_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let E_prime_ih = hard_chain
            .heights_mapping
            .get(&E_prime.height())
            .unwrap()
            .get(&E_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 3);
        assert_eq!(*C_prime_ih, 2);
        assert_eq!(*D_prime_ih, 1);
        assert_eq!(*E_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        hard_chain.append_block(B.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let E_prime_ih = hard_chain
            .heights_mapping
            .get(&E_prime.height())
            .unwrap()
            .get(&E_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.valid_tips, HashSet::new());
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 3);
        assert_eq!(*C_prime_ih, 2);
        assert_eq!(*D_prime_ih, 1);
        assert_eq!(*E_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*B_ih, 2);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        hard_chain.append_block(A.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        let mut tips = HashSet::new();
        tips.insert(D.block_hash().unwrap());
        tips.insert(D_second.block_hash().unwrap());
        tips.insert(D_tertiary.block_hash().unwrap());

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*B_ih, 2);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        assert_eq!(hard_chain.valid_tips, tips);
        assert_eq!(hard_chain.height(), 5);
        assert_eq!(hard_chain.canonical_tip(), E_prime);
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(E_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        let mut tips = HashSet::new();
        tips.insert(D.block_hash().unwrap());
        tips.insert(E_second.block_hash().unwrap());
        tips.insert(D_tertiary.block_hash().unwrap());

        assert_eq!(*C_second_ih, 2);
        assert_eq!(*D_second_ih, 1);
        assert_eq!(*E_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*B_ih, 2);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        assert_eq!(hard_chain.valid_tips, tips);
        assert_eq!(hard_chain.height(), 5);
        assert_eq!(hard_chain.canonical_tip(), E_prime);
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(F_second.clone()).unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let E_prime_ih = hard_chain
            .heights_mapping
            .get(&E_prime.height())
            .unwrap()
            .get(&E_prime.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        let mut tips = HashSet::new();
        tips.insert(D.block_hash().unwrap());
        tips.insert(E_prime.block_hash().unwrap());
        tips.insert(D_tertiary.block_hash().unwrap());

        assert_eq!(*C_prime_ih, 2);
        assert_eq!(*D_prime_ih, 1);
        assert_eq!(*E_prime_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*B_ih, 2);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        assert_eq!(hard_chain.valid_tips, tips);
        assert_eq!(hard_chain.height(), 6);
        assert_eq!(hard_chain.canonical_tip(), F_second);
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(E.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let E_prime_ih = hard_chain
            .heights_mapping
            .get(&E_prime.height())
            .unwrap()
            .get(&E_prime.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        let mut tips = HashSet::new();
        tips.insert(F_second.block_hash().unwrap());
        tips.insert(E_prime.block_hash().unwrap());
        tips.insert(D_tertiary.block_hash().unwrap());

        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_prime_ih, 2);
        assert_eq!(*D_prime_ih, 1);
        assert_eq!(*E_prime_ih, 0);
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*D_tertiary_ih, 0);

        assert_eq!(hard_chain.valid_tips, tips);
        assert_eq!(hard_chain.height(), 7);
        assert_eq!(hard_chain.canonical_tip(), G);
        assert_eq!(hard_chain.max_orphan_height, Some(6));
    }

    #[test]
    /// Assertions in stages on random order
    /// of appended blocks.
    ///
    /// The order is the following:
    /// D'', E'', C'', F, F'', C, D',
    /// G, D''', B', C', B, E, D, A, E'
    ///
    /// And fails with yielding F'' as the canonical
    /// tip instead of G at commit hash `d0ad0bd6a7422f6308b96a34a6f7725662c8b7d4`.
    fn stages_append_test4() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_tertiary = Arc::new(D_tertiary);

        hard_chain.append_block(D_second.clone()).unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*D_second_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(4));

        // Check disconnected heads mapping
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            set![D_second.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            D_second.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            (D_second.height(), D_second.block_hash().unwrap())
        );

        hard_chain.append_block(E_second.clone()).unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*D_second_ih, 1);
        assert_eq!(*E_second_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(5));

        // Check disconnected heads mapping
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            set![E_second.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            D_second.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            (E_second.height(), E_second.block_hash().unwrap())
        );

        hard_chain.append_block(C_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 2);
        assert_eq!(*D_second_ih, 1);
        assert_eq!(*E_second_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(5));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![E_second.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (E_second.height(), E_second.block_hash().unwrap())
        );

        hard_chain.append_block(F.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 2);
        assert_eq!(*D_second_ih, 1);
        assert_eq!(*E_second_ih, 0);
        assert_eq!(*F_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![E_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![F.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (E_second.height(), E_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (F.height(), F.block_hash().unwrap())
        );

        hard_chain.append_block(F_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*F_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![F.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (F.height(), F.block_hash().unwrap())
        );

        hard_chain.append_block(C.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![F.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (F.height(), F.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );

        hard_chain.append_block(D_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 0);
        assert_eq!(*D_prime_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![F.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            set![D_prime.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            D_prime.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (F.height(), F.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            (D_prime.height(), D_prime.block_hash().unwrap())
        );

        hard_chain.append_block(G.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&G.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![G.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            set![D_prime.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&F.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            D_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (G.height(), G.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            (D_prime.height(), D_prime.block_hash().unwrap())
        );

        hard_chain.append_block(D_tertiary.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&G.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![G.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            set![D_prime.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            set![D_tertiary.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&F.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            D_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            D_tertiary.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (G.height(), G.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            (D_prime.height(), D_prime.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            (D_tertiary.height(), D_tertiary.block_hash().unwrap())
        );

        hard_chain.append_block(B_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&G.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![G.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            set![D_prime.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            set![D_tertiary.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&F.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            B_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            D_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            D_tertiary.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (G.height(), G.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            (D_prime.height(), D_prime.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            (D_tertiary.height(), D_tertiary.block_hash().unwrap())
        );

        hard_chain.append_block(C_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&G.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_prime.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            set![
                F_second.block_hash().unwrap(),
                D_prime.block_hash().unwrap(),
                D_tertiary.block_hash().unwrap()
            ]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![G.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&F.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_prime.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            B_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            B_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            B_prime.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_prime.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_tertiary.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (G.height(), G.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );;

        hard_chain.append_block(B.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*B_ih, 1);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(E.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let E_ih = hard_chain
            .heights_mapping
            .get(&E.height())
            .unwrap()
            .get(&E.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*B_ih, 1);
        assert_eq!(*C_ih, 0);
        assert_eq!(*E_ih, 2);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(D.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let E_ih = hard_chain
            .heights_mapping
            .get(&E.height())
            .unwrap()
            .get(&E.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*E_ih, 2);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_ih, 3);
        assert_eq!(*C_ih, 4);
        assert_eq!(*B_ih, 5);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(A.clone()).unwrap();
        hard_chain.append_block(E_prime.clone()).unwrap();

        assert_eq!(hard_chain.height(), 7);
        assert_eq!(hard_chain.canonical_tip(), G);
        assert_eq!(hard_chain.max_orphan_height, Some(6));
    }

    #[test]
    /// Assertions in stages on random order
    /// of appended blocks.
    ///
    /// The order is the following:
    /// G, C', C'', E', C, B', F'', E'', B, A, D', F, D''', E, D, D'',
    fn stages_append_test5() {
        let db = test_helpers::init_tempdb();
        let mut chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_tertiary = Arc::new(D_tertiary);

        let mut blocks = vec![
            G.clone(),
            C_prime.clone(),
            C_second.clone(),
            E_prime.clone(),
            C.clone(),
            B_prime.clone(),
            F_second.clone(),
            E_second.clone(),
            B.clone(),
            A.clone(),
            D_prime.clone(),
            F.clone(),
            D_tertiary.clone(),
            E.clone(),
            D.clone(),
            D_second.clone(),
        ];

        chain.append_block(blocks.remove(0)).unwrap(); // G
        chain.append_block(blocks.remove(0)).unwrap(); // C_prime
        chain.append_block(blocks.remove(0)).unwrap(); // C_second
        chain.append_block(blocks.remove(0)).unwrap(); // E_prime
        chain.append_block(blocks.remove(0)).unwrap(); // C
        chain.append_block(blocks.remove(0)).unwrap(); // B_prime
        chain.append_block(blocks.remove(0)).unwrap(); // F_second
        chain.append_block(blocks.remove(0)).unwrap(); // E_second
        chain.append_block(blocks.remove(0)).unwrap(); // B
        chain.append_block(blocks.remove(0)).unwrap(); // A
        chain.append_block(blocks.remove(0)).unwrap(); // D_prime

        assert_eq!(chain.height(), 5);
        assert_eq!(chain.canonical_tip, E_prime.clone());
        assert_eq!(chain.valid_tips, set![C_second.block_hash().unwrap(), C.block_hash().unwrap()]);

        chain.append_block(blocks.remove(0)).unwrap(); // F
        chain.append_block(blocks.remove(0)).unwrap(); // D_tertiary
        chain.append_block(blocks.remove(0)).unwrap(); // E
        chain.append_block(blocks.remove(0)).unwrap(); // D
        chain.append_block(blocks.remove(0)).unwrap(); // D_second

        assert_eq!(chain.height(), 7);
        assert_eq!(chain.canonical_tip, G);
        assert_eq!(chain.valid_tips, set![E_prime.block_hash().unwrap(), D_tertiary.block_hash().unwrap(), F_second.block_hash().unwrap()]);
    }

    #[test]
    /// Assertions in stages of random block order.
    /// 
    /// The sample ordering, taken from the stress test,
    /// is the following:
    /// E', E'', D'', C, D''', F, D, B', C'', E, F'', G, A, C', D', B
    fn stages_append_test6() {
        let db = test_helpers::init_tempdb();
        let mut chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_tertiary = Arc::new(D_tertiary);

        let mut blocks = vec![
            E_prime.clone(),
            E_second.clone(),
            D_second.clone(),
            C.clone(),
            D_tertiary.clone(),
            F.clone(),
            D.clone(),
            B_prime.clone(),
            C_second.clone(),
            E.clone(),
            F_second.clone(),
            G.clone(),
            A.clone(),
            C_prime.clone(),
            D_prime.clone(),
            B.clone(),
        ];

        chain.append_block(blocks.remove(0)).unwrap(); // E_prime
        chain.append_block(blocks.remove(0)).unwrap(); // E_second
        chain.append_block(blocks.remove(0)).unwrap(); // D_second
        chain.append_block(blocks.remove(0)).unwrap(); // C
        chain.append_block(blocks.remove(0)).unwrap(); // D_tertiary
        chain.append_block(blocks.remove(0)).unwrap(); // F
        chain.append_block(blocks.remove(0)).unwrap(); // D
        chain.append_block(blocks.remove(0)).unwrap(); // B_prime
        chain.append_block(blocks.remove(0)).unwrap(); // C_second
        chain.append_block(blocks.remove(0)).unwrap(); // E
        chain.append_block(blocks.remove(0)).unwrap(); // F_second
        chain.append_block(blocks.remove(0)).unwrap(); // G
        chain.append_block(blocks.remove(0)).unwrap(); // A
        chain.append_block(blocks.remove(0)).unwrap(); // C_prime
        chain.append_block(blocks.remove(0)).unwrap(); // D_prime
        chain.append_block(blocks.remove(0)).unwrap(); // B

        assert_eq!(chain.height(), 7);
        assert_eq!(chain.canonical_tip, G);
        assert_eq!(chain.valid_tips, set![E_prime.block_hash().unwrap(), D_tertiary.block_hash().unwrap(), F_second.block_hash().unwrap()]);
    }

    #[test]
    /// Assertions in stages of random block order.
    /// 
    /// The sample ordering, taken from the stress test,
    /// is the following:
    /// E, D''', D', A, B, F'', E'', C, F, C'', D'', G, C', E', D, B'
    fn stages_append_test7() {
        let db = test_helpers::init_tempdb();
        let mut chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_tertiary = Arc::new(D_tertiary);

        let mut blocks = vec![
            E.clone(),
            D_tertiary.clone(),
            D_prime.clone(),
            A.clone(),
            B.clone(),
            F_second.clone(),
            E_second.clone(),
            C.clone(),
            F.clone(),
            C_second.clone(),
            D_second.clone(),
            G.clone(),
            C_prime.clone(),
            E_prime.clone(),
            D.clone(),
            B_prime.clone(),
        ];

        chain.append_block(blocks.remove(0)).unwrap(); // E
        chain.append_block(blocks.remove(0)).unwrap(); // D_tertiary
        chain.append_block(blocks.remove(0)).unwrap(); // D_prime
        chain.append_block(blocks.remove(0)).unwrap(); // A
        chain.append_block(blocks.remove(0)).unwrap(); // B
        chain.append_block(blocks.remove(0)).unwrap(); // F_second
        chain.append_block(blocks.remove(0)).unwrap(); // E_second
        chain.append_block(blocks.remove(0)).unwrap(); // C
        chain.append_block(blocks.remove(0)).unwrap(); // F
        chain.append_block(blocks.remove(0)).unwrap(); // C_second
        chain.append_block(blocks.remove(0)).unwrap(); // D_second
        chain.append_block(blocks.remove(0)).unwrap(); // G
        chain.append_block(blocks.remove(0)).unwrap(); // C_prime
        chain.append_block(blocks.remove(0)).unwrap(); // E_prime
        chain.append_block(blocks.remove(0)).unwrap(); // D
        chain.append_block(blocks.remove(0)).unwrap(); // B_prime

        assert_eq!(chain.height(), 7);
        assert_eq!(chain.canonical_tip, G);
        assert_eq!(chain.valid_tips, set![E_prime.block_hash().unwrap(), D_tertiary.block_hash().unwrap(), F_second.block_hash().unwrap()]);
    }

    #[test]
    /// Assertions in stages of random block order.
    /// 
    /// The sample ordering, taken from the stress test,
    /// is the following:
    /// E, D'', D, A, C'', F'', G, E'', C, B, C', D''', E', F, B', D',
    fn stages_append_test8() {
        let db = test_helpers::init_tempdb();
        let mut chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

        let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
        let D_tertiary = Arc::new(D_tertiary);

        let mut blocks = vec![
            E.clone(),
            D_second.clone(),
            D.clone(),
            A.clone(),
            C_second.clone(),
            F_second.clone(),
            G.clone(),
            E_second.clone(),
            C.clone(),
            B.clone(),
            C_prime.clone(),
            D_tertiary.clone(),
            E_prime.clone(),
            F.clone(),
            B_prime.clone(),
            D_prime.clone(),
            
        ];

        chain.append_block(blocks.remove(0)).unwrap(); // E
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        chain.append_block(blocks.remove(0)).unwrap(); // D_second
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        chain.append_block(blocks.remove(0)).unwrap(); // D
        assert_eq!(chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        chain.append_block(blocks.remove(0)).unwrap(); // A
        assert_eq!(chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.height(), 1);
        assert_eq!(chain.canonical_tip, A);
        chain.append_block(blocks.remove(0)).unwrap(); // C_second
        assert_eq!(chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.height(), 1);
        assert_eq!(chain.canonical_tip, A);
        chain.append_block(blocks.remove(0)).unwrap(); // F_second
        assert_eq!(chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.height(), 1);
        assert_eq!(chain.canonical_tip, A);
        chain.append_block(blocks.remove(0)).unwrap(); // G
        assert_eq!(chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.height(), 1);
        assert_eq!(chain.canonical_tip, A);
        chain.append_block(blocks.remove(0)).unwrap(); // E_second
        assert_eq!(chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.height(), 1);
        assert_eq!(chain.canonical_tip, A);
        chain.append_block(blocks.remove(0)).unwrap(); // C
        assert_eq!(chain.validations_mapping.get(&C.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.height(), 1);
        assert_eq!(chain.canonical_tip, A);
        chain.append_block(blocks.remove(0)).unwrap(); // B
        assert_eq!(chain.height(), 5);
        assert_eq!(chain.canonical_tip, E);
        assert!(chain.validations_mapping.get(&C.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&D.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&E.block_hash().unwrap()).is_none());
        assert_eq!(chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        chain.append_block(blocks.remove(0)).unwrap(); // C_prime
        assert_eq!(chain.height(), 5);
        assert_eq!(chain.canonical_tip, E);
        assert!(chain.validations_mapping.get(&C.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&D.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&E.block_hash().unwrap()).is_none());
        assert_eq!(chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_prime.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        chain.append_block(blocks.remove(0)).unwrap(); // D_tertiary
        assert_eq!(chain.height(), 5);
        assert_eq!(chain.canonical_tip, E);
        assert!(chain.validations_mapping.get(&C.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&D.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&E.block_hash().unwrap()).is_none());
        assert_eq!(chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_prime.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        chain.append_block(blocks.remove(0)).unwrap(); // E_prime
        assert_eq!(chain.height(), 5);
        assert_eq!(chain.canonical_tip, E);
        assert!(chain.validations_mapping.get(&C.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&D.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&E.block_hash().unwrap()).is_none());
        assert_eq!(chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_prime.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&E_prime.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        chain.append_block(blocks.remove(0)).unwrap(); // F
        assert_eq!(chain.height(), 7);
        assert_eq!(chain.canonical_tip, G);
        assert!(chain.validations_mapping.get(&C.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&D.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&E.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&F.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&G.block_hash().unwrap()).is_none());
        assert_eq!(chain.validations_mapping.get(&C_prime.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&E_prime.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToDisconnected);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        chain.append_block(blocks.remove(0)).unwrap(); // B_prime
        assert_eq!(chain.height(), 7);
        assert_eq!(chain.canonical_tip, G);
        assert!(chain.validations_mapping.get(&C.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&D.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&E.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&F.block_hash().unwrap()).is_none());
        assert!(chain.validations_mapping.get(&G.block_hash().unwrap()).is_none());
        assert_eq!(chain.validations_mapping.get(&C_prime.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
        assert_eq!(chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), &OrphanType::ValidChainTip);
        assert_eq!(chain.validations_mapping.get(&E_prime.block_hash().unwrap()).unwrap(), &OrphanType::DisconnectedTip);
        assert_eq!(chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
        assert_eq!(chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
        assert_eq!(chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
        assert_eq!(chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::ValidChainTip);
        assert!(chain.valid_tips.contains(&F_second.block_hash().unwrap()));
        assert!(chain.valid_tips_states.get(&F_second.block_hash().unwrap()).is_some());
        assert!(chain.valid_tips.contains(&D_tertiary.block_hash().unwrap()));
        assert!(chain.valid_tips_states.get(&D_tertiary.block_hash().unwrap()).is_some());
        chain.append_block(blocks.remove(0)).unwrap(); // D_prime

        assert_eq!(chain.height(), 7);
        assert_eq!(chain.canonical_tip, G);
        assert_eq!(chain.valid_tips, set![E_prime.block_hash().unwrap(), D_tertiary.block_hash().unwrap(), F_second.block_hash().unwrap()]);
        assert!(chain.valid_tips_states.get(&E_prime.block_hash().unwrap()).is_some());
        assert!(chain.valid_tips_states.get(&D_tertiary.block_hash().unwrap()).is_some());
        assert!(chain.valid_tips_states.get(&F_second.block_hash().unwrap()).is_some());
    }

    quickcheck! {
        /// Stress test of chain append.
        ///
        /// We have a graph of chains of blocks with
        /// the following structure:
        /// ```
        /// GEN -> A -> B -> C -> D -> E -> F -> G
        ///        |
        ///         -> B' -> C' -> D' -> E'
        ///            |     |
        ///            |     -> D'''
        ///            |
        ///            -> C'' -> D'' -> E'' -> F''
        /// ```
        ///
        /// The tip of the block must always be `G`, regardless
        /// of the order in which the blocks are received. And
        /// the height of the chain must be that of `G` which is 7.
        fn append_stress_test() -> bool {
            let db = test_helpers::init_tempdb();
            let mut hard_chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

            let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
            let A = Arc::new(A);

            let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
            let B = Arc::new(B);

            let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
            let C = Arc::new(C);

            let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D = Arc::new(D);

            let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
            let E = Arc::new(E);

            let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
            let F = Arc::new(F);

            let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
            let G = Arc::new(G);

            let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
            let B_prime = Arc::new(B_prime);

            let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
            let C_prime = Arc::new(C_prime);

            let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D_prime = Arc::new(D_prime);

            let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
            let E_prime = Arc::new(E_prime);

            let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
            let C_second = Arc::new(C_second);

            let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D_second = Arc::new(D_second);

            let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
            let E_second = Arc::new(E_second);

            let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
            let F_second = Arc::new(F_second);

            let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D_tertiary = Arc::new(D_tertiary);

            let mut blocks = vec![
                A.clone(),
                B.clone(),
                C.clone(),
                D.clone(),
                E.clone(),
                F.clone(),
                G.clone(),
                B_prime.clone(),
                C_prime.clone(),
                D_prime.clone(),
                E_prime.clone(),
                C_second.clone(),
                D_second.clone(),
                E_second.clone(),
                F_second.clone(),
                D_tertiary.clone()
            ];

            // Shuffle blocks
            thread_rng().shuffle(&mut blocks);

            let mut block_letters = HashMap::new();

            block_letters.insert(A.block_hash().unwrap(), "A");
            block_letters.insert(B.block_hash().unwrap(), "B");
            block_letters.insert(C.block_hash().unwrap(), "C");
            block_letters.insert(D.block_hash().unwrap(), "D");
            block_letters.insert(E.block_hash().unwrap(), "E");
            block_letters.insert(F.block_hash().unwrap(), "F");
            block_letters.insert(G.block_hash().unwrap(), "G");
            block_letters.insert(B_prime.block_hash().unwrap(), "B'");
            block_letters.insert(C_prime.block_hash().unwrap(), "C'");
            block_letters.insert(D_prime.block_hash().unwrap(), "D'");
            block_letters.insert(E_prime.block_hash().unwrap(), "E'");
            block_letters.insert(C_second.block_hash().unwrap(), "C''");
            block_letters.insert(D_second.block_hash().unwrap(), "D''");
            block_letters.insert(E_second.block_hash().unwrap(), "E''");
            block_letters.insert(F_second.block_hash().unwrap(), "F''");
            block_letters.insert(D_tertiary.block_hash().unwrap(), "D'''");

            // Uncomment this for printing a failed order
            // let blocks_clone = blocks.clone();

            // std::panic::set_hook(Box::new(move |_| {
            //     print!("Failed block ordering: ");
            //     for b in blocks_clone.clone() {
            //         print!("{}, ", block_letters.get(&b.block_hash().unwrap()).unwrap());
            //     }
            //     print!("\n");
            // }));

            for b in blocks {
                hard_chain.append_block(b).unwrap();
            }

            assert_eq!(hard_chain.height(), 7);
            assert_eq!(hard_chain.canonical_tip, G);
            assert_eq!(hard_chain.valid_tips, set![E_prime.block_hash().unwrap(), D_tertiary.block_hash().unwrap(), F_second.block_hash().unwrap()]);
            assert!(hard_chain.valid_tips_states.get(&E_prime.block_hash().unwrap()).is_some());
            assert!(hard_chain.valid_tips_states.get(&D_tertiary.block_hash().unwrap()).is_some());
            assert!(hard_chain.valid_tips_states.get(&F_second.block_hash().unwrap()).is_some());
            assert!(hard_chain.validations_mapping.get(&A.block_hash().unwrap()).is_none());
            assert!(hard_chain.validations_mapping.get(&B.block_hash().unwrap()).is_none());
            assert!(hard_chain.validations_mapping.get(&C.block_hash().unwrap()).is_none());
            assert!(hard_chain.validations_mapping.get(&D.block_hash().unwrap()).is_none());
            assert!(hard_chain.validations_mapping.get(&E.block_hash().unwrap()).is_none());
            assert!(hard_chain.validations_mapping.get(&F.block_hash().unwrap()).is_none());
            assert!(hard_chain.validations_mapping.get(&G.block_hash().unwrap()).is_none());
            assert_eq!(hard_chain.validations_mapping.get(&B_prime.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
            assert_eq!(hard_chain.validations_mapping.get(&C_prime.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
            assert_eq!(hard_chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), &OrphanType::ValidChainTip);
            assert_eq!(hard_chain.validations_mapping.get(&E_prime.block_hash().unwrap()).unwrap(), &OrphanType::ValidChainTip);
            assert_eq!(hard_chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
            assert_eq!(hard_chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
            assert_eq!(hard_chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), &OrphanType::BelongsToValidChain);
            assert_eq!(hard_chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), &OrphanType::ValidChainTip);

            true
        }

        fn it_rewinds_correctly1() -> bool {
            let db = test_helpers::init_tempdb();
            let mut hard_chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

            let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
            let A = Arc::new(A);

            let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
            let B = Arc::new(B);

            let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
            let C = Arc::new(C);

            let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D = Arc::new(D);

            let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
            let E = Arc::new(E);

            let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
            let F = Arc::new(F);

            let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
            let G = Arc::new(G);

            let blocks = vec![
                A.clone(),
                B.clone(),
                C.clone(),
                D.clone(),
                E.clone(),
                F.clone(),
                G.clone(),
            ];

            for b in blocks {
                hard_chain.append_block(b).unwrap();
            }

            assert_eq!(hard_chain.height(), 7);
            assert_eq!(hard_chain.canonical_tip(), G.clone());
            assert_eq!(hard_chain.max_orphan_height, None);
            assert!(hard_chain.query(&A.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&B.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&C.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&D.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&E.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&F.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&G.block_hash().unwrap()).is_some());

            hard_chain.rewind(&B.block_hash().unwrap()).unwrap();

            assert_eq!(hard_chain.height(), 2);
            assert_eq!(hard_chain.canonical_tip(), B);
            assert_eq!(hard_chain.max_orphan_height, Some(7));
            assert!(hard_chain.valid_tips.contains(&G.block_hash().unwrap()));
            assert!(hard_chain.query(&A.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&B.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&C.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&D.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&E.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&F.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&G.block_hash().unwrap()).is_none());
            assert_eq!(*hard_chain.validations_mapping.get(&C.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&F.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            let mut tips = HashSet::new();
            tips.insert(G.block_hash().unwrap());

            assert_eq!(hard_chain.valid_tips, tips);
            true
        }

        fn it_rewinds_correctly2() -> bool {
            let db = test_helpers::init_tempdb();
            let mut hard_chain = Chain::<DummyBlock>::new(db, DummyCheckpoint::genesis(), true);

            let mut A = DummyBlock::new(Some(Hash::NULL), crate::random_socket_addr(), 1);
            let A = Arc::new(A);

            let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
            let B = Arc::new(B);

            let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), crate::random_socket_addr(), 3);
            let C = Arc::new(C);

            let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D = Arc::new(D);

            let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), crate::random_socket_addr(), 5);
            let E = Arc::new(E);

            let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), crate::random_socket_addr(), 6);
            let F = Arc::new(F);

            let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), crate::random_socket_addr(), 7);
            let G = Arc::new(G);

            let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), crate::random_socket_addr(), 2);
            let B_prime = Arc::new(B_prime);

            let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
            let C_prime = Arc::new(C_prime);

            let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D_prime = Arc::new(D_prime);

            let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), crate::random_socket_addr(), 5);
            let E_prime = Arc::new(E_prime);

            let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), crate::random_socket_addr(), 3);
            let C_second = Arc::new(C_second);

            let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D_second = Arc::new(D_second);

            let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), crate::random_socket_addr(), 5);
            let E_second = Arc::new(E_second);

            let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), crate::random_socket_addr(), 6);
            let F_second = Arc::new(F_second);

            let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), crate::random_socket_addr(), 4);
            let D_tertiary = Arc::new(D_tertiary);

            let blocks = vec![
                A.clone(),
                B.clone(),
                C.clone(),
                D.clone(),
                E.clone(),
                F.clone(),
                G.clone(),
                B_prime.clone(),
                C_prime.clone(),
                D_prime.clone(),
                E_prime.clone(),
                C_second.clone(),
                D_second.clone(),
                E_second.clone(),
                F_second.clone(),
                D_tertiary.clone(),
            ];

            let mut block_letters = HashMap::new();

            block_letters.insert(A.block_hash().unwrap(), "A");
            block_letters.insert(B.block_hash().unwrap(), "B");
            block_letters.insert(C.block_hash().unwrap(), "C");
            block_letters.insert(D.block_hash().unwrap(), "D");
            block_letters.insert(E.block_hash().unwrap(), "E");
            block_letters.insert(F.block_hash().unwrap(), "F");
            block_letters.insert(G.block_hash().unwrap(), "G");
            block_letters.insert(B_prime.block_hash().unwrap(), "B'");
            block_letters.insert(C_prime.block_hash().unwrap(), "C'");
            block_letters.insert(D_prime.block_hash().unwrap(), "D'");
            block_letters.insert(E_prime.block_hash().unwrap(), "E'");
            block_letters.insert(C_second.block_hash().unwrap(), "C''");
            block_letters.insert(D_second.block_hash().unwrap(), "D''");
            block_letters.insert(E_second.block_hash().unwrap(), "E''");
            block_letters.insert(F_second.block_hash().unwrap(), "F''");
            block_letters.insert(D_tertiary.block_hash().unwrap(), "D'''");

            for b in blocks {
                hard_chain.append_block(b).unwrap();
            }

            assert_eq!(hard_chain.height(), 7);
            assert_eq!(hard_chain.canonical_tip(), G.clone());
            assert!(hard_chain.query(&A.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&B.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&C.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&D.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&E.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&F.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&G.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&B_prime.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&C_prime.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&D_prime.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&E_prime.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&C_second.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&D_second.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&E_second.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&F_second.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&D_tertiary.block_hash().unwrap()).is_none());
            let mut tips = HashSet::new();
            tips.insert(F_second.block_hash().unwrap());
            tips.insert(E_prime.block_hash().unwrap());
            tips.insert(D_tertiary.block_hash().unwrap());

            assert_eq!(*hard_chain.validations_mapping.get(&B_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&C_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E_prime.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            let mut tips = HashSet::new();
            tips.insert(F_second.block_hash().unwrap());
            tips.insert(E_prime.block_hash().unwrap());
            tips.insert(D_tertiary.block_hash().unwrap());
            assert_eq!(tips, hard_chain.valid_tips);

            hard_chain.rewind(&B.block_hash().unwrap()).unwrap();

            assert_eq!(*hard_chain.validations_mapping.get(&B_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E_prime.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&C.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&F.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            let mut tips = HashSet::new();
            tips.insert(F_second.block_hash().unwrap());
            tips.insert(E_prime.block_hash().unwrap());
            tips.insert(D_tertiary.block_hash().unwrap());
            tips.insert(G.block_hash().unwrap());
            assert_eq!(tips, hard_chain.valid_tips);

            true
        }
    }
}
