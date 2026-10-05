// Copyright (C) Polkadot Fellows.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot. If not, see <http://www.gnu.org/licenses/>.

//! Helpers for the runtimes' tests.

use alloc::{vec, vec::Vec};
use codec::Decode;
use scale_info::{TypeDef, TypeInfo};

/// Every call of a runtime by pallet and call name, decoded from zero bytes. A call whose
/// arguments do not decode from zeros is left out.
pub fn every_call<Call: TypeInfo + Decode>() -> Vec<(&'static str, &'static str, Call)> {
	let TypeDef::Variant(pallets) = Call::type_info().type_def else {
		panic!("the runtime call is an enum")
	};
	let mut calls = vec![];
	for pallet in pallets.variants {
		let TypeDef::Variant(variants) = pallet.fields[0].ty.type_info().type_def else {
			panic!("a pallet's calls are an enum")
		};
		for variant in variants.variants {
			let mut bytes = [0u8; 1026];
			bytes[0] = pallet.index;
			bytes[1] = variant.index;
			if let Ok(call) = Call::decode(&mut &bytes[..]) {
				calls.push((pallet.name, variant.name, call));
			}
		}
	}
	calls
}
