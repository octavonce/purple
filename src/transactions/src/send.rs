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

use account::{Address, Balance, MultiSig, ShareMap, Signature};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use crypto::{Hash, PublicKey as Pk, SecretKey as Sk};
use patricia_trie::{TrieDBMut, TrieMut};
use persistence::{BlakeDbHasher, Codec};
use std::io::Cursor;
use std::str;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Send {
    from: Address,
    to: Address,
    amount: Balance,
    fee: Balance,
    asset_hash: Hash,
    fee_hash: Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<Hash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<Signature>,
}

impl Send {
    pub const TX_TYPE: u8 = 3;

    /// Applies the send transaction to the provided database.
    ///
    /// This function will panic if the `from` account does not exist.
    pub fn apply(&self, trie: &mut TrieDBMut<BlakeDbHasher, Codec>) {
        let bin_from = &self.from.to_bytes();
        let bin_to = &self.to.to_bytes();
        let bin_asset_hash = &self.asset_hash.to_vec();
        let bin_fee_hash = &self.fee_hash.to_vec();

        // Convert addresses to strings
        let from = hex::encode(bin_from);
        let to = hex::encode(bin_to);

        // Convert hashes to strings
        let asset_hash = hex::encode(bin_asset_hash);
        let fee_hash = hex::encode(bin_fee_hash);

        // Calculate nonce keys
        //
        // The key of a nonce has the following format:
        // `<account-address>.n`
        let from_nonce_key = format!("{}.n", from);
        let to_nonce_key = format!("{}.n", to);
        let from_nonce_key = from_nonce_key.as_bytes();
        let to_nonce_key = to_nonce_key.as_bytes();

        // Retrieve serialized nonces
        let bin_from_nonce = &trie.get(&from_nonce_key).unwrap().unwrap();
        let bin_to_nonce = trie.get(&to_nonce_key);

        // Read the nonce of the sender
        let mut from_nonce = decode_be_u64!(bin_from_nonce).unwrap();

        // Increment sender nonce
        from_nonce += 1;

        let from_nonce: Vec<u8> = encode_be_u64!(from_nonce);

        // Calculate currency keys
        //
        // The key of a currency entry has the following format:
        // `<account-address>.<currency-hash>`
        let from_cur_key = format!("{}.{}", from, asset_hash);
        let from_fee_key = format!("{}.{}", from, fee_hash);
        let to_cur_key = format!("{}.{}", to, asset_hash);

        // Calculate stock address key
        //
        // The key of a stock's address entry has the following format:
        // `<stock-hash>.adr`
        let stock_addr_key = format!("{}.adr", asset_hash);
        let stock_addr_key = stock_addr_key.as_bytes();

        match trie.get(&stock_addr_key) {
            // The transferred currency is a stock
            Ok(Some(addr)) => match bin_to_nonce {
                // The receiver account exists.
                Ok(Some(_)) => {
                    let addr = hex::encode(addr);
                    let share_map_key = format!("{}.sm", addr);
                    let share_map_key = share_map_key.as_bytes();

                    let sender_balance = unwrap!(
                        trie.get(&from_cur_key.as_bytes()).unwrap(),
                        "The sender does not have an entry for the given currency"
                    );

                    let mut sender_balance = unwrap!(
                        decode_be_u32!(&sender_balance),
                        "Invalid stored balance format"
                    );

                    let mut sender_fee_balance = unwrap!(
                        Balance::from_bytes(&unwrap!(
                            trie.get(&from_fee_key.as_bytes()).unwrap(),
                            "The sender does not have an entry for the given currency"
                        )),
                        "Invalid stored balance format"
                    );

                    let mut share_map = unwrap!(
                        ShareMap::from_bytes(&unwrap!(
                            trie.get(&share_map_key).unwrap(),
                            "There is no share map for the referenced account"
                        )),
                        "Invalid stored share map"
                    );

                    // Convert amount to u32
                    let amount = format!("{}", self.amount.clone());
                    let amount = amount.parse::<u32>().unwrap();

                    // Subtract from sender
                    sender_balance -= amount;
                    sender_fee_balance -= self.fee.clone();

                    // Transfer shares in share map
                    share_map.transfer_shares(
                        &self.from.unwrap_normal(),
                        &self.to.unwrap_normal(),
                        amount,
                    );

                    // The receiver account exists so we try to
                    // retrieve it's balance.
                    let receiver_balance = match trie.get(&to_cur_key.as_bytes()) {
                        Ok(Some(balance)) => decode_be_u32!(&balance).unwrap() + amount.clone(),
                        Ok(None) => amount.clone(),
                        Err(err) => panic!(err),
                    };

                    // Update trie
                    trie.insert(&to_cur_key.as_bytes(), &encode_be_u32!(receiver_balance))
                        .unwrap();
                    trie.insert(&from_cur_key.as_bytes(), &encode_be_u32!(sender_balance))
                        .unwrap();
                    trie.insert(&share_map_key, &share_map.to_bytes()).unwrap();
                    trie.insert(from_nonce_key, &from_nonce).unwrap();
                }
                Ok(None) => {
                    let addr = hex::encode(addr);
                    let share_map_key = format!("{}.sm", addr);
                    let share_map_key = share_map_key.as_bytes();

                    let sender_balance = unwrap!(
                        trie.get(&from_cur_key.as_bytes()).unwrap(),
                        "The sender does not have an entry for the given currency"
                    );

                    let mut sender_balance = unwrap!(
                        decode_be_u32!(&sender_balance),
                        "Invalid stored balance format"
                    );

                    let mut sender_fee_balance = unwrap!(
                        Balance::from_bytes(&unwrap!(
                            trie.get(&from_fee_key.as_bytes()).unwrap(),
                            "The sender does not have an entry for the given currency"
                        )),
                        "Invalid stored balance format"
                    );

                    let mut share_map = unwrap!(
                        ShareMap::from_bytes(&unwrap!(
                            trie.get(&share_map_key).unwrap(),
                            "There is no share map for the referenced account"
                        )),
                        "Invalid stored share map"
                    );

                    // Convert amount to u32
                    let amount = format!("{}", self.amount.clone());
                    let amount = amount.parse::<u32>().unwrap();

                    // Subtract from sender
                    sender_balance -= amount;
                    sender_fee_balance -= self.fee.clone();

                    // Transfer shares in share map
                    share_map.transfer_shares(
                        &self.from.unwrap_normal(),
                        &self.to.unwrap_normal(),
                        amount,
                    );

                    // The receiver account exists so we try to
                    // retrieve it's balance.
                    let receiver_balance = match trie.get(&to_cur_key.as_bytes()) {
                        Ok(Some(balance)) => decode_be_u32!(&balance).unwrap() + amount.clone(),
                        Ok(None) => amount.clone(),
                        Err(err) => panic!(err),
                    };

                    // Create new account by adding a `0` nonce entry.
                    trie.insert(&to_nonce_key, &[0, 0, 0, 0, 0, 0, 0, 0])
                        .unwrap();

                    // Update trie
                    trie.insert(&to_cur_key.as_bytes(), &encode_be_u32!(receiver_balance))
                        .unwrap();
                    trie.insert(&from_cur_key.as_bytes(), &encode_be_u32!(sender_balance))
                        .unwrap();
                    trie.insert(&share_map_key, &share_map.to_bytes()).unwrap();
                    trie.insert(from_nonce_key, &from_nonce).unwrap();
                }
                Err(err) => panic!(err),
            },
            // The transferred currency is a normal currency
            Ok(None) => match bin_to_nonce {
                // The receiver account exists.
                Ok(Some(_)) => {
                    if fee_hash == asset_hash {
                        // The transaction's fee is paid in the same currency
                        // that is being transferred, so we only retrieve one
                        // balance.
                        let mut sender_balance = unwrap!(
                            Balance::from_bytes(&unwrap!(
                                trie.get(&from_cur_key.as_bytes()).unwrap(),
                                "The sender does not have an entry for the given currency"
                            )),
                            "Invalid stored balance format"
                        );

                        // Subtract fee from sender balance
                        sender_balance -= self.fee.clone();

                        // Subtract amount transferred from sender balance
                        sender_balance -= self.amount.clone();

                        // The receiver account exists so we try to retrieve his balance
                        let receiver_balance: Balance = match trie.get(&to_cur_key.as_bytes()) {
                            Ok(Some(balance)) => {
                                Balance::from_bytes(&balance).unwrap() + self.amount.clone()
                            }
                            Ok(None) => self.amount.clone(),
                            Err(err) => panic!(err),
                        };

                        // Update trie
                        trie.insert(from_cur_key.as_bytes(), &sender_balance.to_bytes())
                            .unwrap();
                        trie.insert(to_cur_key.as_bytes(), &receiver_balance.to_bytes())
                            .unwrap();
                        trie.insert(from_nonce_key, &from_nonce).unwrap();
                    } else {
                        // The transaction's fee is paid in a different currency
                        // than the one being transferred so we retrieve both balances.
                        let mut sender_cur_balance = unwrap!(
                            Balance::from_bytes(&unwrap!(
                                trie.get(&from_cur_key.as_bytes()).unwrap(),
                                "The sender does not have an entry for the given currency"
                            )),
                            "Invalid stored balance format"
                        );

                        let mut sender_fee_balance = unwrap!(
                            Balance::from_bytes(&unwrap!(
                                trie.get(&from_fee_key.as_bytes()).unwrap(),
                                "The sender does not have an entry for the given currency"
                            )),
                            "Invalid stored balance format"
                        );

                        // Subtract fee from sender
                        sender_fee_balance -= self.fee.clone();

                        // Subtract amount transferred from sender
                        sender_cur_balance -= self.amount.clone();

                        // The receiver account exists so we try to retrieve his balance
                        let receiver_balance: Balance = match trie.get(&to_cur_key.as_bytes()) {
                            Ok(Some(balance)) => {
                                Balance::from_bytes(&balance).unwrap() + self.amount.clone()
                            }
                            Ok(None) => self.amount.clone(),
                            Err(err) => panic!(err),
                        };

                        // Update trie
                        trie.insert(from_cur_key.as_bytes(), &sender_cur_balance.to_bytes())
                            .unwrap();
                        trie.insert(from_fee_key.as_bytes(), &sender_fee_balance.to_bytes())
                            .unwrap();
                        trie.insert(to_cur_key.as_bytes(), &receiver_balance.to_bytes())
                            .unwrap();
                        trie.insert(from_nonce_key, &from_nonce).unwrap();
                    }
                }
                Ok(None) => {
                    // The receiver account does not exist so we create it.
                    //
                    // This can only happen if the receiver address is a normal address.
                    if let Address::Normal(_) = &self.to {
                        if fee_hash == asset_hash {
                            // The transaction's fee is paid in the same currency
                            // that is being transferred, so we only retrieve one
                            // balance.
                            let mut sender_balance = unwrap!(
                                Balance::from_bytes(&unwrap!(
                                    trie.get(&from_cur_key.as_bytes()).unwrap(),
                                    "The sender does not have an entry for the given currency"
                                )),
                                "Invalid stored balance format"
                            );

                            let receiver_balance = self.amount.clone();

                            // Subtract fee from sender balance
                            sender_balance -= self.fee.clone();

                            // Subtract amount transferred from sender balance
                            sender_balance -= self.amount.clone();

                            // Create new account by adding a `0` nonce entry.
                            trie.insert(&to_nonce_key, &[0, 0, 0, 0, 0, 0, 0, 0])
                                .unwrap();

                            // Update balances
                            trie.insert(from_cur_key.as_bytes(), &sender_balance.to_bytes())
                                .unwrap();
                            trie.insert(to_cur_key.as_bytes(), &receiver_balance.to_bytes())
                                .unwrap();
                            trie.insert(from_nonce_key, &from_nonce).unwrap();
                        } else {
                            // The transaction's fee is paid in a different currency
                            // than the one being transferred so we retrieve both balances.
                            let mut sender_cur_balance = unwrap!(
                                Balance::from_bytes(&unwrap!(
                                    trie.get(&from_cur_key.as_bytes()).unwrap(),
                                    "The sender does not have an entry for the given currency"
                                )),
                                "Invalid stored balance format"
                            );

                            let mut sender_fee_balance = unwrap!(
                                Balance::from_bytes(&unwrap!(
                                    trie.get(&from_fee_key.as_bytes()).unwrap(),
                                    "The sender does not have an entry for the given currency"
                                )),
                                "Invalid stored balance format"
                            );

                            let receiver_balance = self.amount.clone();

                            // Subtract fee from sender
                            sender_fee_balance -= self.fee.clone();

                            // Subtract amount transferred from sender
                            sender_cur_balance -= self.amount.clone();

                            // Create new account by adding a `0` nonce entry.
                            trie.insert(&to_nonce_key, &[0, 0, 0, 0, 0, 0, 0, 0])
                                .unwrap();

                            // Update balances
                            trie.insert(from_cur_key.as_bytes(), &sender_cur_balance.to_bytes())
                                .unwrap();
                            trie.insert(from_fee_key.as_bytes(), &sender_fee_balance.to_bytes())
                                .unwrap();
                            trie.insert(to_cur_key.as_bytes(), &receiver_balance.to_bytes())
                                .unwrap();
                            trie.insert(from_nonce_key, &from_nonce).unwrap();
                        }
                    } else {
                        panic!("The receiving account does not exist and it's address is not a normal one!")
                    }
                }
                Err(err) => panic!(err),
            },
            Err(err) => panic!(err),
        }
    }

    /// Signs the transaction with the given secret key.
    ///
    /// This function will panic if there already exists
    /// a signature and the address type doesn't match
    /// the signature type.
    pub fn sign(&mut self, skey: Sk) {
        // Assemble data
        let message = assemble_sign_message(&self);

        // Sign data
        let signature = crypto::sign(&message, &skey);

        match self.signature {
            Some(Signature::Normal(_)) => {
                if let Address::Normal(_) = self.from {
                    let result = Signature::Normal(signature);
                    self.signature = Some(result);
                } else {
                    panic!("Invalid address type");
                }
            }
            Some(Signature::MultiSig(ref mut sig)) => {
                if let Address::Normal(_) = self.from {
                    panic!("Invalid address type");
                } else {
                    // Append signature to the multi sig struct
                    sig.append_sig(signature);
                }
            }
            None => {
                if let Address::Normal(_) = self.from {
                    // Create a normal signature
                    let result = Signature::Normal(signature);

                    // Attach signature to struct
                    self.signature = Some(result);
                } else {
                    // Create a multi signature
                    let result = Signature::MultiSig(MultiSig::from_sig(signature));

                    // Attach signature to struct
                    self.signature = Some(result);
                }
            }
        };
    }

    /// Verifies the signature of the transaction.
    ///
    /// Returns `false` if the signature field is missing.
    ///
    /// This function panics if the transaction has a multi
    /// signature attached to it or if the signer's address
    /// is not a normal address.
    pub fn verify_sig(&mut self) -> bool {
        let message = assemble_sign_message(&self);

        match self.signature {
            Some(Signature::Normal(ref sig)) => {
                if let Address::Normal(ref addr) = self.from {
                    crypto::verify(&message, sig, &addr.pkey())
                } else {
                    panic!("The address of the signer is not a normal address!");
                }
            }
            Some(Signature::MultiSig(_)) => {
                panic!("Calling this function on a multi signature transaction is not permitted!");
            }
            None => false,
        }
    }

    /// Verifies the multi signature of the transaction.
    ///
    /// Returns `false` if the signature field is missing.
    ///
    /// This function panics if the transaction has a multi
    /// signature attached to it or if the signer's address
    /// is not a normal address.
    pub fn verify_multi_sig(&mut self, required_keys: u8, pkeys: &[Pk]) -> bool {
        if pkeys.len() < required_keys as usize {
            false
        } else {
            let message = assemble_sign_message(&self);

            match self.signature {
                Some(Signature::Normal(_)) => {
                    panic!("Calling this function on a transaction with a normal signature is not permitted!");
                }
                Some(Signature::MultiSig(ref sig)) => sig.verify(&message, required_keys, pkeys),
                None => false,
            }
        }
    }

    /// Verifies the multi signature of the transaction.
    ///
    /// Returns `false` if the signature field is missing.
    pub fn verify_multi_sig_shares(
        &mut self,
        required_percentile: u8,
        share_map: ShareMap,
    ) -> bool {
        let message = assemble_sign_message(&self);

        match self.signature {
            Some(Signature::Normal(_)) => {
                panic!("Calling this function on a transaction with a normal signature is not permitted!");
            }
            Some(Signature::MultiSig(ref sig)) => {
                sig.verify_shares(&message, required_percentile, share_map)
            }
            None => false,
        }
    }

    /// Serializes the transaction struct to a binary format.
    ///
    /// Fields:
    /// 1) Transaction type(3)      - 8bits
    /// 2) Amount length            - 8bits
    /// 3) Fee length               - 8bits
    /// 4) Signature length         - 16bits
    /// 5) From                     - 33byte binary
    /// 6) To                       - 33byte binary
    /// 7) Currency hash            - 32byte binary
    /// 8) Fee hash                 - 32byte binary
    /// 9) Hash                     - 32byte binary
    /// 10) Signature               - Binary of signature length
    /// 11) Amount                  - Binary of amount length
    /// 12) Fee                     - Binary of fee length
    pub fn to_bytes(&self) -> Result<Vec<u8>, &'static str> {
        let mut buffer: Vec<u8> = Vec::new();
        let tx_type: u8 = Self::TX_TYPE;

        let hash = if let Some(hash) = &self.hash {
            &hash.0
        } else {
            return Err("Hash field is missing");
        };

        let signature = if let Some(signature) = &self.signature {
            signature.to_bytes()
        } else {
            return Err("Signature field is missing");
        };

        let from = &self.from.to_bytes();
        let to = &self.to.to_bytes();
        let fee_hash = &&self.fee_hash.0;
        let asset_hash = &&self.asset_hash.0;
        let amount = &self.amount.to_bytes();
        let fee = &self.fee.to_bytes();

        let fee_len = fee.len();
        let amount_len = amount.len();
        let signature_len = signature.len();

        buffer.write_u8(tx_type).unwrap();
        buffer.write_u8(amount_len as u8).unwrap();
        buffer.write_u8(fee_len as u8).unwrap();
        buffer.write_u16::<BigEndian>(signature_len as u16).unwrap();

        buffer.append(&mut from.to_vec());
        buffer.append(&mut to.to_vec());
        buffer.append(&mut asset_hash.to_vec());
        buffer.append(&mut fee_hash.to_vec());
        buffer.append(&mut hash.to_vec());
        buffer.append(&mut signature.to_vec());
        buffer.append(&mut amount.to_vec());
        buffer.append(&mut fee.to_vec());

        Ok(buffer)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Send, &'static str> {
        let mut rdr = Cursor::new(bytes.to_vec());
        let tx_type = if let Ok(result) = rdr.read_u8() {
            result
        } else {
            return Err("Bad transaction type");
        };

        if tx_type != Self::TX_TYPE {
            return Err("Bad transation type");
        }

        rdr.set_position(1);

        let amount_len = if let Ok(result) = rdr.read_u8() {
            result
        } else {
            return Err("Bad amount len");
        };

        rdr.set_position(2);

        let fee_len = if let Ok(result) = rdr.read_u8() {
            result
        } else {
            return Err("Bad fee len");
        };

        rdr.set_position(3);

        let signature_len = if let Ok(result) = rdr.read_u16::<BigEndian>() {
            result
        } else {
            return Err("Bad signature len");
        };

        // Consume cursor
        let mut buf = rdr.into_inner();
        let _: Vec<u8> = buf.drain(..5).collect();

        let from = if buf.len() > 33 as usize {
            let from_vec: Vec<u8> = buf.drain(..33).collect();

            match Address::from_bytes(&from_vec) {
                Ok(addr) => addr,
                Err(err) => return Err(err),
            }
        } else {
            return Err("Incorrect packet structure");
        };

        let to = if buf.len() > 33 as usize {
            let to_vec: Vec<u8> = buf.drain(..33).collect();

            match Address::from_bytes(&to_vec) {
                Ok(addr) => addr,
                Err(err) => return Err(err),
            }
        } else {
            return Err("Incorrect packet structure");
        };

        let asset_hash = if buf.len() > 32 as usize {
            let mut hash = [0; 32];
            let hash_vec: Vec<u8> = buf.drain(..32).collect();

            hash.copy_from_slice(&hash_vec);

            Hash(hash)
        } else {
            return Err("Incorrect packet structure");
        };

        let fee_hash = if buf.len() > 32 as usize {
            let mut hash = [0; 32];
            let hash_vec: Vec<u8> = buf.drain(..32).collect();

            hash.copy_from_slice(&hash_vec);

            Hash(hash)
        } else {
            return Err("Incorrect packet structure");
        };

        let hash = if buf.len() > 32 as usize {
            let mut hash = [0; 32];
            let hash_vec: Vec<u8> = buf.drain(..32).collect();

            hash.copy_from_slice(&hash_vec);

            Hash(hash)
        } else {
            return Err("Incorrect packet structure");
        };

        let signature = if buf.len() > signature_len as usize {
            let sig_vec: Vec<u8> = buf.drain(..signature_len as usize).collect();

            match Signature::from_bytes(&sig_vec) {
                Ok(sig) => sig,
                Err(_) => return Err("Bad signature"),
            }
        } else {
            return Err("Incorrect packet structure");
        };

        let amount = if buf.len() > amount_len as usize {
            let amount_vec: Vec<u8> = buf.drain(..amount_len as usize).collect();

            match Balance::from_bytes(&amount_vec) {
                Ok(result) => result,
                Err(_) => return Err("Bad amount"),
            }
        } else {
            return Err("Incorrect packet structure");
        };

        let fee = if buf.len() == fee_len as usize {
            let fee_vec: Vec<u8> = buf.drain(..fee_len as usize).collect();

            match Balance::from_bytes(&fee_vec) {
                Ok(result) => result,
                Err(_) => return Err("Bad gas price"),
            }
        } else {
            return Err("Incorrect packet structure");
        };

        let send = Send {
            from: from,
            to: to,
            fee_hash: fee_hash,
            fee: fee,
            amount: amount,
            asset_hash: asset_hash,
            hash: Some(hash),
            signature: Some(signature),
        };

        Ok(send)
    }

    /// Returns a random valid transaction for the provided state.
    pub fn arbitrary_valid(trie: &mut TrieDBMut<BlakeDbHasher, Codec>, sk: Sk) -> Self {
        unimplemented!();
    }

    impl_hash!();
}

fn assemble_hash_message(obj: &Send) -> Vec<u8> {
    let mut signature = if let Some(ref sig) = obj.signature {
        sig.to_bytes()
    } else {
        panic!("Signature field is missing!");
    };

    let mut buf: Vec<u8> = Vec::new();
    let mut from = obj.from.to_bytes();
    let mut to = obj.to.to_bytes();
    let mut amount = obj.amount.to_bytes();
    let mut fee = obj.fee.to_bytes();
    let asset_hash = obj.asset_hash.0;
    let fee_hash = obj.fee_hash.0;

    // Compose data to hash
    buf.append(&mut from);
    buf.append(&mut to);
    buf.append(&mut asset_hash.to_vec());
    buf.append(&mut fee_hash.to_vec());
    buf.append(&mut amount);
    buf.append(&mut fee);
    buf.append(&mut signature);

    buf
}

fn assemble_sign_message(obj: &Send) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut from = obj.from.to_bytes();
    let mut to = obj.to.to_bytes();
    let mut amount = obj.amount.to_bytes();
    let mut fee = obj.fee.to_bytes();
    let asset_hash = obj.asset_hash.0;
    let fee_hash = obj.fee_hash.0;

    // Compose data to sign
    buf.append(&mut from);
    buf.append(&mut to);
    buf.append(&mut asset_hash.to_vec());
    buf.append(&mut fee_hash.to_vec());
    buf.append(&mut amount);
    buf.append(&mut fee);

    buf
}

use quickcheck::Arbitrary;

impl Arbitrary for Send {
    fn arbitrary<G: quickcheck::Gen>(g: &mut G) -> Send {
        Send {
            from: Arbitrary::arbitrary(g),
            to: Arbitrary::arbitrary(g),
            amount: Arbitrary::arbitrary(g),
            fee: Arbitrary::arbitrary(g),
            asset_hash: Arbitrary::arbitrary(g),
            fee_hash: Arbitrary::arbitrary(g),
            hash: Some(Arbitrary::arbitrary(g)),
            signature: Some(Arbitrary::arbitrary(g)),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate test_helpers;

    use super::*;
    use account::{NormalAddress, Shares};
    use crypto::Identity;
    use OpenShares;

    #[test]
    fn apply_it_creates_a_new_account() {
        let id = Identity::new();
        let to_id = Identity::new();
        let from_addr = Address::normal_from_pkey(*id.pkey());
        let to_addr = Address::normal_from_pkey(*to_id.pkey());
        let asset_hash = crypto::hash_slice(b"Test currency");

        let mut db = test_helpers::init_tempdb();
        let mut root = Hash::NULL_RLP;
        let mut trie = TrieDBMut::<BlakeDbHasher, Codec>::new(&mut db, &mut root);

        // Manually initialize sender balance
        test_helpers::init_balance(&mut trie, from_addr.clone(), asset_hash, b"10000.0");

        let amount = Balance::from_bytes(b"100.123").unwrap();
        let fee = Balance::from_bytes(b"10.0").unwrap();

        let mut tx = Send {
            from: from_addr.clone(),
            to: to_addr.clone(),
            amount: amount.clone(),
            fee: fee.clone(),
            asset_hash: asset_hash,
            fee_hash: asset_hash,
            signature: None,
            hash: None,
        };

        tx.sign(id.skey().clone());
        tx.hash();

        // Apply transaction
        tx.apply(&mut trie);

        // Commit changes
        trie.commit();

        let from_nonce_key = format!("{}.n", hex::encode(&from_addr.to_bytes()));
        let to_nonce_key = format!("{}.n", hex::encode(&to_addr.to_bytes()));
        let from_nonce_key = from_nonce_key.as_bytes();
        let to_nonce_key = to_nonce_key.as_bytes();

        let bin_from_nonce = &trie.get(&from_nonce_key).unwrap().unwrap();
        let bin_to_nonce = &trie.get(&to_nonce_key).unwrap().unwrap();

        let bin_asset_hash = asset_hash.to_vec();
        let hex_asset_hash = hex::encode(&bin_asset_hash);

        let sender_balance_key =
            format!("{}.{}", hex::encode(&from_addr.to_bytes()), hex_asset_hash);
        let receiver_balance_key =
            format!("{}.{}", hex::encode(&to_addr.to_bytes()), hex_asset_hash);
        let sender_balance_key = sender_balance_key.as_bytes();
        let receiver_balance_key = receiver_balance_key.as_bytes();

        let sender_balance =
            Balance::from_bytes(&trie.get(&sender_balance_key).unwrap().unwrap()).unwrap();
        let receiver_balance =
            Balance::from_bytes(&trie.get(&receiver_balance_key).unwrap().unwrap()).unwrap();

        // Check nonces
        assert_eq!(bin_from_nonce.to_vec(), vec![0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(bin_to_nonce.to_vec(), vec![0, 0, 0, 0, 0, 0, 0, 0]);

        // Verify that the correct amount of funds have been subtracted from the sender
        assert_eq!(
            sender_balance,
            Balance::from_bytes(b"10000.0").unwrap() - amount.clone() - fee.clone()
        );

        // Verify that the receiver has received the correct amount of funds
        assert_eq!(receiver_balance, amount);
    }

    #[test]
    fn apply_it_sends_to_an_existing_account() {
        let id = Identity::new();
        let to_id = Identity::new();
        let from_addr = Address::normal_from_pkey(*id.pkey());
        let to_addr = Address::normal_from_pkey(*to_id.pkey());
        let asset_hash = crypto::hash_slice(b"Test currency");

        let mut db = test_helpers::init_tempdb();
        let mut root = Hash::NULL_RLP;
        let mut trie = TrieDBMut::<BlakeDbHasher, Codec>::new(&mut db, &mut root);

        // Manually initialize sender and receiver balances
        test_helpers::init_balance(&mut trie, from_addr.clone(), asset_hash, b"10000.0");
        test_helpers::init_balance(&mut trie, to_addr.clone(), asset_hash, b"10.0");

        let amount = Balance::from_bytes(b"100.123").unwrap();
        let fee = Balance::from_bytes(b"10.0").unwrap();

        let mut tx = Send {
            from: from_addr.clone(),
            to: to_addr.clone(),
            amount: amount.clone(),
            fee: fee.clone(),
            asset_hash: asset_hash,
            fee_hash: asset_hash,
            signature: None,
            hash: None,
        };

        tx.sign(id.skey().clone());
        tx.hash();

        // Apply transaction
        tx.apply(&mut trie);

        // Commit changes
        trie.commit();

        let from_nonce_key = format!("{}.n", hex::encode(&from_addr.to_bytes()));
        let to_nonce_key = format!("{}.n", hex::encode(&to_addr.to_bytes()));
        let from_nonce_key = from_nonce_key.as_bytes();
        let to_nonce_key = to_nonce_key.as_bytes();

        let bin_from_nonce = &trie.get(&from_nonce_key).unwrap().unwrap();
        let bin_to_nonce = &trie.get(&to_nonce_key).unwrap().unwrap();

        let bin_asset_hash = asset_hash.to_vec();
        let hex_asset_hash = hex::encode(&bin_asset_hash);

        let sender_balance_key =
            format!("{}.{}", hex::encode(&from_addr.to_bytes()), hex_asset_hash);
        let receiver_balance_key =
            format!("{}.{}", hex::encode(&to_addr.to_bytes()), hex_asset_hash);
        let sender_balance_key = sender_balance_key.as_bytes();
        let receiver_balance_key = receiver_balance_key.as_bytes();

        let sender_balance =
            Balance::from_bytes(&trie.get(&sender_balance_key).unwrap().unwrap()).unwrap();
        let receiver_balance =
            Balance::from_bytes(&trie.get(&receiver_balance_key).unwrap().unwrap()).unwrap();

        // Check nonces
        assert_eq!(bin_from_nonce.to_vec(), vec![0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(bin_to_nonce.to_vec(), vec![0, 0, 0, 0, 0, 0, 0, 0]);

        // Verify that the correct amount of funds have been subtracted from the sender
        assert_eq!(
            sender_balance,
            Balance::from_bytes(b"10000.0").unwrap() - amount.clone() - fee.clone()
        );

        // Verify that the receiver has received the correct amount of funds
        assert_eq!(
            receiver_balance,
            Balance::from_bytes(b"10.0").unwrap() + amount
        );
    }

    #[test]
    fn apply_it_sends_stocks_to_existing() {
        let id = Identity::new();
        let to_id = Identity::new();
        let from_addr = Address::normal_from_pkey(*id.pkey());
        let from_normal_address = NormalAddress::from_pkey(*id.pkey());
        let to_addr = Address::normal_from_pkey(*to_id.pkey());
        let asset_hash = crypto::hash_slice(b"Test currency");

        let mut db = test_helpers::init_tempdb();
        let mut root = Hash::NULL_RLP;
        let mut trie = TrieDBMut::<BlakeDbHasher, Codec>::new(&mut db, &mut root);

        // Manually initialize sender and receiver balances
        test_helpers::init_balance(&mut trie, from_addr.clone(), asset_hash, b"10000.0");
        test_helpers::init_balance(&mut trie, to_addr.clone(), asset_hash, b"10.0");

        let fee = Balance::from_bytes(b"10.0").unwrap();
        let shares = Shares::new(1000, 1000000, 60);
        let mut share_map = ShareMap::new();

        share_map.add_shareholder(from_normal_address.clone(), 1000);

        // Create shares account
        let mut open_shares = OpenShares {
            creator: from_normal_address.clone(),
            share_map: share_map.clone(),
            shares: shares.clone(),
            asset_hash: asset_hash.clone(),
            fee_hash: asset_hash.clone(),
            amount: Balance::from_bytes(b"100.0").unwrap(),
            fee: Balance::from_bytes(b"30.0").unwrap(),
            nonce: 1,
            address: None,
            stock_hash: None,
            signature: None,
            hash: None,
        };

        open_shares.compute_stock_hash();
        open_shares.compute_address();
        open_shares.sign(id.skey().clone());
        open_shares.hash();
        open_shares.apply(&mut trie);

        let mut tx = Send {
            from: from_addr.clone(),
            to: to_addr.clone(),
            amount: Balance::from_bytes(b"100").unwrap(), // Send 100 shares
            fee: fee.clone(),
            asset_hash: open_shares.stock_hash.unwrap(),
            fee_hash: asset_hash,
            signature: None,
            hash: None,
        };

        tx.sign(id.skey().clone());
        tx.hash();

        // Apply transaction
        tx.apply(&mut trie);

        // Commit changes
        trie.commit();

        let addr = open_shares.address.unwrap().to_bytes();
        let addr = hex::encode(addr);

        let share_map_key = format!("{}.sm", addr);
        let share_map_key = share_map_key.as_bytes();

        let from_nonce_key = format!("{}.n", hex::encode(&from_addr.to_bytes()));
        let to_nonce_key = format!("{}.n", hex::encode(&to_addr.to_bytes()));
        let from_nonce_key = from_nonce_key.as_bytes();
        let to_nonce_key = to_nonce_key.as_bytes();

        let bin_from_nonce = &trie.get(&from_nonce_key).unwrap().unwrap();
        let bin_to_nonce = &trie.get(&to_nonce_key).unwrap().unwrap();

        let bin_stock_hash = open_shares.stock_hash.unwrap().to_vec();
        let hex_stock_hash = hex::encode(&bin_stock_hash);

        let sender_balance_key =
            format!("{}.{}", hex::encode(&from_addr.to_bytes()), hex_stock_hash);
        let receiver_balance_key =
            format!("{}.{}", hex::encode(&to_addr.to_bytes()), hex_stock_hash);
        let sender_balance_key = sender_balance_key.as_bytes();
        let receiver_balance_key = receiver_balance_key.as_bytes();

        let sender_balance = trie.get(&sender_balance_key).unwrap().unwrap();
        let sender_balance = decode_be_u32!(&sender_balance).unwrap();
        let receiver_balance = trie.get(&receiver_balance_key).unwrap().unwrap();
        let receiver_balance = decode_be_u32!(&receiver_balance).unwrap();
        let written_share_map = trie.get(&share_map_key).unwrap().unwrap();
        let written_share_map = ShareMap::from_bytes(&written_share_map).unwrap();

        share_map.transfer_shares(&from_addr.unwrap_normal(), &to_addr.unwrap_normal(), 100);

        // Check nonces
        assert_eq!(bin_from_nonce.to_vec(), vec![0, 0, 0, 0, 0, 0, 0, 2]);
        assert_eq!(bin_to_nonce.to_vec(), vec![0, 0, 0, 0, 0, 0, 0, 0]);

        // Verify that the correct amount of stocks have been subtracted from the sender
        assert_eq!(sender_balance, 900);

        // Verify that the receiver has received the correct amount of stocks
        assert_eq!(receiver_balance, 100);

        // Check share map
        assert_eq!(share_map, written_share_map);
    }

    #[test]
    fn apply_it_sends_stocks_to_new() {
        let id = Identity::new();
        let to_id = Identity::new();
        let from_addr = Address::normal_from_pkey(*id.pkey());
        let from_normal_address = NormalAddress::from_pkey(*id.pkey());
        let to_addr = Address::normal_from_pkey(*to_id.pkey());
        let asset_hash = crypto::hash_slice(b"Test currency");

        let mut db = test_helpers::init_tempdb();
        let mut root = Hash::NULL_RLP;
        let mut trie = TrieDBMut::<BlakeDbHasher, Codec>::new(&mut db, &mut root);

        // Manually initialize sender and receiver balances
        test_helpers::init_balance(&mut trie, from_addr.clone(), asset_hash, b"10000.0");

        let fee = Balance::from_bytes(b"10.0").unwrap();
        let shares = Shares::new(1000, 1000000, 60);
        let mut share_map = ShareMap::new();

        share_map.add_shareholder(from_normal_address.clone(), 1000);

        // Create shares account
        let mut open_shares = OpenShares {
            creator: from_normal_address.clone(),
            share_map: share_map.clone(),
            shares: shares.clone(),
            asset_hash: asset_hash.clone(),
            fee_hash: asset_hash.clone(),
            amount: Balance::from_bytes(b"100.0").unwrap(),
            fee: Balance::from_bytes(b"30.0").unwrap(),
            nonce: 1,
            address: None,
            stock_hash: None,
            signature: None,
            hash: None,
        };

        open_shares.compute_stock_hash();
        open_shares.compute_address();
        open_shares.sign(id.skey().clone());
        open_shares.hash();
        open_shares.apply(&mut trie);

        let mut tx = Send {
            from: from_addr.clone(),
            to: to_addr.clone(),
            amount: Balance::from_bytes(b"100").unwrap(), // Send 100 shares
            fee: fee.clone(),
            asset_hash: open_shares.stock_hash.unwrap(),
            fee_hash: asset_hash,
            signature: None,
            hash: None,
        };

        tx.sign(id.skey().clone());
        tx.hash();

        // Apply transaction
        tx.apply(&mut trie);

        // Commit changes
        trie.commit();

        let addr = open_shares.address.unwrap().to_bytes();
        let addr = hex::encode(addr);

        let share_map_key = format!("{}.sm", addr);
        let share_map_key = share_map_key.as_bytes();

        let from_nonce_key = format!("{}.n", hex::encode(&from_addr.to_bytes()));
        let to_nonce_key = format!("{}.n", hex::encode(&to_addr.to_bytes()));
        let from_nonce_key = from_nonce_key.as_bytes();
        let to_nonce_key = to_nonce_key.as_bytes();

        let bin_from_nonce = &trie.get(&from_nonce_key).unwrap().unwrap();
        let bin_to_nonce = &trie.get(&to_nonce_key).unwrap().unwrap();

        let bin_stock_hash = open_shares.stock_hash.unwrap().to_vec();
        let hex_stock_hash = hex::encode(&bin_stock_hash);

        let sender_balance_key =
            format!("{}.{}", hex::encode(&from_addr.to_bytes()), hex_stock_hash);
        let receiver_balance_key =
            format!("{}.{}", hex::encode(&to_addr.to_bytes()), hex_stock_hash);
        let sender_balance_key = sender_balance_key.as_bytes();
        let receiver_balance_key = receiver_balance_key.as_bytes();

        let sender_balance = trie.get(&sender_balance_key).unwrap().unwrap();
        let sender_balance = decode_be_u32!(&sender_balance).unwrap();
        let receiver_balance = trie.get(&receiver_balance_key).unwrap().unwrap();
        let receiver_balance = decode_be_u32!(&receiver_balance).unwrap();
        let written_share_map = trie.get(&share_map_key).unwrap().unwrap();
        let written_share_map = ShareMap::from_bytes(&written_share_map).unwrap();

        share_map.transfer_shares(&from_addr.unwrap_normal(), &to_addr.unwrap_normal(), 100);

        // Check nonces
        assert_eq!(bin_from_nonce.to_vec(), vec![0, 0, 0, 0, 0, 0, 0, 2]);
        assert_eq!(bin_to_nonce.to_vec(), vec![0, 0, 0, 0, 0, 0, 0, 0]);

        // Verify that the correct amount of stocks have been subtracted from the sender
        assert_eq!(sender_balance, 900);

        // Verify that the receiver has received the correct amount of stocks
        assert_eq!(receiver_balance, 100);

        // Check share map
        assert_eq!(share_map, written_share_map);
    }

    quickcheck! {
        fn serialize_deserialize(tx: Send) -> bool {
            tx == Send::from_bytes(&Send::to_bytes(&tx).unwrap()).unwrap()
        }

         fn verify_hash(tx: Send) -> bool {
            let mut tx = tx;

            for _ in 0..3 {
                tx.hash();
            }

            tx.verify_hash()
        }

        fn verify_signature(
            to: Address,
            amount: Balance,
            fee: Balance,
            asset_hash: Hash,
            fee_hash: Hash
        ) -> bool {
            let id = Identity::new();

            let mut tx = Send {
                from: Address::normal_from_pkey(*id.pkey()),
                to: to,
                amount: amount,
                fee: fee,
                asset_hash: asset_hash,
                fee_hash: fee_hash,
                signature: None,
                hash: None
            };

            tx.sign(id.skey().clone());
            tx.verify_sig()
        }

        fn verify_multi_signature(
            to: Address,
            amount: Balance,
            fee: Balance,
            asset_hash: Hash,
            fee_hash: Hash
        ) -> bool {
            let mut ids: Vec<Identity> = (0..30)
                .into_iter()
                .map(|_| Identity::new())
                .collect();

            let creator_id = ids.pop().unwrap();
            let pkeys: Vec<Pk> = ids
                .iter()
                .map(|i| *i.pkey())
                .collect();

            let mut tx = Send {
                from: Address::multi_sig_from_pkeys(&pkeys, *creator_id.pkey(), 4314),
                to: to,
                amount: amount,
                fee: fee,
                asset_hash: asset_hash,
                fee_hash: fee_hash,
                signature: None,
                hash: None
            };

            // Sign using each identity
            for id in ids {
                tx.sign(id.skey().clone());
            }

            tx.verify_multi_sig(10, &pkeys)
        }

        fn verify_multi_signature_shares(
            to: Address,
            amount: Balance,
            fee: Balance,
            asset_hash: Hash,
            fee_hash: Hash
        ) -> bool {
            let mut ids: Vec<Identity> = (0..30)
                .into_iter()
                .map(|_| Identity::new())
                .collect();

            let creator_id = ids.pop().unwrap();
            let pkeys: Vec<Pk> = ids
                .iter()
                .map(|i| *i.pkey())
                .collect();

            let addresses: Vec<NormalAddress> = pkeys
                .iter()
                .map(|pk| NormalAddress::from_pkey(*pk))
                .collect();

            let mut share_map = ShareMap::new();

            for addr in addresses.clone() {
                share_map.add_shareholder(addr, 100);
            }

            let mut tx = Send {
                from: Address::shareholders_from_pkeys(&pkeys, *creator_id.pkey(), 4314),
                to: to,
                amount: amount,
                fee: fee,
                asset_hash: asset_hash,
                fee_hash: fee_hash,
                signature: None,
                hash: None
            };

            // Sign using each identity
            for id in ids {
                tx.sign(id.skey().clone());
            }

            tx.verify_multi_sig_shares(10, share_map)
        }
    }
}
