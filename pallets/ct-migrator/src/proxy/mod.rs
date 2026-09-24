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

//! Proxy stage, receiving side: recreates the proxy delegations the relay chain sent.
//!
//! Delegations are written into the proxy pallet, merged with any the delegator already has here,
//! whether or not the delegator has an account here. The migrated `ProxyDeposit` hold is released
//! and the entry re-reserved at this chain's rates. An entry whose deposit cannot be reserved is
//! kept under-backed: it can be removed whole, but removing one definition at a time needs the
//! missing deposit first. Delays are converted from relay-chain blocks with
//! `Config::RcBlocksPerLocalBlock`, rounding up. A set that cannot be written is rolled back and
//! parked in `FailedProxies`.

#[cfg(test)]
mod tests;

use crate::{BalanceOf, Config, Event, FailedProxies, HoldReason, Pallet, LOG_TARGET};
use alloc::vec::Vec;
use core::marker::PhantomData;
use frame_support::traits::{
	fungible::{InspectHold, Unbalanced, UnbalancedHold},
	tokens::Precision,
	Get, ReservableCurrency,
};
use migrator_types::PortableProxy;
use sp_runtime::{traits::Zero, DispatchError, SaturatedConversion, Saturating};

pub type PortableProxyOf<T> = PortableProxy<<T as frame_system::Config>::AccountId>;

/// Why a proxy set could not be integrated. The caller rolls the set back and parks it.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
	/// Releasing the migrated deposit or writing the merged definitions failed.
	FailedToProcessProxy,
}

impl From<Error> for DispatchError {
	fn from(e: Error) -> Self {
		DispatchError::Other(match e {
			Error::FailedToProcessProxy => "FailedToProcessProxy",
		})
	}
}

pub struct ProxyReceiver<T>(PhantomData<T>);

impl<T: Config> ProxyReceiver<T> {
	/// Integrate a batch of migrated proxy sets.
	///
	/// Every set is processed in a transaction of its own: one that fails is rolled back and
	/// parked in `FailedProxies`, the rest of the batch continues.
	// TODO(ahm-v2): the root call that receives a batch invokes this.
	pub fn receive(proxies: Vec<PortableProxyOf<T>>) {
		let (count_good, count_bad) = Pallet::<T>::receive_batch(
			proxies,
			|proxy| Self::do_receive_proxy(proxy).map_err(DispatchError::from),
			|()| (),
			|proxy, e| {
				log::error!(
					target: LOG_TARGET,
					"Failed to integrate proxies of {:?}: {e:?}; parking them",
					proxy.delegator,
				);
				FailedProxies::<T>::insert(&proxy.delegator, &proxy);
			},
		);
		Pallet::<T>::deposit_event(Event::ProxiesReceived { count_good, count_bad });
	}

	/// Receive a single proxy set and write it to storage.
	fn do_receive_proxy(proxy: &PortableProxyOf<T>) -> Result<(), Error> {
		// Release the migrated deposit; the entry is re-reserved below at this chain's rates.
		let proxy_reason: T::RuntimeHoldReason = HoldReason::ProxyDeposit.into();
		let migrated = <T as Config>::Currency::balance_on_hold(&proxy_reason, &proxy.delegator);
		if !migrated.is_zero() {
			Self::release_hold(&proxy_reason, &proxy.delegator, migrated)
				.map_err(|_| Error::FailedToProcessProxy)?;
		}
		let delay_ratio = T::RcBlocksPerLocalBlock::get().max(1);

		pallet_proxy::Proxies::<T>::try_mutate(&proxy.delegator, |(defs, deposit)| {
			for delegate in proxy.delegates.iter() {
				let def = pallet_proxy::ProxyDefinition {
					delegate: delegate.delegate.clone(),
					proxy_type: delegate.proxy_type.into(),
					delay: delegate.delay.div_ceil(delay_ratio).saturated_into(),
				};
				// Inserted the way `add_proxy_delegate` does: the pallet binary-searches this
				// vec, so it has to stay sorted.
				if let Err(i) = defs.binary_search(&def) {
					defs.try_insert(i, def).map_err(|_| Error::FailedToProcessProxy)?;
				}
			}

			// Top up the reserve to what the proxy pallet charges for the merged set.
			let required = pallet_proxy::Pallet::<T>::deposit(defs.len() as u32);
			let top_up = required.saturating_sub(*deposit);
			if !top_up.is_zero() {
				match <T as pallet_proxy::Config>::Currency::reserve(&proxy.delegator, top_up) {
					Ok(()) => *deposit = required,
					Err(_) => log::warn!(
						target: LOG_TARGET,
						"Proxies of {:?} under-backed: could not reserve {top_up:?}",
						proxy.delegator,
					),
				}
			}
			Ok(())
		})
	}

	/// Move `amount` from a hold back to free balance, leaving total issuance untouched.
	///
	/// Credits the free part before decreasing the hold. `MutateHold::release` does the reverse,
	/// which dusts a sub-ED free balance once the hold reaches zero.
	fn release_hold(
		reason: &T::RuntimeHoldReason,
		who: &T::AccountId,
		amount: BalanceOf<T>,
	) -> Result<(), DispatchError> {
		<T as Config>::Currency::increase_balance(who, amount, Precision::Exact)?;
		<T as Config>::Currency::decrease_balance_on_hold(reason, who, amount, Precision::Exact)?;
		Ok(())
	}
}
