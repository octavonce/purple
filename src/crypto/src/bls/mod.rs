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

use multi_sigs::bls::common::{SigKey, VerKey, Keypair};

pub mod pkey;
pub mod skey;

use pkey::*;
use skey::*;

pub fn gen_bls_keypair() -> (BlsPkey, BlsSkey) {
    let keypair = Keypair::new(None);
    (BlsPkey::new(keypair.ver_key), BlsSkey::new(keypair.sig_key))
}