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

//! Unit tests for the proxy stage.

use super::*;
use crate::mock::*;
use frame_support::{assert_ok, hypothetically};
use migrator_types::sibling_account;
use sp_core::H256;

type Migrator = ProxyMigrator<Test>;

fn delegate(
	who: &AccountId32,
	proxy_type: PortableProxyType,
	delay: u32,
) -> PortableProxyDelegate<AccountId32> {
	PortableProxyDelegate { delegate: who.clone(), proxy_type, delay }
}

fn portable(
	delegator: &AccountId32,
	delegates: Vec<PortableProxyDelegate<AccountId32>>,
) -> PortableProxy<AccountId32> {
	PortableProxy { delegator: delegator.clone(), delegates: delegates.try_into().unwrap() }
}

fn entry(who: &AccountId32) -> (Vec<(AccountId32, ProxyType, u64)>, u128) {
	let (defs, deposit) = pallet_proxy::Proxies::<Test>::get(who);
	(defs.into_iter().map(|d| (d.delegate, d.proxy_type, d.delay)).collect(), deposit)
}

/// Deposit the proxy pallet reserves for `n` definitions in this mock: base 40 + 4 per entry.
fn proxy_deposit(n: u128) -> u128 {
	40 + 4 * n
}

#[test]
fn migrated_delegator_sends_portable_defs_and_loses_its_entry() {
	new_test_ext().execute_with(|| {
		let bob = acc(2); // delegator whose balance migrated
		let d1 = acc(21); // delegate with a portable permission
		let d2 = acc(22); // delegate with a permission the Coretime chain does not represent

		// GIVEN a migrated delegator with one portable and one untranslatable definition.
		fund(&bob, 548);
		add_proxy(&bob, &d1, ProxyType::Any, 4);
		add_proxy(&bob, &d2, ProxyType::Staking, 0);
		withdraw(&bob);

		// WHEN the stage runs.
		let block = Migrator::migrate_many(None);

		// THEN the portable definition travels with its delay in relay-chain blocks, and the
		// whole entry is gone: the delegator's account left, so a record here could only claim
		// money that is no longer reserved.
		assert_eq!(
			block,
			BlockProxies {
				proxies: vec![portable(&bob, vec![delegate(&d1, PortableProxyType::Any, 4)])],
				last_key: None,
			}
		);
		assert!(!pallet_proxy::Proxies::<Test>::contains_key(&bob));
	});
}

#[test]
fn shell_drained_delegator_keeps_untranslatable_defs_with_a_clamped_deposit() {
	new_test_ext().execute_with(|| {
		let carol = acc(3); // session-key holder: drained to a shell, record survives
		let d1 = acc(21);
		let d2 = acc(22);

		// GIVEN a shell-drained delegator with one portable and one untranslatable definition.
		fund(&carol, 548);
		add_proxy(&carol, &d1, ProxyType::Any, 0);
		add_proxy(&carol, &d2, ProxyType::Staking, 0);
		drain_to_shell(&carol);
		assert_eq!(reserved(&carol), 0);

		// WHEN the stage runs.
		let block = Migrator::migrate_many(None);

		// THEN the portable definition travels, the untranslatable one stays, and the recorded
		// deposit is clamped to the (zero) reserve so the entry never claims money that is gone.
		assert_eq!(
			block.proxies,
			vec![portable(&carol, vec![delegate(&d1, PortableProxyType::Any, 0)])]
		);
		assert_eq!(entry(&carol), (vec![(d2, ProxyType::Staking, 0)], 0));
	});
}

#[test]
fn delegator_that_stayed_sends_portable_defs_and_keeps_the_rest_fully_backed() {
	new_test_ext().execute_with(|| {
		let dave = acc(4); // delegator the accounts stage left on this chain
		let d1 = acc(21);
		let d2 = acc(22);

		// GIVEN a funded delegator whose deposit is still reserved in full.
		fund(&dave, 548);
		add_proxy(&dave, &d1, ProxyType::Any, 0);
		add_proxy(&dave, &d2, ProxyType::Staking, 0);

		// WHEN the stage runs.
		let block = Migrator::migrate_many(None);

		// THEN the portable definition is recreated on the Coretime chain like anyone else's,
		// and the untranslatable one stays with its deposit record intact: the reserve never
		// left, so there is nothing to clamp.
		assert_eq!(
			block.proxies,
			vec![portable(&dave, vec![delegate(&d1, PortableProxyType::Any, 0)])]
		);
		assert_eq!(entry(&dave), (vec![(d2, ProxyType::Staking, 0)], proxy_deposit(2)));
		assert_eq!(reserved(&dave), proxy_deposit(2));

		// Hypothetically, a below-ED pure proxy with the deposit-free entry v1 left it: the
		// definition travels and the entry, having nothing left to keep, is removed.
		hypothetically!({
			let pure = acc(5);
			force_dust_account(&pure, 4);
			pallet_proxy::Proxies::<Test>::insert(
				&pure,
				(
					BoundedVec::truncate_from(vec![pallet_proxy::ProxyDefinition {
						delegate: d1.clone(),
						proxy_type: ProxyType::Any,
						delay: 0,
					}]),
					0u128,
				),
			);

			assert_eq!(
				Migrator::migrate_many(None).proxies,
				vec![portable(&pure, vec![delegate(&d1, PortableProxyType::Any, 0)])]
			);
			assert!(!pallet_proxy::Proxies::<Test>::contains_key(&pure));
		});
	});
}

#[test]
fn husk_entries_are_removed_and_their_defs_travel() {
	new_test_ext().execute_with(|| {
		let husk = acc(12); // v1 leftover: proxy entry, no account behind it
		let d1 = acc(21);

		// GIVEN an entry whose delegator has no account.
		pallet_proxy::Proxies::<Test>::insert(
			&husk,
			(
				BoundedVec::truncate_from(vec![pallet_proxy::ProxyDefinition {
					delegate: d1.clone(),
					proxy_type: ProxyType::Any,
					delay: 0,
				}]),
				0u128,
			),
		);

		// WHEN the stage runs.
		let block = Migrator::migrate_many(None);

		// THEN the record is cleaned up and its definition still travels.
		assert_eq!(
			block.proxies,
			vec![portable(&husk, vec![delegate(&d1, PortableProxyType::Any, 0)])]
		);
		assert!(!pallet_proxy::Proxies::<Test>::contains_key(&husk));
	});
}

#[test]
fn sovereigns_are_named_by_their_sibling_address_on_the_wire() {
	new_test_ext().execute_with(|| {
		let para = child_sov(2000); // delegator: a parachain's sovereign account
		let controller = child_sov(2001); // delegate: another parachain's sovereign account

		// GIVEN a sovereign delegating to a sovereign, both migrated.
		fund(&para, 548);
		add_proxy(&para, &controller, ProxyType::Any, 0);
		withdraw(&para);

		// WHEN the stage runs.
		let block = Migrator::migrate_many(None);

		// THEN both sides of the delegation carry the address that IS that parachain on the
		// Coretime chain — where the accounts stage sent the delegator's balance.
		assert_eq!(
			block.proxies,
			vec![portable(
				&sibling_account(2000),
				vec![delegate(&sibling_account(2001), PortableProxyType::Any, 0)]
			)]
		);
	});
}

#[test]
fn announcements_of_migrated_announcers_are_dropped_and_the_rest_clamped() {
	new_test_ext().execute_with(|| {
		let frank = acc(6); // delegator
		let eve = acc(5); // announcer whose account migrated
		let ada = acc(13); // announcer who stayed
		let grace = acc(7); // announcer drained to a shell
		let announcement_deposit = 25 + 6; // base + one announcement

		// GIVEN one announcement of each kind.
		fund(&frank, 500);
		for announcer in [&eve, &ada, &grace] {
			add_proxy(&frank, announcer, ProxyType::Any, 0);
			fund(announcer, 500);
			assert_ok!(Proxy::announce(
				RuntimeOrigin::signed(announcer.clone()),
				frank.clone(),
				H256::zero()
			));
			assert_eq!(reserved(announcer), announcement_deposit);
		}
		withdraw(&eve);
		drain_to_shell(&grace);

		// WHEN the announcements are drained.
		assert_eq!(Migrator::drain_announcements(), 1);

		// THEN the migrated announcer's record is gone, the kept announcer's is intact, and the
		// shell's is clamped to its (zero) reserve.
		assert!(!pallet_proxy::Announcements::<Test>::contains_key(&eve));
		assert_eq!(pallet_proxy::Announcements::<Test>::get(&ada).1, announcement_deposit);
		assert_eq!(reserved(&ada), announcement_deposit);
		assert_eq!(pallet_proxy::Announcements::<Test>::get(&grace).1, 0);
	});
}

#[test]
fn migration_is_paged_at_the_block_limit() {
	new_test_ext().execute_with(|| {
		let delegate = acc(200);
		let count = MAX_PROXIES_PER_BLOCK + 1;

		// GIVEN one more migrated delegator than a block takes.
		for n in 1..=count {
			let delegator = acc(n as u8);
			fund(&delegator, 548);
			add_proxy(&delegator, &delegate, ProxyType::Any, 0);
			withdraw(&delegator);
		}

		// WHEN the stage runs block by block.
		let first = Migrator::migrate_many(None);
		assert_eq!(first.proxies.len() as u32, MAX_PROXIES_PER_BLOCK);
		assert!(first.last_key.is_some());

		let second = Migrator::migrate_many(first.last_key);

		// THEN the second block picks up exactly where the first stopped and exhausts the map.
		assert_eq!(second.proxies.len(), 1);
		assert_eq!(second.last_key, None);
		assert_eq!(pallet_proxy::Proxies::<Test>::iter().count(), 0);
	});
}
