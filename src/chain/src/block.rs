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

use crate::{ChainErr, EasyBlock, HardBlock, StateBlock};
use common::Checkpointable;
use chrono::prelude::*;
use crypto::Hash;
use std::boxed::Box;
use std::sync::Arc;
use std::net::SocketAddr;
use std::fmt::Debug;
use std::hash::Hash as HashTrait;

/// Generic block interface
pub trait Block<'a>: Debug + PartialEq + Eq + HashTrait {
    /// Per tip validation state
    type ChainState: Checkpointable<'a> + Debug;

    /// Size of the block cache.
    const BLOCK_CACHE_SIZE: usize = 20;

    /// Maximum orphans allowed.
    #[cfg(not(test))]
    const MAX_ORPHANS: usize = 100;

    #[cfg(test)]
    const MAX_ORPHANS: usize = 20;

    /// Number of blocks between a valid chain and
    /// the canonical in order for the valid chain
    /// to become canonical
    #[cfg(not(test))]
    const SWITCH_OFFSET: usize = 2;
    
    #[cfg(test)]
    const SWITCH_OFFSET: usize = 0;

    /// Blocks with height below the canonical height minus
    /// this number will be rejected.
    const MIN_HEIGHT: u64 = 10;

    /// Blocks with height above the canonical height plus
    /// this number will be rejected.
    const MAX_HEIGHT: u64 = 10;

    /// The number of blocks after which a state checkpoint will be made.
    /// 
    /// This number **MUST** be less or equal than the minimum accepted height.
    const CHECKPOINT_INTERVAL: usize = 10;

    /// Max checkpoints to keep.
    const MAX_CHECKPOINTS: usize = 10;

    /// How many blocks to keep behind the canonical 
    /// chain when pruning is enabled. This number should
    /// be equal to `CHECKPOINT_INTERVAL * MAX_CHECKPOINTS`.
    const BLOCKS_TO_KEEP: usize = 100;

    /// Returns the genesis block.
    fn genesis() -> Arc<Self>;

    /// Returns the genesis state of the chain
    fn genesis_state() -> Self::ChainState;

    /// Returns the hash of the block.
    fn block_hash(&self) -> Option<Hash>;

    /// Returns the parent hash of the block.
    fn parent_hash(&self) -> Option<Hash>;

    /// Returns the timestamp of the block.
    fn timestamp(&self) -> DateTime<Utc>;

    /// Returns the height of the block.
    fn height(&self) -> u64;

    /// Returns the ip of the block's miner
    fn address(&self) -> Option<&SocketAddr>;

    /// Callback that executes after a block is written to a chain.
    fn after_write() -> Option<Box<FnMut(Arc<Self>)>>;

    /// Condition that must result if successful, returns the state
    /// that is to be associated with the new appended block.
    /// 
    /// If this functions returns an `Err`, the block will not be appended.
    fn append_condition(block: Arc<Self>, chain_state: Self::ChainState) -> Result<Self::ChainState, ChainErr>;

    /// Serializes the block.
    fn to_bytes(&self) -> Vec<u8>;

    /// Deserializes the block
    fn from_bytes(bytes: &[u8]) -> Result<Arc<Self>, &'static str>;
}

/// Wrapper enum used **only** for serialization/deserialization
#[derive(Clone, Debug, PartialEq)]
pub enum BlockWrapper {
    EasyBlock(Arc<EasyBlock>),
    HardBlock(Arc<HardBlock>),
    StateBlock(Arc<StateBlock>),
}

impl BlockWrapper {
    pub fn from_bytes(bytes: &[u8]) -> Result<Arc<BlockWrapper>, &'static str> {
        let first_byte = bytes[0];

        match first_byte {
            EasyBlock::BLOCK_TYPE => Ok(Arc::new(BlockWrapper::EasyBlock(EasyBlock::from_bytes(bytes)?))),
            HardBlock::BLOCK_TYPE => Ok(Arc::new(BlockWrapper::HardBlock(HardBlock::from_bytes(bytes)?))),
            StateBlock::BLOCK_TYPE => Ok(Arc::new(BlockWrapper::StateBlock(StateBlock::from_bytes(bytes)?))),
            _ => return Err("Invalid block type")
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            BlockWrapper::EasyBlock(block) => block.to_bytes(),
            BlockWrapper::HardBlock(block) => block.to_bytes(),
            BlockWrapper::StateBlock(block) => block.to_bytes(),
        }
    }

    pub fn block_hash(&self) -> Option<Hash> {
        match self {
            BlockWrapper::EasyBlock(block) => block.block_hash(),
            BlockWrapper::HardBlock(block) => block.block_hash(),
            BlockWrapper::StateBlock(block) => block.block_hash(),
        }
    }
}

impl quickcheck::Arbitrary for BlockWrapper {
    fn arbitrary<G: quickcheck::Gen>(g: &mut G) -> BlockWrapper {
        use rand::Rng;

        let mut rng = rand::thread_rng();
        let random = rng.gen_range(0, 3);

        match random {
            0 => BlockWrapper::EasyBlock(quickcheck::Arbitrary::arbitrary(g)),
            1 => BlockWrapper::HardBlock(quickcheck::Arbitrary::arbitrary(g)),
            2 => BlockWrapper::StateBlock(quickcheck::Arbitrary::arbitrary(g)),
            _ => panic!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::*;

    quickcheck! {
        fn wrapper_serialize_deserialize(block: BlockWrapper) -> bool {
            BlockWrapper::from_bytes(&BlockWrapper::from_bytes(&block.to_bytes()).unwrap().to_bytes()).unwrap();

            true
        }
    }
}