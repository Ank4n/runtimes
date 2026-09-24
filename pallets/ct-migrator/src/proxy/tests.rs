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

//! Unit tests for the receiving side of the proxy stage.

use super::*;
use crate::mock::*;
use frame_support::{assert_ok, traits::fungible::Mutate};
use migrator_types::PortableProxyType;

type Receiver = ProxyReceiver<Test>;

/// Deposit the proxy pallet reserves for `n` definitions in this mock: base 100 + 20 per entry.
fn proxy_deposit(n: u128) -> u128 {
	100 + 20 * n
}

fn entry(who: &AccountId) -> (Vec<(AccountId, ProxyType, u64)>, u128) {
	let (defs, deposit) = pallet_proxy::Proxies::<Test>::get(who);
	(defs.into_iter().map(|d| (d.delegate, d.proxy_type, d.delay)).collect(), deposit)
}

#[test]
fn receive_recreates_defs_and_resizes_the_deposit_to_local_rates() {
	new_test_ext().execute_with(|| {
		let pure = acc(10); // keyless delegator; its deposit arrived as a ProxyDeposit hold
		let controller = acc(11); // its delegate

		// GIVEN the delegator as the accounts stage left it.
		give_proxy_deposit(&pure, 400, ED);
		let ti_before = total_issuance();

		// WHEN its delegation arrives with a 4-block relay-chain delay.
		Receiver::receive(vec![portable_proxy(
			&pure,
			vec![(controller, PortableProxyType::Any, 4)],
		)]);

		// THEN the definition exists in the real proxy pallet with the delay converted to this
		// chain's block time (4 relay blocks -> 2 local blocks).
		assert_eq!(entry(&pure), (vec![(controller, ProxyType::Any, 2)], proxy_deposit(1)));

		// AND the migrated deposit was resized to local rates: base 100 + factor 20 reserved,
		// the remainder released to the delegator as free balance. Issuance is untouched.
		assert_eq!(reserved(&pure), proxy_deposit(1));
		assert_eq!(held(HoldReason::ProxyDeposit, &pure), 0);
		assert_eq!(free(&pure), ED + 400 - proxy_deposit(1));
		assert_eq!(total_issuance(), ti_before);
		assert_eq!(migrator_events(), vec![Event::ProxiesReceived { count_good: 1, count_bad: 0 }]);
	});
}

#[test]
fn sub_ed_free_survives_the_deposit_resize() {
	new_test_ext().execute_with(|| {
		let pure = acc(10); // keyless delegator with sub-ED liquid dust and a proxy deposit
		let controller = acc(11);

		// GIVEN 3 planck of free balance beside the 400 hold.
		give_proxy_deposit(&pure, 400, 3);

		// WHEN its delegation arrives.
		Receiver::receive(vec![portable_proxy(
			&pure,
			vec![(controller, PortableProxyType::Any, 0)],
		)]);

		// THEN the resize releases the whole migrated hold and re-reserves 120 at local rates;
		// the 3-planck dust rides along instead of burning while the hold is momentarily empty.
		assert_eq!(reserved(&pure), proxy_deposit(1));
		assert_eq!(free(&pure), 3 + 400 - proxy_deposit(1));
		assert_eq!(total_issuance(), 403);
	});
}

#[test]
fn delay_conversion_rounds_up() {
	new_test_ext().execute_with(|| {
		let delegator = acc(10);
		let d1 = acc(11);
		let d2 = acc(12);

		Receiver::receive(vec![portable_proxy(
			&delegator,
			vec![(d1, PortableProxyType::Any, 3), (d2, PortableProxyType::Any, 1)],
		)]);

		// ceil(3 / 2) = 2 and ceil(1 / 2) = 1: a delayed proxy stays delayed, so the
		// announcement requirement is never lost in the conversion.
		assert_eq!(entry(&delegator).0, vec![(d1, ProxyType::Any, 2), (d2, ProxyType::Any, 1)]);
	});
}

#[test]
fn receive_merges_with_existing_local_defs_sorted_and_dedups() {
	new_test_ext().execute_with(|| {
		let dan = acc(10); // delegator with a pre-existing local proxy
		let local = acc(12); // local delegate, added before the migration reaches this chain
		let migrated = acc(11); // delegate arriving from the relay chain; sorts before `local`

		// GIVEN a local entry priced at local rates.
		<Balances as Mutate<AccountId>>::mint_into(&dan, 1_000).unwrap();
		assert_ok!(Proxy::add_proxy(RuntimeOrigin::signed(dan), local, ProxyType::Any, 0));
		assert_eq!(reserved(&dan), proxy_deposit(1));

		// WHEN a migrated set arrives containing a new delegate AND a duplicate of the local one.
		Receiver::receive(vec![portable_proxy(
			&dan,
			vec![(migrated, PortableProxyType::Any, 0), (local, PortableProxyType::Any, 0)],
		)]);

		// THEN the duplicate is not re-added, the merged vec is sorted the way the pallet keeps
		// it, and the deposit tops up to the 2-def requirement.
		assert_eq!(
			entry(&dan),
			(vec![(migrated, ProxyType::Any, 0), (local, ProxyType::Any, 0)], proxy_deposit(2))
		);
		assert_eq!(reserved(&dan), proxy_deposit(2));

		// AND the pallet's own binary search still finds the local definition.
		assert_ok!(Proxy::remove_proxy(RuntimeOrigin::signed(dan), local, ProxyType::Any, 0));
		assert_eq!(entry(&dan), (vec![(migrated, ProxyType::Any, 0)], proxy_deposit(1)));
	});
}

#[test]
fn receive_writes_the_entry_even_when_the_deposit_cannot_be_reserved() {
	new_test_ext().execute_with(|| {
		let broke = acc(10); // delegator that arrived with no balance at all
		let delegate = acc(11);

		Receiver::receive(vec![portable_proxy(
			&broke,
			vec![(delegate, PortableProxyType::NonTransfer, 0)],
		)]);

		// Access outranks the deposit: the entry exists, under-backed, and nothing failed.
		assert_eq!(entry(&broke), (vec![(delegate, ProxyType::NonTransfer, 0)], 0));
		assert_eq!(migrator_events(), vec![Event::ProxiesReceived { count_good: 1, count_bad: 0 }]);
	});
}

#[test]
fn overflowing_merged_set_is_parked_and_rolled_back() {
	new_test_ext().execute_with(|| {
		let max = acc(10); // delegator already at MaxProxies (= 4 in this mock)

		// GIVEN a full local entry and a migrated deposit waiting to be resized.
		<Balances as Mutate<AccountId>>::mint_into(&max, 10_000).unwrap();
		for i in 41..45u8 {
			assert_ok!(Proxy::add_proxy(RuntimeOrigin::signed(max), acc(i), ProxyType::Any, 0));
		}
		give_proxy_deposit(&max, 400, 0);
		let before = entry(&max);

		// WHEN one more delegate arrives.
		let overflowing = portable_proxy(&max, vec![(acc(45), PortableProxyType::Any, 0)]);
		Receiver::receive(vec![overflowing.clone()]);

		// THEN the whole item is rolled back — including the hold release — and parked for
		// recovery.
		assert_eq!(FailedProxies::<Test>::get(max), Some(overflowing));
		assert_eq!(held(HoldReason::ProxyDeposit, &max), 400);
		assert_eq!(entry(&max), before);
		assert_eq!(migrator_events(), vec![Event::ProxiesReceived { count_good: 0, count_bad: 1 }]);
	});
}
