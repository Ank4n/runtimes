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
//! Delegations are written into the real proxy pallet, merged with any the delegator already has
//! here. The migrated relay-chain deposit (a `ProxyDeposit` hold placed by the accounts stage) is
//! released whole and the entry re-reserved at this chain's rates; the difference stays free in
//! the delegator's hands. Delays arrive in relay-chain blocks and are converted with
//! `Config::RcBlockTimeRatio`. A bad set is rolled back and parked in `FailedProxies` without
//! failing the batch: the migration cannot stop mid-run to deal with it, and the parked entry is
//! what makes it recoverable afterwards.
//!
//! The call that carries a batch over XCM is the stage machine's; [`ProxyReceiver::receive`] is
//! what it invokes.

extern crate alloc;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

use crate::{Config, Event, FailedProxies, HoldReason, Pallet};
use alloc::vec::Vec;
use core::marker::PhantomData;
use frame_support::traits::{
	fungible::{Inspect, InspectHold, Unbalanced, UnbalancedHold},
	tokens::Precision,
	Get, ReservableCurrency,
};
use migrator_types::{with_rollback, PortableProxy};
use sp_runtime::{traits::Zero, DispatchError, SaturatedConversion, Saturating};

const LOG_TARGET: &str = "runtime::ct-migrator";

pub type BalanceOf<T> =
	<<T as Config>::Currency as Inspect<<T as frame_system::Config>::AccountId>>::Balance;
pub type PortableProxyOf<T> = PortableProxy<<T as frame_system::Config>::AccountId>;

/// Why a proxy set could not be integrated. The caller rolls the set back and parks it.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
	/// Releasing the migrated deposit or writing the merged definitions failed.
	FailedToProcessProxy,
}

pub struct ProxyReceiver<T>(PhantomData<T>);

impl<T: Config> ProxyReceiver<T> {
	/// Integrate a batch of migrated proxy sets.
	///
	/// Every set is processed in a transaction of its own: one that fails is rolled back and
	/// parked in `FailedProxies`, the rest of the batch continues.
	// TODO(ahm-v2): `receive_proxies` (call index 6, root) invokes this.
	pub fn receive(proxies: Vec<PortableProxyOf<T>>) {
		let (count_good, count_bad) = Self::receive_batch(
			proxies,
			Self::do_receive_proxy,
			|()| (),
			|proxy, e| {
				log::error!(
					target: LOG_TARGET,
					"Failed to integrate proxies of {:?}: {e:?}; parking them",
					proxy.delegator,
				);
				FailedProxies::<T>::insert(proxy.delegator.clone(), proxy);
			},
		);
		Pallet::<T>::deposit_event(Event::ProxiesReceived { count_good, count_bad });
	}

	/// Run `integrate` over every item in its own storage transaction, counting successes and
	/// handing each failure to `park`. Shared by every receiving stage so all of them isolate
	/// and report failures identically.
	pub(crate) fn receive_batch<I, R, E>(
		items: Vec<I>,
		integrate: impl Fn(&I) -> Result<R, E>,
		mut on_good: impl FnMut(R),
		park: impl Fn(I, E),
	) -> (u32, u32) {
		let (mut count_good, mut count_bad) = (0, 0);
		for item in items {
			match with_rollback(|| integrate(&item)) {
				Ok(r) => {
					count_good += 1;
					on_good(r);
				},
				Err(e) => {
					count_bad += 1;
					park(item, e);
				},
			}
		}
		(count_good, count_bad)
	}

	/// Receive a single proxy set and write it to storage.
	fn do_receive_proxy(proxy: &PortableProxyOf<T>) -> Result<(), Error> {
		// Resize the migrated relay-chain deposit to this chain's rates: release it whole —
		// making it free balance — and re-reserve below only what the recreated entry needs.
		// The difference stays free on this chain, in the delegator's hands.
		let proxy_reason: T::RuntimeHoldReason = HoldReason::ProxyDeposit.into();
		let migrated = <T as Config>::Currency::balance_on_hold(&proxy_reason, &proxy.delegator);
		if !migrated.is_zero() {
			Self::release_hold(&proxy_reason, &proxy.delegator, migrated)
				.map_err(|_| Error::FailedToProcessProxy)?;
		}
		let delay_ratio = T::RcBlockTimeRatio::get().max(1);

		pallet_proxy::Proxies::<T>::try_mutate(&proxy.delegator, |(defs, deposit)| {
			for delegate in proxy.delegates.iter() {
				let def = pallet_proxy::ProxyDefinition {
					delegate: delegate.delegate.clone(),
					proxy_type: delegate.proxy_type.into(),
					delay: (delegate.delay / delay_ratio).saturated_into(),
				};
				if !defs.contains(&def) {
					defs.try_push(def).map_err(|_| Error::FailedToProcessProxy)?;
				}
			}

			// Back the entry at this chain's rates (normally from the released deposit above),
			// topping up whatever is already reserved for pre-existing local proxies. Priced by
			// the proxy pallet itself, so a migrated entry can never diverge from what the
			// pallet would charge.
			let required = pallet_proxy::Pallet::<T>::deposit(defs.len() as u32);
			let top_up = required.saturating_sub(*deposit);
			if !top_up.is_zero() {
				match <T as pallet_proxy::Config>::Currency::reserve(&proxy.delegator, top_up) {
					Ok(()) => *deposit = required,
					// Access outranks the deposit; the entry stays under-backed until the owner
					// tops it up.
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

	/// Move `amount` from a hold back to free balance without the account ever sitting at zero
	/// reserve mid-operation. For the stages that re-attribute a migrated reserve to the pallet
	/// owning the deposit.
	///
	/// `MutateHold::release` decreases the hold first, and if that takes it to zero while the
	/// free part is still sub-ED, pallet-balances dusts the remainder. Deposit holders whose
	/// liquid dust deliberately travelled here alongside the deposit are in exactly that shape,
	/// so the free part is credited *first* and the hold never passes through zero while free
	/// is below ED. The two primitives are mint-and-burn of the same amount, so total issuance
	/// is untouched, just as `release` would be.
	pub fn release_hold(
		reason: &T::RuntimeHoldReason,
		who: &T::AccountId,
		amount: BalanceOf<T>,
	) -> Result<(), DispatchError> {
		<T as Config>::Currency::increase_balance(who, amount, Precision::Exact)?;
		<T as Config>::Currency::decrease_balance_on_hold(reason, who, amount, Precision::Exact)?;
		Ok(())
	}
}
