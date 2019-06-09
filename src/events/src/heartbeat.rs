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

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use causality::Stamp;
use crypto::{BlakeHasher, Hash, PublicKey, SecretKey as Sk, Signature};
use merkle_light::hash::Algorithm;
use merkle_light::merkle::MerkleTree;
use network::NodeId;
use rayon::prelude::*;
use std::boxed::Box;
use std::hash::Hasher;
use std::io::Cursor;
use std::iter::FromIterator;
use transactions::*;

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Heartbeat {
    /// The node id of the event sender
    pub node_id: NodeId,

    /// The current timestamp of the sender
    pub stamp: Stamp,

    #[serde(skip_serializing_if = "Option::is_none")]
    /// The root hash of the transactions contained
    /// by the heartbeat event.
    pub root_hash: Option<Hash>,

    #[serde(skip_serializing_if = "Option::is_none")]
    /// The hash of the event
    pub hash: Option<Hash>,

    /// The hash of the parent event in the causal graph.
    pub parent_hash: Hash,

    #[serde(skip_serializing_if = "Option::is_none")]
    /// The signature of the sender
    pub signature: Option<Signature>,

    /// The transactions contained by the heartbeat event
    pub transactions: Vec<Box<Tx>>,
}

impl Heartbeat {
    pub const EVENT_TYPE: u8 = 0;

    /// Calculates the root hash of the merkle
    /// tree formed by the transactions stored
    /// in the heartbeat event.
    pub fn calculate_root_hash(&mut self) {
        let mut hasher = BlakeHasher::new();
        let txs_hashes: Vec<Hash> = self
            .transactions
            .iter()
            .map(|tx| {
                let message: Vec<u8> = tx.compute_hash_message();

                hasher.write(&message);
                hasher.hash()
            })
            .collect();

        let mt: MerkleTree<Hash, BlakeHasher> = MerkleTree::from_iter(txs_hashes);

        self.root_hash = Some(mt.root());
    }

    /// Signs the event with the given secret key.
    ///
    /// This function will panic if there already exists
    /// a signature and the address type doesn't match
    /// the signature type.
    pub fn sign(&mut self, skey: &Sk) {
        // Assemble data
        let message = assemble_message(&self);

        // Sign data
        let signature = crypto::sign(&message, skey, &self.node_id.to_pkey());

        self.signature = Some(signature);
    }

    /// Serializes a heartbeat struct.
    ///
    /// All fields are written in big endian.
    ///
    /// Fields:
    /// 1) Event type(0) - 8bits
    /// 2) Stamp length  - 16bits
    /// 3) Txs length    - 32bits
    /// 4) Node id       - 32byte binary
    /// 5) Root hash     - 32byte binary
    /// 6) Parent hash   - 32byte binary
    /// 7) Hash          - 32byte binary
    /// 8) Signature     - 64byte binary
    /// 9) Stamp         - Binary of stamp length
    /// 10) Transactions - Binary of txs length
    pub fn to_bytes(&self) -> Result<Vec<u8>, &'static str> {
        let mut buffer: Vec<u8> = Vec::new();
        let event_type: u8 = Self::EVENT_TYPE;

        let root_hash = if let Some(root_hash) = &self.root_hash {
            &root_hash.0
        } else {
            return Err("Root hash field is missing");
        };

        let hash = if let Some(hash) = &self.hash {
            &hash.0
        } else {
            return Err("Hash field is missing");
        };

        let signature = if let Some(signature) = &self.signature {
            signature
        } else {
            return Err("Signature field is missing");
        };

        // Serialize transactions
        let transactions: Result<Vec<Vec<u8>>, _> = self
            .transactions
            .par_iter()
            .map(|tx| match (*tx).to_bytes() {
                Ok(tx) => Ok(tx),
                Err(_) => Err("Bad transaction"),
            })
            .collect();

        if let Err(err) = transactions {
            return Err(err);
        }

        let node_id = &self.node_id.0;
        let parent_hash = &self.parent_hash.0;
        let mut transactions: Vec<u8> = rlp::encode_list::<Vec<u8>, _>(&transactions.unwrap());
        let mut stamp: Vec<u8> = self.stamp.to_bytes();

        let txs_len = transactions.len();
        let stamp_len = stamp.len();

        buffer.write_u8(event_type).unwrap();
        buffer.write_u16::<BigEndian>(stamp_len as u16).unwrap();
        buffer.write_u32::<BigEndian>(txs_len as u32).unwrap();

        buffer.append(&mut node_id.to_vec());
        buffer.append(&mut root_hash.to_vec());
        buffer.append(&mut parent_hash.to_vec());
        buffer.append(&mut hash.to_vec());
        buffer.append(&mut signature.inner_bytes());
        buffer.append(&mut stamp);
        buffer.append(&mut transactions);

        Ok(buffer)
    }

    /// Deserializes a heartbeat struct from a byte array
    pub fn from_bytes(bin: &[u8]) -> Result<Heartbeat, &'static str> {
        let mut rdr = Cursor::new(bin.to_vec());
        let event_type = if let Ok(result) = rdr.read_u8() {
            result
        } else {
            return Err("Bad event type");
        };

        if event_type != Self::EVENT_TYPE {
            return Err("Bad event type");
        }

        rdr.set_position(1);

        let stamp_len = if let Ok(result) = rdr.read_u16::<BigEndian>() {
            result
        } else {
            return Err("Bad stamp len");
        };

        rdr.set_position(3);

        let txs_len = if let Ok(result) = rdr.read_u32::<BigEndian>() {
            result
        } else {
            return Err("Bad transaction len");
        };

        // Consume cursor
        let mut buf = rdr.into_inner();
        let _: Vec<u8> = buf.drain(..7).collect();

        let node_id = if buf.len() > 32 as usize {
            let mut node_id = [0; 32];
            let node_id_vec: Vec<u8> = buf.drain(..32).collect();

            node_id.copy_from_slice(&node_id_vec);

            let is_valid_pk = PublicKey::from_bytes(&node_id).is_ok();

            if is_valid_pk {
                NodeId(node_id)
            } else {
                return Err("Invalid node id");
            }
        } else {
            return Err("Incorrect packet structure! Buffer size is smaller than the minimum size for the node id");
        };

        let root_hash = if buf.len() > 32 as usize {
            let mut hash = [0; 32];
            let hash_vec: Vec<u8> = buf.drain(..32).collect();

            hash.copy_from_slice(&hash_vec);

            Hash(hash)
        } else {
            return Err("Incorrect packet structure! Buffer size is smaller than the minimum size for the root hash");
        };

        let parent_hash = if buf.len() > 32 as usize {
            let mut hash = [0; 32];
            let hash_vec: Vec<u8> = buf.drain(..32).collect();

            hash.copy_from_slice(&hash_vec);

            Hash(hash)
        } else {
            return Err("Incorrect packet structure! Buffer size is smaller than the minimum size for the root hash");
        };

        let hash = if buf.len() > 32 as usize {
            let mut hash = [0; 32];
            let hash_vec: Vec<u8> = buf.drain(..32).collect();

            hash.copy_from_slice(&hash_vec);

            Hash(hash)
        } else {
            return Err("Incorrect packet structure! Buffer size is smaller than the minimum size for the hash");
        };

        let signature = if buf.len() > 64 as usize {
            let sig_vec: Vec<u8> = buf.drain(..64).collect();

            Signature::new(&sig_vec)
        } else {
            return Err("Incorrect packet structure! Buffer size is smaller than the minimum size for the signature");
        };

        let stamp = if buf.len() > stamp_len as usize {
            let stamp_bin: Vec<u8> = buf.drain(..stamp_len as usize).collect();

            if let Ok(stamp) = Stamp::from_bytes(&stamp_bin) {
                stamp
            } else {
                return Err("Bad stamp");
            }
        } else {
            return Err("Incorrect packet structure! Buffer size is smaller than the stamp length");
        };

        let transactions = if buf.len() == txs_len as usize {
            let ser_txs: Vec<Vec<u8>> = rlp::decode_list(&buf);
            let txs: Result<Vec<Box<Tx>>, _> = ser_txs
                .par_iter()
                .map(|tx| {
                    let tx_type = &tx[0];

                    match *tx_type {
                        1 => {
                            let deserialized = match Call::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(_) => return Err("Invalid call transaction"),
                            };

                            Ok(Box::new(Tx::Call(deserialized)))
                        }
                        2 => {
                            let deserialized = match OpenContract::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(e) => return Err(e),
                            };

                            Ok(Box::new(Tx::OpenContract(deserialized)))
                        }
                        3 => {
                            let deserialized = match Send::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(_) => return Err("Invalid send transaction"),
                            };

                            Ok(Box::new(Tx::Send(deserialized)))
                        }
                        4 => {
                            let deserialized = match CreateCurrency::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(_) => return Err("Invalid create currency transaction"),
                            };

                            Ok(Box::new(Tx::CreateCurrency(deserialized)))
                        }
                        5 => {
                            let deserialized = match CreateMintable::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(_) => return Err("Invalid create mintable transaction"),
                            };

                            Ok(Box::new(Tx::CreateMintable(deserialized)))
                        }
                        6 => {
                            let deserialized = match Mint::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(_) => return Err("Invalid mint transaction"),
                            };

                            Ok(Box::new(Tx::Mint(deserialized)))
                        }
                        7 => {
                            let deserialized = match Burn::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(_) => return Err("Invalid burn transaction"),
                            };

                            Ok(Box::new(Tx::Burn(deserialized)))
                        },
                        8 => {
                            let deserialized = match ChangeMinter::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(_) => return Err("Invalid `ChangeMinter` transaction"),
                            };

                            Ok(Box::new(Tx::ChangeMinter(deserialized)))
                        },
                        9 => {
                            let deserialized = match CreateUnique::from_bytes(&tx) {
                                Ok(result) => result,
                                Err(_) => return Err("Invalid `CreateUnique` transaction"),
                            };

                            Ok(Box::new(Tx::CreateUnique(deserialized)))
                        },
                        _ => return Err("Bad transaction type"),
                    }
                })
                .collect();

            match txs {
                Ok(result) => result,
                Err(err) => return Err(err),
            }
        } else {
            return Err("Incorrect packet structure! Buffer size is smaller than the size of the transactions");
        };

        let heartbeat = Heartbeat {
            node_id,
            stamp,
            transactions,
            parent_hash,
            root_hash: Some(root_hash),
            hash: Some(hash),
            signature: Some(signature),
        };

        Ok(heartbeat)
    }

    impl_hash!();
}

fn assemble_message(obj: &Heartbeat) -> Vec<u8> {
    unimplemented!();
}

#[cfg(test)]
use quickcheck::Arbitrary;

#[cfg(test)]
impl Arbitrary for Heartbeat {
    fn arbitrary<G: quickcheck::Gen>(g: &mut G) -> Heartbeat {
        let mut txs: Vec<Box<Tx>> = Vec::with_capacity(30);

        for _ in 0..30 {
            txs.push(Arbitrary::arbitrary(g));
        }

        Heartbeat {
            node_id: Arbitrary::arbitrary(g),
            parent_hash: Arbitrary::arbitrary(g),
            root_hash: Some(Arbitrary::arbitrary(g)),
            hash: Some(Arbitrary::arbitrary(g)),
            signature: Some(Arbitrary::arbitrary(g)),
            stamp: Arbitrary::arbitrary(g),
            transactions: txs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    quickcheck! {
        fn serialize_deserialize(tx: Heartbeat) -> bool {
            tx == Heartbeat::from_bytes(&Heartbeat::to_bytes(&tx).unwrap()).unwrap()
        }
    }
}
