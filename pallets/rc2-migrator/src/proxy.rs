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

//! Proxy stage: migrates the proxy definitions of para-management accounts to the Coretime
//! chain, where para management continues. Runs before the registrar stage so the manager set
//! can still be read from `paras_registrar::Paras`.
//!
//! Scope, per the migration design:
//! - Only **manager-linked** delegators travel: the delegator manages a para, one of its delegates
//!   does, or a definition is of type `ParaRegistration`. Keyless (pure) managers can only ever act
//!   on the Coretime chain through definitions recreated there; for everyone else the recreation is
//!   a harmless convenience.
//! - Only definitions whose permission the Coretime chain represents travel (the runtime's
//!   `TryInto<PortableProxyType>`); everything else stays on this chain.
//! - Deposits do not travel: they were refunded by the accounts stage. Entries left behind get
//!   their recorded deposit clamped to what is still actually reserved, so no ghost deposit records
//!   are created.

use crate::*;
use alloc::collections::BTreeSet;
use sp_runtime::traits::UniqueSaturatedInto;

pub struct ProxyMigrator<T>(PhantomData<T>);

impl<T: Config> ProxyMigrator<T> {
	/// Migrate proxy definitions until the per-block limit is reached.
	///
	/// Returns the cursor to continue from on the next block, or `None` once the map is
	/// exhausted. The caller wraps this in a storage transaction; an `Err` rolls back the whole
	/// block's changes.
	pub fn migrate_many(last_key: Option<T::AccountId>) -> Result<Option<T::AccountId>, Error<T>> {
		// Rebuilt every block instead of persisted: `Paras` is still intact (the registrar
		// stage runs after this one) and small.
		let managers: BTreeSet<T::AccountId> =
			paras_registrar::Paras::<T>::iter().map(|(_, info)| info.manager).collect();

		let mut iter = match &last_key {
			Some(last_key) => pallet_proxy::Proxies::<T>::iter_from(
				pallet_proxy::Proxies::<T>::hashed_key_for(last_key),
			),
			None => pallet_proxy::Proxies::<T>::iter(),
		};

		let mut batch = Vec::new();
		let mut processed = 0u32;
		let maybe_last_key = loop {
			let Some((who, (defs, deposit))) = iter.next() else { break None };
			processed += 1;

			// Held-back accounts keep their entry untouched: it may be a pure's only control.
			if HeldBackAccounts::<T>::contains_key(&who) {
				if processed >= MAX_RECORDS_PER_BLOCK {
					break Some(who);
				}
				continue;
			}

			// Convert each definition once; `Ok` means the Coretime chain represents the
			// permission, `Err` means it must stay here whatever else happens.
			let defs: Vec<_> = defs
				.into_iter()
				.map(|def| {
					let portable: Result<PortableProxyType, ()> =
						def.proxy_type.clone().try_into().map_err(|_| ());
					(def, portable)
				})
				.collect();

			// Split the definitions into what travels and what stays.
			let manager_linked = managers.contains(&who) ||
				defs.iter().any(|(def, portable)| {
					managers.contains(&def.delegate) ||
						*portable == Ok(PortableProxyType::ParaRegistration)
				});
			let (travel, stay): (Vec<_>, Vec<_>) =
				defs.into_iter().partition(|(_, portable)| manager_linked && portable.is_ok());

			if !travel.is_empty() {
				let delegates: Vec<_> = travel
					.into_iter()
					.map(|(def, portable)| migrator_types::PortableProxyDelegate {
						delegate: def.delegate,
						proxy_type: portable.expect("partition kept only Ok conversions; qed"),
						delay: def.delay.unique_saturated_into(),
					})
					.collect();
				batch.push(PortableProxy {
					delegator: who.clone(),
					delegates: delegates
						.try_into()
						.map_err(|_| Error::<T>::FailedToWithdrawAccount)?,
				});
			}

			// A delegator with no funds has nothing that could strand: its manager-linked
			// definitions were sent above, the rest of the entry is deleted — this cleans the
			// zero-balance husks v1 left behind. For funded delegators, the deposit was refunded
			// by the accounts stage; clamp the recorded field to what is still actually reserved
			// so the entry never claims money that is gone.
			let stay: Vec<_> = stay.into_iter().map(|(def, _)| def).collect();
			match frame_system::Account::<T>::try_get(&who) {
				Ok(account) if !stay.is_empty() => {
					let backed = deposit.min(account.data.reserved);
					let stay: BoundedVec<_, <T as pallet_proxy::Config>::MaxProxies> =
						stay.try_into().expect("subset of a bounded vec; qed");
					pallet_proxy::Proxies::<T>::insert(&who, (stay, backed));
				},
				_ => pallet_proxy::Proxies::<T>::remove(&who),
			}

			if batch.len() >= MAX_RECORDS_PER_XCM as usize {
				Pallet::<T>::send_proxies(core::mem::take(&mut batch))?;
			}
			if processed >= MAX_RECORDS_PER_BLOCK {
				break Some(who);
			}
		};

		if !batch.is_empty() {
			Pallet::<T>::send_proxies(batch)?;
		}
		Ok(maybe_last_key)
	}
}
