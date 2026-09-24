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

//! Proxy stage: migrates proxy delegations to the Coretime chain.
//!
//! Every definition whose permission the Coretime chain represents (the runtime's
//! `TryInto<PortableProxyType>`) travels there and is recreated, whatever the delegator's
//! balance. The rest stay here with the recorded deposit clamped to what is still reserved; the
//! deposits themselves moved with the accounts stage. An entry with nothing left to keep, or
//! whose delegator has no account, is removed.
//!
//! Announcements are not migrated. A record whose announcer's account is gone is removed; the
//! others get their recorded deposit clamped the same way.

#[cfg(test)]
mod tests;

use crate::{Config, LOG_TARGET};
use alloc::vec::Vec;
use core::marker::PhantomData;
use frame_support::{storage::with_storage_layer, BoundedVec};
use migrator_types::{
	translate_destination, PortableProxy, PortableProxyDelegate, PortableProxyType,
};
use sp_runtime::{traits::UniqueSaturatedInto, AccountId32, DispatchError};

/// Maximum number of proxy entries processed per relay-chain block. Bounds the unbenchmarked
/// work of one `on_initialize` here and of the resulting batch on the Coretime chain.
pub const MAX_PROXIES_PER_BLOCK: u32 = 100;

type ProxyDefinitionOf<T> = pallet_proxy::ProxyDefinition<
	<T as frame_system::Config>::AccountId,
	<T as pallet_proxy::Config>::ProxyType,
	pallet_proxy::BlockNumberFor<T>,
>;

/// Why a proxy entry could not be migrated. The caller rolls the entry back and skips it.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
	/// More definitions travel than the wire format holds.
	TooManyDelegates,
}

impl From<Error> for DispatchError {
	fn from(e: Error) -> Self {
		DispatchError::Other(match e {
			Error::TooManyDelegates => "TooManyDelegates",
		})
	}
}

/// Everything one block of the stage sends, ready to be shipped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BlockProxies {
	/// Proxy sets to recreate on the Coretime chain.
	pub proxies: Vec<PortableProxy<AccountId32>>,
	/// Where the next block continues from; `None` once the map is exhausted.
	pub last_key: Option<AccountId32>,
}

pub struct ProxyMigrator<T>(PhantomData<T>);

impl<T: Config> ProxyMigrator<T> {
	/// Drop the announcement records of announcers whose accounts migrated away and clamp the
	/// recorded deposit of the others to what is still reserved. Returns the number of records
	/// dropped.
	///
	/// One-shot; the map is small. The caller wraps this in a storage transaction.
	// TODO(ahm-v2): the `ProxyInit` arm of the stage machine runs this once.
	pub fn drain_announcements() -> u32 {
		let mut dropped = 0u32;
		let records: Vec<_> = pallet_proxy::Announcements::<T>::iter().collect();
		for (announcer, (announcements, deposit)) in records {
			match frame_system::Account::<T>::try_get(&announcer) {
				Ok(account) => {
					let backed = deposit.min(account.data.reserved);
					if backed != deposit {
						pallet_proxy::Announcements::<T>::insert(
							&announcer,
							(announcements, backed),
						);
					}
				},
				Err(()) => {
					pallet_proxy::Announcements::<T>::remove(&announcer);
					dropped += 1;
				},
			}
		}
		log::info!(target: LOG_TARGET, "Dropped {dropped} announcement records");
		dropped
	}

	/// Migrate proxy entries until the per-block limit is reached.
	///
	/// The caller wraps this in a storage transaction and ships the result. Each entry is
	/// migrated in a transaction of its own, so one that cannot be is skipped whole.
	// TODO(ahm-v2): the `ProxyOngoing { last_key }` arm runs this once per block and ships
	// `BlockProxies::proxies` in batches.
	pub fn migrate_many(last_key: Option<T::AccountId>) -> BlockProxies {
		// Get iterator starting after last processed key
		let iter = match &last_key {
			Some(last_key) => pallet_proxy::Proxies::<T>::iter_from(
				pallet_proxy::Proxies::<T>::hashed_key_for(last_key),
			),
			None => pallet_proxy::Proxies::<T>::iter(),
		};

		let mut proxies = Vec::new();
		let mut last_key = None;
		let mut processed = 0u32;
		for (who, (defs, deposit)) in iter {
			processed += 1;

			match with_storage_layer(|| {
				Self::migrate_single(&who, defs.into_inner(), deposit).map_err(DispatchError::from)
			}) {
				Ok(Some(proxy)) => proxies.push(proxy),
				Ok(None) => (),
				Err(e) => {
					log::warn!(target: LOG_TARGET, "Skipping proxy entry of {who:?}: {e:?}");
				},
			}

			if processed >= MAX_PROXIES_PER_BLOCK {
				last_key = Some(who);
				break;
			}
		}
		BlockProxies { proxies, last_key }
	}

	/// Migrate a single proxy entry. `Ok(None)` means none of its definitions is portable.
	pub fn migrate_single(
		who: &T::AccountId,
		defs: Vec<ProxyDefinitionOf<T>>,
		deposit: u128,
	) -> Result<Option<PortableProxy<AccountId32>>, Error> {
		// A definition travels if the Coretime chain represents its permission, else it stays.
		// Account ids on the wire are the destination's address for them, see
		// `migrator_types::translate_destination`.
		let mut delegates = Vec::new();
		let mut stay = Vec::new();
		for def in defs {
			let portable: Result<PortableProxyType, _> = def.proxy_type.clone().try_into();
			match portable {
				Ok(proxy_type) => delegates.push(PortableProxyDelegate {
					delegate: translate_destination(&def.delegate),
					proxy_type,
					delay: def.delay.unique_saturated_into(),
				}),
				Err(_) => stay.push(def),
			}
		}

		let proxy = if delegates.is_empty() {
			None
		} else {
			Some(PortableProxy {
				delegator: translate_destination(who),
				delegates: delegates.try_into().map_err(|_| Error::TooManyDelegates)?,
			})
		};

		// Whatever stays keeps a deposit record no larger than what is still reserved.
		if stay.is_empty() {
			pallet_proxy::Proxies::<T>::remove(who);
		} else {
			match frame_system::Account::<T>::try_get(who) {
				Ok(account) => {
					let backed = deposit.min(account.data.reserved);
					let stay: BoundedVec<_, <T as pallet_proxy::Config>::MaxProxies> =
						BoundedVec::truncate_from(stay);
					pallet_proxy::Proxies::<T>::insert(who, (stay, backed));
				},
				Err(()) => pallet_proxy::Proxies::<T>::remove(who),
			}
		}

		// TODO(ahm-v2): definitions that do not travel are dropped with the entry of a delegator
		// whose account is gone. Whether to recreate them on Asset Hub instead, as v1 did, is
		// undecided.
		Ok(proxy)
	}
}
