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

use std::fmt;
use std::hash::{Hash, Hasher};
use multi_sigs::bls::common::SigKey;

pub struct BlsSkey(SigKey);

impl Clone for BlsSkey {
    fn clone(&self) -> BlsSkey {
        BlsSkey::new(SigKey::from_bytes(&self.0.to_bytes()).unwrap())
    }
}

impl PartialEq for BlsSkey {
    fn eq(&self, other: &Self) -> bool {
        crate::hash_slice(&self.0.to_bytes()) == crate::hash_slice(&other.0.to_bytes())
    }
}

impl Eq for BlsSkey { }

impl fmt::Debug for BlsSkey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BlsSkey({})", hex::encode(self.0.to_bytes()))
    }
}

impl Hash for BlsSkey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bytes().hash(state);
    }
}

impl BlsSkey {
    pub fn new(key: SigKey) -> BlsSkey {
        BlsSkey(key)
    }
}