// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Registrar stage: drains `paras_registrar::Paras` records from the relay chain and sends them
//! to the Coretime chain in portable format.
//!
//! The registration deposits themselves already moved during the accounts stage (as
//! `RcMigratedReserve` holds on the manager accounts); the receiving side re-attributes them to
//! `RegistrarDeposit` holds as each record arrives. `NextFreeParaId` moves in the stage-init
//! message; `PendingSwap` is deliberately left behind.

use crate::*;

pub struct RegistrarMigrator<T>(PhantomData<T>);

impl<T: Config> RegistrarMigrator<T> {
	/// Drain registrar records until the per-block limit is reached.
	///
	/// Returns the cursor to continue from on the next block, or `None` once the map is
	/// exhausted. The caller wraps this in a storage transaction; an `Err` rolls back the whole
	/// block's removals.
	pub fn migrate_many(last_key: Option<ParaId>) -> Result<Option<ParaId>, Error<T>> {
		let mut iter = match last_key {
			Some(last_key) => paras_registrar::Paras::<T>::iter_from(
				paras_registrar::Paras::<T>::hashed_key_for(last_key),
			),
			None => paras_registrar::Paras::<T>::iter(),
		};

		let mut batch = Vec::new();
		let mut processed = 0u32;
		let maybe_last_key = loop {
			let Some((para_id, info)) = iter.next() else { break None };
			processed += 1;

			// Removing the current key while iterating a map is sound; the record is drained,
			// not copied — the registrar ceases to exist on the RC.
			paras_registrar::Paras::<T>::remove(para_id);
			batch.push(PortableParaInfo {
				para_id: para_id.into(),
				manager: info.manager,
				deposit: info.deposit,
				locked: info.locked,
			});

			if batch.len() >= MAX_RECORDS_PER_XCM as usize {
				Pallet::<T>::send_registrar(core::mem::take(&mut batch), None)?;
			}
			if processed >= MAX_RECORDS_PER_BLOCK {
				break Some(para_id);
			}
		};

		if !batch.is_empty() {
			Pallet::<T>::send_registrar(batch, None)?;
		}
		Ok(maybe_last_key)
	}
}
