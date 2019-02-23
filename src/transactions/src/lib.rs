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

#[cfg(test)]
extern crate tempfile;

#[cfg(test)]
extern crate kvdb_rocksdb;

#[macro_use]
extern crate unwrap;
#[macro_use]
extern crate quickcheck;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate bin_tools;

extern crate account;
extern crate byteorder;
extern crate causality;
extern crate crypto;
extern crate elastic_array;
extern crate hashdb;
extern crate hex;
extern crate network;
extern crate patricia_trie;
extern crate persistence;
extern crate purple_vm;
extern crate rand;
extern crate rust_decimal;
extern crate serde;

#[macro_use]
mod macros;

mod burn;
mod call;
mod change_minter;
mod create_currency;
mod create_mintable;
mod create_unique;
mod genesis;
mod issue_shares;
mod mint;
mod open_contract;
mod open_multi_sig;
mod open_shares;
mod pay;
mod send;

pub use burn::*;
pub use call::*;
pub use create_currency::*;
pub use create_mintable::*;
pub use genesis::*;
pub use issue_shares::*;
pub use mint::*;
pub use open_contract::*;
pub use open_multi_sig::*;
pub use open_shares::*;
pub use pay::*;
pub use send::*;

use crypto::Identity;
use patricia_trie::{TrieDBMut, TrieMut};
use persistence::{BlakeDbHasher, Codec};
use quickcheck::Arbitrary;
use rand::Rng;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Tx {
    Call(Call),
    OpenContract(OpenContract),
    Send(Send),
    Burn(Burn),
    CreateCurrency(CreateCurrency),
    CreateMintable(CreateMintable),
    Mint(Mint),
    IssueShares(IssueShares),
    OpenMultiSig(OpenMultiSig),
    OpenShares(OpenShares),
    Pay(Pay),
}

impl Tx {
    pub fn to_bytes(&self) -> Result<Vec<u8>, &'static str> {
        match *self {
            Tx::Call(ref tx) => tx.to_bytes(),
            Tx::OpenContract(ref tx) => tx.to_bytes(),
            Tx::Send(ref tx) => tx.to_bytes(),
            Tx::Burn(ref tx) => tx.to_bytes(),
            Tx::CreateCurrency(ref tx) => tx.to_bytes(),
            Tx::CreateMintable(ref tx) => tx.to_bytes(),
            Tx::Mint(ref tx) => tx.to_bytes(),
            Tx::IssueShares(ref tx) => tx.to_bytes(),
            Tx::OpenMultiSig(ref tx) => tx.to_bytes(),
            Tx::OpenShares(ref tx) => tx.to_bytes(),
            Tx::Pay(ref tx) => tx.to_bytes(),
        }
    }

    pub fn compute_hash_message(&self) -> Vec<u8> {
        match *self {
            Tx::Call(ref tx) => tx.compute_hash_message(),
            Tx::OpenContract(ref tx) => tx.compute_hash_message(),
            Tx::Send(ref tx) => tx.compute_hash_message(),
            Tx::Burn(ref tx) => tx.compute_hash_message(),
            Tx::CreateCurrency(ref tx) => tx.compute_hash_message(),
            Tx::CreateMintable(ref tx) => tx.compute_hash_message(),
            Tx::Mint(ref tx) => tx.compute_hash_message(),
            Tx::IssueShares(ref tx) => tx.compute_hash_message(),
            Tx::OpenMultiSig(ref tx) => tx.compute_hash_message(),
            Tx::OpenShares(ref tx) => tx.compute_hash_message(),
            Tx::Pay(ref tx) => tx.compute_hash_message(),
        }
    }

    pub fn arbitrary_valid(trie: &mut TrieDBMut<BlakeDbHasher, Codec>) -> Tx {
        let mut rng = rand::thread_rng();
        let random = rng.gen_range(2, 12);
        let id = Identity::new();

        match random {
            2 => Tx::OpenContract(OpenContract::arbitrary_valid(trie, id.skey().clone())),
            3 => Tx::Send(Send::arbitrary_valid(trie, id.skey().clone())),
            4 => Tx::Burn(Burn::arbitrary_valid(trie, id.skey().clone())),
            5 => Tx::CreateCurrency(CreateCurrency::arbitrary_valid(trie, id.skey().clone())),
            6 => Tx::CreateMintable(CreateMintable::arbitrary_valid(trie, id.skey().clone())),
            7 => Tx::Mint(Mint::arbitrary_valid(trie, id.skey().clone())),
            8 => Tx::IssueShares(IssueShares::arbitrary_valid(trie, id.skey().clone())),
            9 => Tx::OpenMultiSig(OpenMultiSig::arbitrary_valid(trie, id.skey().clone())),
            10 => Tx::OpenShares(OpenShares::arbitrary_valid(trie, id.skey().clone())),
            11 => Tx::Pay(Pay::arbitrary_valid(trie, id.skey().clone())),
            _ => panic!(),
        }
    }
}

impl Arbitrary for Tx {
    fn arbitrary<G: quickcheck::Gen>(g: &mut G) -> Tx {
        let mut rng = rand::thread_rng();
        let random = rng.gen_range(1, 12);

        match random {
            1 => Tx::Call(Arbitrary::arbitrary(g)),
            2 => Tx::OpenContract(Arbitrary::arbitrary(g)),
            3 => Tx::Send(Arbitrary::arbitrary(g)),
            4 => Tx::Burn(Arbitrary::arbitrary(g)),
            5 => Tx::CreateCurrency(Arbitrary::arbitrary(g)),
            6 => Tx::CreateMintable(Arbitrary::arbitrary(g)),
            7 => Tx::Mint(Arbitrary::arbitrary(g)),
            8 => Tx::IssueShares(Arbitrary::arbitrary(g)),
            9 => Tx::OpenMultiSig(Arbitrary::arbitrary(g)),
            10 => Tx::OpenShares(Arbitrary::arbitrary(g)),
            11 => Tx::Pay(Arbitrary::arbitrary(g)),
            _ => panic!(),
        }
    }
}
