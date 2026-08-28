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

//! Unit tests for `pallet-rc2-migrator`.
//!
//! The pallet's contract, in the abstract: drain every account and record on this chain into
//! portable payloads, burn exactly what is shipped, and keep the conservation ledger
//! (`RcMigratedBalance`) exact at every step — a failed send must roll back to a retryable
//! state, never leave balances burned-but-unsent. The tests pin that contract with exact values;
//! the counterpart chains appear only as captured XCM messages.

use crate::{accounts::AccountsMigrator, mock::*, *};
use frame_support::{
	assert_noop, assert_ok, hypothetically,
	traits::{LockableCurrency, OnInitialize, ReservableCurrency, WithdrawReasons},
	weights::Weight,
};
use migrator_types::{PortableProxyDelegate, PortableProxyType};
use runtime_parachains::hrmp as parachains_hrmp;
use sp_core::H256;
use sp_runtime::{traits::BadOrigin, AccountId32};

type Stage = MigrationStageOf<Test>;

fn root() -> RuntimeOrigin {
	RuntimeOrigin::root()
}

/// Execute the next block's `on_initialize` of the migrator.
fn run_block() {
	let now = System::block_number() + 1;
	System::set_block_number(now);
	<Rc2Migrator as OnInitialize<u32>>::on_initialize(now);
}

/// Seed the conservation tracker the way `AccountsInit` does, for tests that drive a stage
/// function directly instead of walking the machine.
fn seed_tracker() {
	RcMigratedBalance::<Test>::put(MigratedBalances {
		kept: total_issuance(),
		..Default::default()
	});
}

// ---------------------------------------------------------------------------
// Expected-reserve indexing
// ---------------------------------------------------------------------------

#[test]
fn build_expected_reserves_indexes_every_deposit_source() {
	new_test_ext().execute_with(|| {
		let alice = acc(1); // parachain manager
		let bob = acc(2); // delegator with a portable (Any) proxy def
		let carol = acc(3); // delegator with only a non-portable (Staking) def
		let dave = acc(4); // multisig depositor
		let eve = acc(5); // proxy announcer
		let frank = acc(6); // delegator eve announces for
		let delegate = acc(7);

		// GIVEN one deposit of every kind the relay chain knows.
		fund(&alice, 1_000);
		register_para(2000, &alice); // 300 recorded + reserved
		open_channel(2000, 2001, 70, 30);
		open_request(2000, 2002, 25);
		fund(&bob, 500);
		add_proxy(&bob, &delegate, ProxyType::Any); // 44 reserved
		fund(&carol, 500);
		add_proxy(&carol, &delegate, ProxyType::Staking); // 44 reserved
		fund(&dave, 500);
		let call = Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![] }));
		assert_ok!(Multisig::as_multi(
			RuntimeOrigin::signed(dave.clone()),
			2,
			vec![eve.clone()],
			None,
			call,
			Weight::zero(),
		)); // 30 base + 2 * 5 factor = 40 reserved
		fund(&frank, 500);
		add_proxy(&frank, &eve, ProxyType::Any);
		fund(&eve, 500);
		assert_ok!(Proxy::announce(RuntimeOrigin::signed(eve.clone()), frank.clone(), H256::zero())); // 25 + 6 = 31 reserved

		// WHEN the index is built.
		let records = AccountsMigrator::<Test>::build_expected_reserves();

		// THEN every source is classified: registrar + HRMP (+ requests) are Coretime-bound,
		// portable proxy deposits travel under their own reason, everything whose purpose ends
		// with this chain is refunded.
		assert_eq!(records, 8, "para + channel + request + 3 proxies + multisig + announcement");
		assert_eq!(ExpectedCtReserve::<Test>::get(&alice), 300);
		assert_eq!(ExpectedCtReserve::<Test>::get(child_sov(2000)), 70 + 25);
		assert_eq!(ExpectedCtReserve::<Test>::get(child_sov(2001)), 30);
		assert_eq!(ExpectedProxyReserve::<Test>::get(&bob), 44);
		assert_eq!(ExpectedProxyReserve::<Test>::get(&frank), 44);
		assert_eq!(ExpectedRefundReserve::<Test>::get(&carol), 44);
		assert_eq!(ExpectedRefundReserve::<Test>::get(&dave), 40);
		assert_eq!(ExpectedRefundReserve::<Test>::get(&eve), 31);
	});
}

// ---------------------------------------------------------------------------
// Single-account withdrawal: the split rule
// ---------------------------------------------------------------------------

fn withdraw(who: &AccountId32) -> Option<accounts::Withdrawal> {
	let info = frame_system::Account::<Test>::get(who);
	AccountsMigrator::<Test>::withdraw_account(who, info).expect("withdrawal must not error")
}

fn ct_holds(w: &accounts::Withdrawal) -> Vec<(PortableHoldReason, u128)> {
	w.ct.as_ref()
		.map(|a| a.holds.iter().map(|h| (h.reason, h.amount)).collect())
		.unwrap_or_default()
}

#[test]
fn withdraw_splits_deposit_buffer_and_teleport() {
	new_test_ext().execute_with(|| {
		let alice = acc(1); // parachain manager, cleanly migrating
		fund(&alice, 1_000);
		register_para(2000, &alice); // free 700, reserved 300
		AccountsMigrator::<Test>::build_expected_reserves();
		let ti_before = total_issuance();

		let w = withdraw(&alice).expect("migrates");

		// Deposit -> CT hold, one buffer of free follows it, the rest teleports to AH.
		assert_eq!(ct_holds(&w), vec![(PortableHoldReason::UnnamedReserve, 300)]);
		assert_eq!(w.ct.as_ref().unwrap().free, 100);
		assert_eq!(w.ah, Some((alice.clone(), 600)));
		// The account is gone and exactly its total was burned.
		assert!(!frame_system::Account::<Test>::contains_key(&alice));
		assert_eq!(total_issuance(), ti_before - 1_000);
	});
}

#[test]
fn withdraw_parks_unattributed_reserve_under_its_own_reason() {
	new_test_ext().execute_with(|| {
		let bob = acc(2); // account with a reserve no pallet's records explain (on-chain anomaly)
		fund(&bob, 600);
		<Balances as ReservableCurrency<AccountId32>>::reserve(&bob, 200).unwrap();
		AccountsMigrator::<Test>::build_expected_reserves();

		let w = withdraw(&bob).expect("migrates");

		assert_eq!(ct_holds(&w), vec![(PortableHoldReason::UnattributedReserve, 200)]);
		assert_eq!(w.ct.as_ref().unwrap().free, 100);
		assert_eq!(w.ah, Some((bob.clone(), 300)));
		assert!(migrator_events()
			.contains(&Event::UnattributedReserve { who: bob.clone(), amount: 200 }));
	});
}

#[test]
fn withdraw_refunds_deposits_whose_purpose_ends_here() {
	new_test_ext().execute_with(|| {
		let carol = acc(3); // delegator with only a Staking def: nothing travels, deposit refunds
		fund(&carol, 500);
		add_proxy(&carol, &acc(7), ProxyType::Staking); // free 456, reserved 44
		AccountsMigrator::<Test>::build_expected_reserves();

		let w = withdraw(&carol).expect("migrates");

		// The refund joins the liquid balance; with no CT-bound hold there is no buffer either.
		assert!(w.ct.is_none());
		assert_eq!(w.ah, Some((carol.clone(), 500)));
		assert!(migrator_events()
			.contains(&Event::DepositRefunded { who: carol.clone(), amount: 44 }));
	});
}

#[test]
fn withdraw_attributes_shortfall_in_priority_order() {
	new_test_ext().execute_with(|| {
		let dave = acc(4); // account whose live reserve under-covers the recorded deposits
		fund(&dave, 200);
		<Balances as ReservableCurrency<AccountId32>>::reserve(&dave, 100).unwrap();
		// Recorded expectations exceed the live 100: CT-bound deposits are made whole first,
		// proxy deposits second, refunds last. (Set directly: only the split math is under test.)
		ExpectedCtReserve::<Test>::insert(&dave, 50);
		ExpectedProxyReserve::<Test>::insert(&dave, 30);
		ExpectedRefundReserve::<Test>::insert(&dave, 40);

		let w = withdraw(&dave).expect("migrates");

		assert_eq!(
			ct_holds(&w),
			vec![
				(PortableHoldReason::UnnamedReserve, 50),
				(PortableHoldReason::ProxyDeposit, 30),
			]
		);
		// Of the refundable 40 only 20 reserve was left; it becomes liquid.
		assert!(migrator_events()
			.contains(&Event::DepositRefunded { who: dave.clone(), amount: 20 }));
		// liquid = 100 free + 20 refunded; buffer 100 stays with the deposit, 20 teleports.
		assert_eq!(w.ct.as_ref().unwrap().free, 100);
		assert_eq!(w.ah, Some((dave.clone(), 20)));
	});
}

#[test]
fn withdraw_routes_never_signed_any_delegators_wholly_to_ct() {
	new_test_ext().execute_with(|| {
		let pure = acc(30); // keyless pure proxy: nonce 0, Any def
		let delegate = acc(31);
		fund(&pure, 544);
		add_proxy(&pure, &delegate, ProxyType::Any); // free 500, reserved 44, nonce still 0
		AccountsMigrator::<Test>::build_expected_reserves();

		let w = withdraw(&pure).expect("migrates");

		// Funds follow control: everything goes where the definitions are recreated.
		assert_eq!(ct_holds(&w), vec![(PortableHoldReason::ProxyDeposit, 44)]);
		assert_eq!(w.ct.as_ref().unwrap().free, 500);
		assert_eq!(w.ah, None);

		// A never-signed delegator WITHOUT an Any def is not a pure (a pure created with less
		// has already lost control by construction): the regular split applies.
		hypothetically!({
			let multisigish = acc(32);
			fund(&multisigish, 544);
			add_proxy(&multisigish, &delegate, ProxyType::NonTransfer);
			AccountsMigrator::<Test>::build_expected_reserves();
			let w = withdraw(&multisigish).expect("migrates");
			assert_eq!(w.ct.as_ref().unwrap().free, 100);
			assert_eq!(w.ah, Some((multisigish, 400)));
		});
	});
}

#[test]
fn withdraw_keeps_sub_ah_ed_dust_with_the_deposit() {
	new_test_ext().execute_with(|| {
		let heidi = acc(8); // deposit holder whose teleport remainder would be below AH's ED
		fund(&heidi, 404);
		<Balances as ReservableCurrency<AccountId32>>::reserve(&heidi, 300).unwrap();
		ExpectedCtReserve::<Test>::insert(&heidi, 300);

		let w = withdraw(&heidi).expect("migrates");

		// liquid 104: buffer 100 + remainder 4 < AH ED (5) -> the dust follows the deposit.
		assert_eq!(w.ct.as_ref().unwrap().free, 104);
		assert_eq!(w.ah, None);
	});
}

#[test]
fn can_migrate_keeps_module_below_ed_and_locked_accounts() {
	new_test_ext().execute_with(|| {
		// Module accounts stay for the sweep stage.
		fund(&pot(), 500);
		assert_eq!(withdraw(&pot()), None);
		assert!(frame_system::Account::<Test>::contains_key(&pot()));

		// Below-ED accounts only exist via external provider refs; they are not migrated.
		let dusty = acc(9);
		force_anomalous_account(&dusty, 4, 0, 0);
		assert_eq!(withdraw(&dusty), None);

		// Locks cannot be translated; the account stays behind whole.
		let locked = acc(10);
		fund(&locked, 500);
		<Balances as LockableCurrency<AccountId32>>::set_lock(
			*b"testlock",
			&locked,
			100,
			WithdrawReasons::all(),
		);
		assert_eq!(withdraw(&locked), None);
		assert_eq!(free(&locked), 500);
	});
}

#[test]
fn withdraw_drains_consumer_referenced_accounts_to_shells() {
	new_test_ext().execute_with(|| {
		let ida = acc(11); // validator-like account: session keys hold a consumer reference
		fund(&ida, 1_000);
		<Balances as ReservableCurrency<AccountId32>>::reserve(&ida, 300).unwrap();
		ExpectedCtReserve::<Test>::insert(&ida, 300);
		// The extra reference some pallet (session keys in production) holds on the account.
		frame_system::Pallet::<Test>::inc_consumers(&ida).unwrap();
		let ti_before = total_issuance();

		let w = withdraw(&ida).expect("migrates");

		// The money moves like any other account's...
		assert_eq!(ct_holds(&w), vec![(PortableHoldReason::UnnamedReserve, 300)]);
		assert_eq!(w.ct.as_ref().unwrap().free, 100);
		assert_eq!(w.ah, Some((ida.clone(), 600)));
		// ...but the record survives as a zero-balance shell.
		let info = frame_system::Account::<Test>::get(&ida);
		assert_eq!(info.data.free + info.data.reserved, 0);
		assert_eq!(info.consumers, 1);
		assert_eq!(total_issuance(), ti_before - 1_000);
		assert!(migrator_events()
			.contains(&Event::AccountShellDrained { who: ida.clone(), amount: 1_000 }));
	});
}

#[test]
fn withdraw_translates_child_sovereigns_to_sibling_addresses() {
	new_test_ext().execute_with(|| {
		open_channel(2000, 2001, 70, 30); // funds + reserves on the child sovereigns
		AccountsMigrator::<Test>::build_expected_reserves();

		let w = withdraw(&child_sov(2000)).expect("migrates");

		let ct = w.ct.as_ref().unwrap();
		assert_eq!(ct.who, migrator_types::sibling_account::<AccountId32>(2000));
		assert_eq!(ct_holds(&w), vec![(PortableHoldReason::UnnamedReserve, 70)]);
	});
}

// ---------------------------------------------------------------------------
// Accounts stage: batching, tracker, rollback
// ---------------------------------------------------------------------------

#[test]
fn accounts_stage_tracks_and_sends_exactly_what_it_burns() {
	new_test_ext().execute_with(|| {
		let alice = acc(1); // parachain manager
		fund(&alice, 1_000);
		register_para(2000, &alice);
		let ti_before = total_issuance();

		assert_ok!(Rc2Migrator::force_set_stage(root(), Stage::AccountsInit));
		run_block(); // AccountsInit: seeds tracker, builds the index
		run_block(); // AccountsOngoing: migrates everything and finishes

		assert_eq!(RcMigrationStage::<Test>::get(), Stage::AccountsDone);
		let tracker = RcMigratedBalance::<Test>::get();
		assert_eq!(tracker.ct_reserved, 300);
		assert_eq!(tracker.ct_free, 100);
		assert_eq!(tracker.ah_free, 600);
		assert_eq!(tracker.kept, ti_before - 1_000);
		assert_eq!(total_issuance(), tracker.kept);

		// The messages carry exactly the burned pieces.
		let sent = take_sent_xcm();
		let ct_calls = decode_ct_calls(&sent);
		assert_eq!(
			ct_calls,
			vec![CtMigratorCall::ReceiveAccounts {
				accounts: vec![migrator_types::PortableAccount {
					who: alice.clone(),
					free: 100,
					holds: vec![migrator_types::PortableHold {
						reason: PortableHoldReason::UnnamedReserve,
						amount: 300,
					}]
					.try_into()
					.unwrap(),
				}],
			}]
		);
		assert_eq!(decode_teleports(&sent), vec![vec![(alice, 600)]]);
	});
}

#[test]
fn accounts_stage_rolls_back_whole_block_when_a_send_fails() {
	new_test_ext().execute_with(|| {
		let alice = acc(1);
		fund(&alice, 1_000);
		register_para(2000, &alice);
		AccountsMigrator::<Test>::build_expected_reserves();
		seed_tracker();
		let tracker_before = RcMigratedBalance::<Test>::get();
		let ti_before = total_issuance();

		// WHEN every send fails, the block's work must roll back whole: nothing burned, nothing
		// sent, cursor unchanged — the same range is retried next block.
		FailSends::set(true);
		let result = migrator_types::with_rollback(|| AccountsMigrator::<Test>::migrate_many(None));
		assert!(matches!(result, Err(Error::<Test>::XcmSendFailed)));
		assert_eq!(free(&alice), 700);
		assert_eq!(reserved(&alice), 300);
		assert_eq!(total_issuance(), ti_before);
		assert_eq!(RcMigratedBalance::<Test>::get(), tracker_before);
		assert!(sent_xcm().is_empty(), "a rolled-back block must not leave messages behind");

		// AND the retry succeeds once sending recovers.
		FailSends::set(false);
		let result = migrator_types::with_rollback(|| AccountsMigrator::<Test>::migrate_many(None));
		assert!(matches!(result, Ok(None)));
		assert!(!frame_system::Account::<Test>::contains_key(&alice));
		assert_eq!(decode_ct_calls(&take_sent_xcm()).len(), 1);
	});
}

#[test]
fn accounts_stage_stops_at_the_per_block_limit_and_resumes_from_the_cursor() {
	new_test_ext().execute_with(|| {
		// GIVEN more accounts than one block may process.
		let count = MAX_ACCOUNTS_PER_BLOCK + 20;
		for i in 0..count {
			let mut bytes = [0u8; 32];
			bytes[..4].copy_from_slice(&i.to_le_bytes());
			bytes[4] = 0xAA;
			fund(&AccountId32::new(bytes), 1_000);
		}
		let ti_before = total_issuance();
		seed_tracker();

		let cursor = migrator_types::with_rollback(|| AccountsMigrator::<Test>::migrate_many(None))
			.expect("first block succeeds");
		let cursor = cursor.expect("more accounts remain than the per-block limit");

		let done =
			migrator_types::with_rollback(|| AccountsMigrator::<Test>::migrate_many(Some(cursor)))
				.expect("second block succeeds");
		assert_eq!(done, None, "two blocks cover everything");

		// Every account is gone and the ledger is exact: all free balance teleported.
		assert_eq!(frame_system::Account::<Test>::iter().count(), 0);
		let tracker = RcMigratedBalance::<Test>::get();
		assert_eq!(tracker.ah_free, ti_before);
		assert_eq!(tracker.kept, 0);
		assert_eq!(total_issuance(), 0);
	});
}

// ---------------------------------------------------------------------------
// Proxy stage
// ---------------------------------------------------------------------------

#[test]
fn proxy_stage_sends_portable_defs_and_deletes_migrated_delegators() {
	new_test_ext().execute_with(|| {
		let bob = acc(2); // migrated delegator with one portable and one untranslatable def
		let d1 = acc(21);
		let d2 = acc(22);
		fund(&bob, 548);
		add_proxy(&bob, &d1, ProxyType::Any);
		add_proxy(&bob, &d2, ProxyType::Staking);
		AccountsMigrator::<Test>::build_expected_reserves();
		seed_tracker();
		migrator_types::with_rollback(|| AccountsMigrator::<Test>::migrate_many(None)).unwrap();
		assert!(!frame_system::Account::<Test>::contains_key(&bob));
		take_sent_xcm();

		let done = proxy::ProxyMigrator::<Test>::migrate_many(None).unwrap();
		assert_eq!(done, None);

		// The portable definition travelled; the whole entry is deleted — the delegator's account
		// is gone, so a record here could only claim money that left.
		assert!(!pallet_proxy::Proxies::<Test>::contains_key(&bob));
		assert_eq!(
			decode_ct_calls(&take_sent_xcm()),
			vec![CtMigratorCall::ReceiveProxies {
				proxies: vec![migrator_types::PortableProxy {
					delegator: bob,
					delegates: vec![PortableProxyDelegate {
						delegate: d1,
						proxy_type: PortableProxyType::Any,
						delay: 0,
					}]
					.try_into()
					.unwrap(),
				}],
			}]
		);
	});
}

#[test]
fn proxy_stage_clamps_entries_of_accounts_that_stay() {
	new_test_ext().execute_with(|| {
		let carol = acc(3); // shell-drained delegator (session keys): record stays, money left
		let d1 = acc(21);
		let d2 = acc(22);
		fund(&carol, 548);
		add_proxy(&carol, &d1, ProxyType::Any);
		add_proxy(&carol, &d2, ProxyType::Staking);
		frame_system::Pallet::<Test>::inc_consumers(&carol).unwrap();
		AccountsMigrator::<Test>::build_expected_reserves();
		seed_tracker();
		migrator_types::with_rollback(|| AccountsMigrator::<Test>::migrate_many(None)).unwrap();
		assert_eq!(reserved(&carol), 0, "shell-drained");

		proxy::ProxyMigrator::<Test>::migrate_many(None).unwrap();

		// The untranslatable def stays, but the recorded deposit is clamped to the (zero) reserve
		// so the entry never claims money that is gone.
		let (defs, deposit) = pallet_proxy::Proxies::<Test>::get(&carol);
		assert_eq!(defs.len(), 1);
		assert_eq!(defs[0].proxy_type, ProxyType::Staking);
		assert_eq!(deposit, 0);
	});
}

#[test]
fn proxy_stage_deletes_fundless_husk_entries() {
	new_test_ext().execute_with(|| {
		let husk = acc(12); // v1 leftover: proxy entry, no account behind it
		let d1 = acc(21);
		let def = pallet_proxy::ProxyDefinition {
			delegate: d1.clone(),
			proxy_type: ProxyType::Any,
			delay: 0u32,
		};
		pallet_proxy::Proxies::<Test>::insert(
			&husk,
			(frame_support::BoundedVec::truncate_from(vec![def]), 0u128),
		);

		proxy::ProxyMigrator::<Test>::migrate_many(None).unwrap();

		// The record is cleaned up; its (manager-linked) definition still travels.
		assert!(!pallet_proxy::Proxies::<Test>::contains_key(&husk));
		assert_eq!(decode_ct_calls(&take_sent_xcm()).len(), 1);
	});
}

#[test]
fn announcement_records_of_migrated_announcers_are_dropped() {
	new_test_ext().execute_with(|| {
		let frank = acc(6); // delegator
		let eve = acc(5); // announcer whose account migrates away
		let ada = acc(13); // announcer who stays (locked account)
		fund(&frank, 500);
		add_proxy(&frank, &eve, ProxyType::Any);
		add_proxy(&frank, &ada, ProxyType::Any);
		fund(&eve, 500);
		fund(&ada, 500);
		assert_ok!(Proxy::announce(RuntimeOrigin::signed(eve.clone()), frank.clone(), H256::zero()));
		assert_ok!(Proxy::announce(RuntimeOrigin::signed(ada.clone()), frank.clone(), H256::zero()));
		<Balances as LockableCurrency<AccountId32>>::set_lock(
			*b"testlock",
			&ada,
			100,
			WithdrawReasons::all(),
		);
		AccountsMigrator::<Test>::build_expected_reserves();
		// Announcement deposits are refunds: they teleport to AH with the announcer's balance.
		assert_eq!(ExpectedRefundReserve::<Test>::get(&eve), 31);
		seed_tracker();
		migrator_types::with_rollback(|| AccountsMigrator::<Test>::migrate_many(None)).unwrap();
		assert!(!frame_system::Account::<Test>::contains_key(&eve));

		proxy::ProxyMigrator::<Test>::drain_announcements().unwrap();

		// Migrated announcer: record dropped (deposit was refunded). Kept announcer: record and
		// reserve intact (the proxy deposit itself sits on frank, the delegator).
		assert!(!pallet_proxy::Announcements::<Test>::contains_key(&eve));
		let (_, deposit) = pallet_proxy::Announcements::<Test>::get(&ada);
		assert_eq!(deposit, 31);
		assert_eq!(reserved(&ada), 31);
	});
}

// ---------------------------------------------------------------------------
// Registrar stage
// ---------------------------------------------------------------------------

#[test]
fn registrar_stage_moves_next_free_id_and_drains_records() {
	new_test_ext().execute_with(|| {
		let alice = acc(1);
		fund(&alice, 1_000);
		register_para(2000, &alice); // bumps NextFreeParaId to 2001

		registrar::RegistrarMigrator::<Test>::migrate_init().unwrap();
		assert_eq!(
			decode_ct_calls(&take_sent_xcm()),
			vec![CtMigratorCall::ReceiveRegistrar { paras: vec![], next_free_para_id: Some(2001) }]
		);
		assert_eq!(paras_registrar::NextFreeParaId::<Test>::get(), 0.into(), "killed on this side");

		let done = registrar::RegistrarMigrator::<Test>::migrate_many(None).unwrap();
		assert_eq!(done, None);
		assert!(paras_registrar::Paras::<Test>::iter().next().is_none());
		assert_eq!(
			decode_ct_calls(&take_sent_xcm()),
			vec![CtMigratorCall::ReceiveRegistrar {
				paras: vec![migrator_types::PortableParaInfo {
					para_id: 2000,
					manager: alice,
					deposit: 300,
					locked: None,
					// The test para has a registrar record but was never onboarded, so it has no
					// lifecycle and no head data — it travels as a reserved id.
					registered: false,
					head_len: 0,
				}],
				next_free_para_id: None,
			}]
		);
	});
}

// ---------------------------------------------------------------------------
// HRMP stage
// ---------------------------------------------------------------------------

#[test]
fn hrmp_stage_drains_requests_and_channels() {
	new_test_ext().execute_with(|| {
		open_channel(2000, 2001, 70, 30);
		open_request(2000, 2002, 25);

		hrmp::HrmpMigrator::<Test>::drain_open_requests().unwrap();
		assert!(parachains_hrmp::HrmpOpenChannelRequests::<Test>::iter().next().is_none());
		assert!(parachains_hrmp::HrmpOpenChannelRequestsList::<Test>::get().is_empty());
		assert_eq!(
			parachains_hrmp::HrmpOpenChannelRequestCount::<Test>::get(ParaId::from(2000)),
			0
		);
		assert_eq!(
			decode_ct_calls(&take_sent_xcm()),
			vec![CtMigratorCall::ReceiveHrmpRequests {
				requests: vec![migrator_types::PortableHrmpRequest {
					sender: 2000,
					recipient: 2002,
					confirmed: false,
					sender_deposit: 25,
					max_message_size: 1024,
					max_capacity: 8,
					max_total_size: 4096,
				}],
			}]
		);
		assert!(migrator_events().contains(&Event::HrmpRequestsSent { count: 1 }));

		let done = hrmp::HrmpMigrator::<Test>::migrate_many(None).unwrap();
		assert_eq!(done, None);
		assert!(parachains_hrmp::HrmpChannels::<Test>::iter().next().is_none());
		assert_eq!(
			decode_ct_calls(&take_sent_xcm()),
			vec![CtMigratorCall::ReceiveHrmp {
				channels: vec![migrator_types::PortableHrmpChannel {
					sender: 2000,
					recipient: 2001,
					max_capacity: 8,
					max_total_size: 4096,
					max_message_size: 1024,
					sender_deposit: 70,
					recipient_deposit: 30,
				}],
			}]
		);
	});
}

// ---------------------------------------------------------------------------
// Sweep and TI correction
// ---------------------------------------------------------------------------

#[test]
fn sweep_empties_pots_reaps_dust_and_teleports_to_the_beneficiary() {
	new_test_ext().execute_with(|| {
		let dusty = acc(9); // reapable below-ED account
		let backed_dust = acc(14); // below-ED with a broken reserve (holds a consumer ref)
		let husk = acc(15); // zero balance, alive only via a stale provider ref
		let modl_dust = {
			let mut bytes = [0u8; 32];
			bytes[..8].copy_from_slice(b"modlxyz\0");
			AccountId32::new(bytes)
		};
		fund(&pot(), 500);
		// The treasury pot's balance is book-kept as inactive; the sweep must reactivate it.
		pallet_balances::InactiveIssuance::<Test>::put(500);
		force_anomalous_account(&dusty, 4, 0, 0);
		force_anomalous_account(&backed_dust, 2, 3, 1);
		force_anomalous_account(&husk, 0, 0, 0);
		force_anomalous_account(&modl_dust, 4, 0, 0);
		seed_tracker();

		assert_ok!(Rc2Migrator::force_set_stage(root(), Stage::Sweep));
		migrator_events();
		run_block();

		assert_eq!(RcMigrationStage::<Test>::get(), Stage::TiCorrection);
		assert!(!frame_system::Account::<Test>::contains_key(&pot()));
		assert_eq!(pallet_balances::InactiveIssuance::<Test>::get(), 0);
		assert!(!frame_system::Account::<Test>::contains_key(&dusty));
		assert!(!frame_system::Account::<Test>::contains_key(&backed_dust));
		assert!(!frame_system::Account::<Test>::contains_key(&husk));
		// Module accounts are never dust-reaped; the sweep only empties the configured pots.
		assert_eq!(free(&modl_dust), 4);

		let events = migrator_events();
		assert!(events.contains(&Event::AccountSwept { who: pot(), amount: 500 }));
		assert!(events.contains(&Event::DustSwept { count: 2, amount: 4 + 5 }));
		assert!(events.contains(&Event::HusksReaped { count: 1 }));

		// Everything swept teleports to the beneficiary in one message, and the ledger moves
		// with it.
		assert_eq!(decode_teleports(&take_sent_xcm()), vec![vec![(acc(200), 509)]]);
		let tracker = RcMigratedBalance::<Test>::get();
		assert_eq!(tracker.ah_free, 509);
		assert_eq!(total_issuance(), tracker.kept);
	});
}

#[test]
fn ti_correction_burns_the_audited_phantom_and_signals_finish() {
	new_test_ext().execute_with(|| {
		// GIVEN issuance that no account holds (the audited anomaly) and nothing else.
		pallet_balances::TotalIssuance::<Test>::put(50);
		TiCorrection::set(50);
		seed_tracker();

		assert_ok!(Rc2Migrator::force_set_stage(root(), Stage::TiCorrection));
		migrator_events();
		run_block();

		assert!(matches!(RcMigrationStage::<Test>::get(), Stage::CoolOff { .. }));
		assert_eq!(total_issuance(), 0);
		let tracker = RcMigratedBalance::<Test>::get();
		assert_eq!(tracker.ti_corrected, 50);
		assert_eq!(tracker.kept, 0);
		let events = migrator_events();
		assert!(events.contains(&Event::TiCorrected { expected: 50, unaccounted: 50, burned: 50 }));
		assert_eq!(
			decode_ct_calls(&take_sent_xcm()),
			vec![CtMigratorCall::FinishMigration { rc_kept: 0, rc_migrated: 0 }]
		);
	});
}

#[test]
fn ti_correction_never_burns_more_than_measured_and_reports_anomalies() {
	new_test_ext().execute_with(|| {
		// Measured phantom (30) below the audited expectation (50): burn the 30, report loudly.
		pallet_balances::TotalIssuance::<Test>::put(30);
		TiCorrection::set(50);
		seed_tracker();

		assert_ok!(Rc2Migrator::force_set_stage(root(), Stage::TiCorrection));
		migrator_events();
		run_block();

		assert_eq!(total_issuance(), 0);
		let events = migrator_events();
		assert!(events.contains(&Event::TiCorrectionAnomaly { expected: 50, unaccounted: 30 }));
		assert!(events.contains(&Event::TiCorrected { expected: 50, unaccounted: 30, burned: 30 }));

		// Hypothetically, with MORE unaccounted than audited, only the audited amount burns; the
		// excess stays on the books for investigation.
		hypothetically!({
			pallet_balances::TotalIssuance::<Test>::put(80);
			seed_tracker();
			assert_ok!(Rc2Migrator::force_set_stage(root(), Stage::TiCorrection));
			run_block();
			assert_eq!(total_issuance(), 30);
			assert!(migrator_events().contains(&Event::TiCorrected {
				expected: 50,
				unaccounted: 80,
				burned: 50,
			}));
		});
	});
}

// ---------------------------------------------------------------------------
// The whole machine
// ---------------------------------------------------------------------------

#[test]
fn full_stage_machine_drains_the_chain_to_zero() {
	new_test_ext().execute_with(|| {
		let alice = acc(1); // parachain manager
		let pure = acc(30); // keyless pure delegator
		let delegate = acc(31);
		let dusty = acc(9); // reapable dust
		fund(&alice, 1_000);
		register_para(2000, &alice);
		open_channel(2000, 2001, 70, 30);
		open_request(2000, 2002, 25);
		fund(&pure, 544);
		add_proxy(&pure, &delegate, ProxyType::Any);
		fund(&pot(), 500);
		force_anomalous_account(&dusty, 4, 0, 0);
		// The audited phantom issuance.
		pallet_balances::TotalIssuance::<Test>::mutate(|ti| *ti += 50);
		TiCorrection::set(50);
		let ti_start = total_issuance();

		// WHEN the machine runs from Scheduled to Done, one stage per block.
		assert_ok!(Rc2Migrator::force_set_stage(root(), Stage::Scheduled { start: 2 }));
		for _ in 0..40 {
			if RcMigrationStage::<Test>::get().is_finished() {
				break;
			}
			run_block();
		}

		// THEN it finishes on schedule: 15 working blocks + the cool-off window.
		assert_eq!(RcMigrationStage::<Test>::get(), Stage::MigrationDone);
		assert_eq!(System::block_number(), 16 + COOL_OFF_BLOCKS);

		// Every record is drained...
		assert!(paras_registrar::Paras::<Test>::iter().next().is_none());
		assert!(parachains_hrmp::HrmpChannels::<Test>::iter().next().is_none());
		assert!(parachains_hrmp::HrmpOpenChannelRequests::<Test>::iter().next().is_none());
		assert!(pallet_proxy::Proxies::<Test>::iter().next().is_none());
		// ...every account is gone...
		assert_eq!(frame_system::Account::<Test>::iter().count(), 0);
		// ...and the ledger balances to zero, exactly.
		let tracker = RcMigratedBalance::<Test>::get();
		assert_eq!(
			tracker.kept +
				tracker.ct_reserved +
				tracker.ct_free +
				tracker.ah_free +
				tracker.ti_corrected,
			ti_start,
			"conservation is exact"
		);
		assert_eq!(tracker.kept, 0, "the relay chain drains to exactly zero");
		assert_eq!(total_issuance(), 0);
		assert_eq!(tracker.ti_corrected, 50);
	});
}

#[test]
fn force_set_stage_requires_root() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			Rc2Migrator::force_set_stage(RuntimeOrigin::signed(acc(1)), Stage::Paused),
			BadOrigin
		);
		assert_ok!(Rc2Migrator::force_set_stage(root(), Stage::Paused));
		assert_eq!(RcMigrationStage::<Test>::get(), Stage::Paused);
		// Paused halts the machine: blocks pass, nothing moves.
		run_block();
		assert_eq!(RcMigrationStage::<Test>::get(), Stage::Paused);
	});
}
