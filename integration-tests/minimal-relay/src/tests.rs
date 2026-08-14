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

//! Tests for the Minimal Relay migration

use crate::mock::*;
use codec::{Decode, Encode};
use std::collections::BTreeMap;
use xcm::latest::prelude::*;

/// An XCM program that executes `call` on the destination with the sender's sovereign-account
/// origin. Both the RC and the system parachains grant each other unpaid execution, so no fee
/// payment is needed.
fn unpaid_transact<Call: Encode>(call: Call) -> Xcm<()> {
	Xcm(vec![
		UnpaidExecution { weight_limit: Unlimited, check_origin: None },
		Transact {
			origin_kind: OriginKind::SovereignAccount,
			fallback_max_weight: None,
			call: call.encode().into(),
		},
	])
}

// Multi-thread runtimes so snapshot loading/hydration actually runs in parallel; the default
// `#[tokio::test]` runtime is current-thread and would serialize the CPU-bound loads.
#[tokio::test(flavor = "multi_thread")]
async fn three_chains_produce_blocks() {
	let (mut rc, mut ah, mut ct) = load_externalities().await;
	// 10 blocks is enough for the message queues to drain whatever the live snapshot carries;
	// `next_block_*` asserts on every block that nothing fails processing and that the weight
	// stays under 80% of the block limit.
	rc.execute_with(|| {
		for _ in 0..10 {
			next_block_rc();
		}
	});
	ah.execute_with(|| {
		for _ in 0..10 {
			next_block_para::<AssetHubPolkadot>();
		}
	});
	ct.execute_with(|| {
		for _ in 0..10 {
			next_block_para::<CoretimePolkadot>();
		}
	});
}

#[tokio::test(flavor = "multi_thread")]
async fn rc_and_asset_hub_exchange_messages() {
	message_round_trip::<AssetHubPolkadot>().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rc_and_coretime_exchange_messages() {
	message_round_trip::<CoretimePolkadot>().await;
}

/// Assert that a `System::Remarked` event was emitted on runtime `T`.
fn assert_remarked<T: frame_system::Config>(chain: &str)
where
	T::RuntimeEvent: TryInto<frame_system::Event<T>>,
{
	assert!(
		frame_system::Pallet::<T>::events().into_iter().any(|record| matches!(
			record.event.try_into(),
			Ok(frame_system::Event::<T>::Remarked { .. })
		)),
		"remark did not execute on {chain}"
	);
}

/// Sends a `System::remark_with_event` from the RC to `P` and back, asserting on the destination
/// that the remark actually executed.
async fn message_round_trip<P: Para>()
where
	RuntimeCallFor<P>: From<frame_system::Call<P::Runtime>>,
{
	let (rc, para) =
		tokio::join!(tokio::spawn(remote_ext(Chain::Relay)), tokio::spawn(remote_ext(P::CHAIN)),);
	let mut rc = rc.expect("failed to load the Relay Chain snapshot");
	let mut para = para.unwrap_or_else(|_| panic!("failed to load the {} snapshot", P::NAME));

	// RC -> para.
	let dmp = rc.execute_with(|| {
		let call: RuntimeCallFor<P> = frame_system::Call::<P::Runtime>::remark_with_event {
			remark: b"minimal-relay dmp".to_vec(),
		}
		.into();
		send_dmp(P::PARA_ID.into(), unpaid_transact(call));
		next_block_rc();
		take_dmp(P::PARA_ID.into())
	});
	rc.commit_all().unwrap();
	// The live snapshot may have queued unrelated messages for this para, so only assert that
	// ours is among them.
	assert!(!dmp.is_empty(), "RC queued no DMP message for {}", P::NAME);

	para.execute_with(|| {
		enqueue_dmp::<P>(dmp);
		next_block_para::<P>();
		assert_remarked::<P::Runtime>(P::NAME);
	});
	para.commit_all().unwrap();

	// para -> RC.
	let ump = para.execute_with(|| {
		let call: polkadot_runtime::RuntimeCall =
			frame_system::Call::remark_with_event { remark: b"minimal-relay ump".to_vec() }.into();
		send_ump::<P>(unpaid_transact(call));
		take_ump::<P>()
	});
	para.commit_all().unwrap();
	assert!(!ump.is_empty(), "{} queued no UMP message for the RC", P::NAME);

	rc.execute_with(|| {
		enqueue_ump(P::PARA_ID.into(), ump);
		next_block_rc();
		assert_remarked::<polkadot_runtime::Runtime>("the RC");
	});
}

/// One row of the balance census: how many accounts have this kind of balance, and how much.
#[derive(Default)]
struct CensusRow {
	accounts: u32,
	total: u128,
}

impl CensusRow {
	fn add(&mut self, amount: u128) {
		self.accounts += 1;
		self.total += amount;
	}
}

/// Aggregate and print every kind of balance on the chain: free, holds per reason, named and
/// unnamed reserves, locks per id and freezes per id. Reporting only, no assertions — this is
/// the input for deciding what the migration must do with each class of balance.
fn print_balance_census<T>(name: &str)
where
	T: frame_system::Config<AccountData = pallet_balances::AccountData<u128>>
		+ pallet_balances::Config<Balance = u128>,
	<T as pallet_balances::Config>::RuntimeHoldReason: std::fmt::Debug,
	<T as pallet_balances::Config>::FreezeIdentifier: std::fmt::Debug,
	<T as pallet_balances::Config>::ReserveIdentifier: std::fmt::Debug,
{
	let dot = |v: u128| v as f64 / 1e10;
	let mut accounts = 0u32;
	let (mut free, mut reserved, mut frozen, mut unnamed_reserve) =
		(CensusRow::default(), CensusRow::default(), CensusRow::default(), CensusRow::default());
	let mut holds = BTreeMap::<String, CensusRow>::new();
	let mut named_reserves = BTreeMap::<String, CensusRow>::new();
	let mut locks = BTreeMap::<String, CensusRow>::new();
	let mut freezes = BTreeMap::<String, CensusRow>::new();

	for (who, info) in frame_system::Account::<T>::iter() {
		accounts += 1;
		let data = info.data;
		if data.free > 0 {
			free.add(data.free);
		}
		if data.frozen > 0 {
			frozen.add(data.frozen);
		}

		// Reserved splits into holds (attributed in balances itself), named reserves (id but no
		// reason) and the unnamed remainder (old `reserve()` API — attributable only by
		// cross-referencing the pallets that placed the deposits).
		let mut attributed = 0u128;
		for hold in pallet_balances::Holds::<T>::get(&who) {
			holds.entry(format!("hold {:?}", hold.id)).or_default().add(hold.amount);
			attributed += hold.amount;
		}
		for reserve in pallet_balances::Reserves::<T>::get(&who) {
			named_reserves
				.entry(format!("named reserve {:?}", reserve.id))
				.or_default()
				.add(reserve.amount);
			attributed += reserve.amount;
		}
		if data.reserved > 0 {
			reserved.add(data.reserved);
			let unnamed = data.reserved.saturating_sub(attributed);
			if unnamed > 0 {
				unnamed_reserve.add(unnamed);
			}
		}

		for lock in pallet_balances::Locks::<T>::get(&who) {
			locks
				.entry(format!("lock {:?}", String::from_utf8_lossy(&lock.id)))
				.or_default()
				.add(lock.amount);
		}
		for freeze in pallet_balances::Freezes::<T>::get(&who) {
			freezes.entry(format!("freeze {:?}", freeze.id)).or_default().add(freeze.amount);
		}
	}

	let row = |label: &str, r: &CensusRow| {
		println!("| {label} | {} | {:.4} |", r.accounts, dot(r.total));
	};
	println!("### Balance census: {name}");
	println!();
	println!(
		"{accounts} accounts, total issuance {:.4} DOT (inactive: {:.4})",
		dot(pallet_balances::TotalIssuance::<T>::get()),
		dot(pallet_balances::InactiveIssuance::<T>::get()),
	);
	println!();
	println!("| Balance kind | Accounts | DOT |");
	println!("|---|---:|---:|");
	row("free", &free);
	row("reserved (holds + named + unnamed)", &reserved);
	for (label, r) in holds.iter().chain(named_reserves.iter()) {
		row(label, r);
	}
	row("unnamed reserve", &unnamed_reserve);
	// `frozen` is the max of overlapping locks/freezes per account, so rows are not additive.
	row("frozen (not additive with rows below)", &frozen);
	for (label, r) in locks.iter().chain(freezes.iter()) {
		row(label, r);
	}

	// Issuance that no account holds: free + reserved across all accounts versus TotalIssuance.
	let in_accounts = free.total + reserved.total;
	println!(
		"| issuance not held by any account | — | {:.4} |",
		dot(pallet_balances::TotalIssuance::<T>::get().saturating_sub(in_accounts)),
	);
}

/// Print the balance census of the Relay Chain snapshot. Run with `--nocapture` to see it.
#[tokio::test(flavor = "multi_thread")]
async fn balance_census() {
	let mut rc = remote_ext(Chain::Relay).await;
	rc.execute_with(|| {
		print_balance_census::<polkadot_runtime::Runtime>("Polkadot Relay Chain");

		// Unclaimed pre-genesis claims are part of total issuance but sit in no account — the
		// prime suspect for the "not held by any account" row above.
		let unclaimed =
			polkadot_runtime_common::claims::Total::<polkadot_runtime::Runtime>::get();
		println!();
		println!("claims::Total (unclaimed pre-genesis claims): {:.4} DOT", unclaimed as f64 / 1e10);

		// AHM v1's final balance bookkeeping, for provenance of the issuance numbers. Read via
		// raw key to avoid depending on pallet-rc-migrator just for one probe.
		let key = [
			sp_io::hashing::twox_128(b"RcMigrator"),
			sp_io::hashing::twox_128(b"RcMigratedBalanceArchive"),
		]
		.concat();
		if let Some(raw) = frame_support::storage::unhashed::get_raw(&key) {
			if let Ok((kept, migrated)) = <(u128, u128)>::decode(&mut &raw[..]) {
				println!(
					"v1 RcMigratedBalanceArchive: kept {:.4} DOT, migrated {:.4} DOT",
					kept as f64 / 1e10,
					migrated as f64 / 1e10,
				);
			}
		} else {
			println!("v1 RcMigratedBalanceArchive: not found");
		}

		// The XCM teleport checking account (tracked teleported-out DOT before v1 moved that
		// role to Asset Hub).
		let check = pallet_xcm::Pallet::<polkadot_runtime::Runtime>::check_account();
		let check_data = frame_system::Account::<polkadot_runtime::Runtime>::get(&check).data;
		println!(
			"XCM checking account {check:?}: free {:.4} DOT, reserved {:.4} DOT",
			check_data.free as f64 / 1e10,
			check_data.reserved as f64 / 1e10,
		);
	});
}

/// Account balances migrate RC -> Coretime.
///
/// Drives the accounts stage of `pallet-rc2-migrator` to completion against the real snapshots,
/// shuttles the resulting DMP messages to the Coretime chain and verifies balance conservation on
/// both ends plus the exact balances of one named account: a parachain manager with a live
/// registrar deposit.
///
/// On the relay chain that deposit is an anonymous reserve (old `Currency::reserve` API, no
/// record of why). The migration must not recreate anonymous money: the amount has to arrive on
/// Coretime as a hold under an explicit reason — `CtMigrator(RcMigratedReserve)` until the pallet
/// owning the deposit migrates and re-attributes it — which is what the hold assertions below
/// pin down.
#[tokio::test(flavor = "multi_thread")]
async fn accounts_migrate_rc_to_ct() {
	type Rc = polkadot_runtime::Runtime;
	type Ct = coretime_polkadot_runtime::Runtime;
	type RcStage = pallet_rc2_migrator::MigrationStageOf<Rc>;
	use pallet_rc2_migrator::{RcMigratedBalance, RcMigrationStage};

	let (rc, ct) = tokio::join!(
		tokio::spawn(remote_ext(Chain::Relay)),
		tokio::spawn(remote_ext(Chain::Coretime)),
	);
	let mut rc = rc.expect("failed to load the Relay Chain snapshot");
	let mut ct = ct.expect("failed to load the Coretime snapshot");

	// GIVEN a parachain manager holding a live registrar deposit (reserved balance) on the RC.
	let (manager, rc_free, rc_reserved, rc_issuance_before) = rc.execute_with(|| {
		let (manager, account) = polkadot_runtime_common::paras_registrar::Paras::<Rc>::iter()
			.find_map(|(_para, info)| {
				let account = frame_system::Account::<Rc>::get(&info.manager);
				// A cleanly migratable manager: a real reserve and nothing else attached.
				(account.data.reserved > 0 &&
					account.data.frozen == 0 &&
					pallet_balances::Holds::<Rc>::get(&info.manager).is_empty() &&
					pallet_balances::Locks::<Rc>::get(&info.manager).is_empty())
				.then(|| (info.manager, account))
			})
			.expect("live RC snapshot has a parachain manager with a registrar deposit");
		(
			manager,
			account.data.free,
			account.data.reserved,
			pallet_balances::TotalIssuance::<Rc>::get(),
		)
	});

	// WHEN the accounts stage runs to completion.
	let (dmp, migrated) = rc.execute_with(|| {
		pallet_rc2_migrator::Pallet::<Rc>::force_set_stage(
			polkadot_runtime::RuntimeOrigin::root(),
			RcStage::AccountsInit,
		)
		.expect("root may set the stage");

		for _ in 0..20 {
			if RcMigrationStage::<Rc>::get() == RcStage::AccountsDone {
				break;
			}
			next_block_rc();
		}
		assert_eq!(
			RcMigrationStage::<Rc>::get(),
			RcStage::AccountsDone,
			"accounts stage must finish within 20 blocks"
		);

		(take_dmp(CoretimePolkadot::PARA_ID.into()), RcMigratedBalance::<Rc>::get())
	});
	rc.commit_all().unwrap();
	assert!(!dmp.is_empty(), "the accounts stage queued no DMP messages for Coretime");

	// THEN the manager is reaped on the RC and issuance dropped by exactly what migrated.
	rc.execute_with(|| {
		assert!(
			!frame_system::Account::<Rc>::contains_key(&manager),
			"the manager account must be reaped on the RC"
		);
		let rc_issuance = pallet_balances::TotalIssuance::<Rc>::get();
		assert_eq!(rc_issuance, rc_issuance_before - migrated.migrated);
		assert_eq!(rc_issuance, migrated.kept);
	});

	// AND the Coretime chain integrates every account: the manager's free balance arrives free,
	// the registrar deposit arrives as a hold, and issuance grows by exactly what the RC burned.
	ct.execute_with(|| {
		let ct_account_before = frame_system::Account::<Ct>::get(&manager);
		let ct_issuance_before = pallet_balances::TotalIssuance::<Ct>::get();
		assert!(
			pallet_balances::Holds::<Ct>::get(&manager).is_empty(),
			"manager unexpectedly already has holds on Coretime"
		);

		enqueue_dmp::<CoretimePolkadot>(dmp);
		// Generous bound; the batches drain in a few blocks.
		for _ in 0..30 {
			next_block_para::<CoretimePolkadot>();
		}

		assert_eq!(
			pallet_ct_migrator::CtMigrationStage::<Ct>::get(),
			pallet_ct_migrator::MigrationStage::DataMigrationOngoing,
		);
		assert!(
			pallet_ct_migrator::FailedAccounts::<Ct>::iter().next().is_none(),
			"no account may fail to integrate"
		);

		let ct_account = frame_system::Account::<Ct>::get(&manager);
		assert_eq!(ct_account.data.free, ct_account_before.data.free + rc_free);
		assert_eq!(ct_account.data.reserved, ct_account_before.data.reserved + rc_reserved);

		let holds = pallet_balances::Holds::<Ct>::get(&manager);
		assert_eq!(holds.len(), 1, "the registrar deposit must arrive as exactly one hold");
		assert_eq!(
			holds[0].id,
			coretime_polkadot_runtime::RuntimeHoldReason::CtMigrator(
				pallet_ct_migrator::HoldReason::RcMigratedReserve
			)
		);
		assert_eq!(holds[0].amount, rc_reserved);

		assert_eq!(
			pallet_balances::TotalIssuance::<Ct>::get(),
			ct_issuance_before + migrated.migrated,
			"Coretime must mint exactly what the RC burned"
		);
	});
}

/// The full migration pipeline RC -> CT: accounts, registrar, HRMP, cool-off, done.
///
/// Drives `pallet-rc2-migrator` from `Scheduled` to `MigrationDone` against the real snapshots,
/// shuttling DMP after every burst of RC blocks, and asserts the end state on both chains:
/// registrar and HRMP state drained from the RC and landed on Coretime, deposits re-attributed
/// to `RegistrarDeposit` holds under the `min(recorded, held)` rule, and balance conservation
/// exact.
///
/// With `AHM_EVENTS=<path>` set, the run also writes the JSONL event stream that the migration
/// monitor frontend replays.
#[tokio::test(flavor = "multi_thread")]
async fn full_migration_rc_to_ct() {
	type Rc = polkadot_runtime::Runtime;
	type Ct = coretime_polkadot_runtime::Runtime;
	type RcStage = pallet_rc2_migrator::MigrationStageOf<Rc>;
	use pallet_rc2_migrator::{RcMigratedBalance, RcMigrationStage};
	use polkadot_runtime_common::paras_registrar;
	use runtime_parachains::hrmp::HrmpChannels;

	let (rc, ct) = tokio::join!(
		tokio::spawn(remote_ext(Chain::Relay)),
		tokio::spawn(remote_ext(Chain::Coretime)),
	);
	let mut rc = rc.expect("failed to load the Relay Chain snapshot");
	let mut ct = ct.expect("failed to load the Coretime snapshot");

	// GIVEN the live registrar and HRMP state of the snapshot.
	let (paras_before, hrmp_before, rc_ti_before) = rc.execute_with(|| {
		crate::events::emit_rc_census("before");
		let paras: Vec<(u32, u128)> = paras_registrar::Paras::<Rc>::iter()
			.map(|(id, info)| (id.into(), info.deposit))
			.collect();
		let channels: Vec<_> = HrmpChannels::<Rc>::iter_keys().collect();
		(paras, channels, pallet_balances::TotalIssuance::<Rc>::get())
	});
	assert!(!paras_before.is_empty(), "live RC snapshot has registered paras");
	assert!(!hrmp_before.is_empty(), "live RC snapshot has HRMP channels");
	let recorded_deposits: u128 = paras_before.iter().map(|(_, d)| d).sum();

	let ct_ti_before = ct.execute_with(|| {
		crate::events::emit_ct_census("before");
		pallet_balances::TotalIssuance::<Ct>::get()
	});

	// WHEN the whole migration runs, DMP shuttled after every burst of RC blocks.
	rc.execute_with(|| {
		let start = frame_system::Pallet::<Rc>::block_number() + 1;
		pallet_rc2_migrator::Pallet::<Rc>::force_set_stage(
			polkadot_runtime::RuntimeOrigin::root(),
			RcStage::Scheduled { start },
		)
		.expect("root may set the stage");
	});
	rc.commit_all().unwrap();

	let mut rounds = 0;
	loop {
		rounds += 1;
		assert!(rounds <= 40, "migration must finish within 40 shuttle rounds");

		let (dmp, rc_stage) = rc.execute_with(|| {
			for _ in 0..3 {
				next_block_rc();
			}
			(take_dmp(CoretimePolkadot::PARA_ID.into()), RcMigrationStage::<Rc>::get())
		});
		rc.commit_all().unwrap();

		ct.execute_with(|| {
			enqueue_dmp::<CoretimePolkadot>(dmp);
			for _ in 0..3 {
				next_block_para::<CoretimePolkadot>();
			}
		});
		ct.commit_all().unwrap();

		if rc_stage == RcStage::MigrationDone {
			break;
		}
	}

	// THEN the RC is drained: registrar and HRMP gone, issuance reduced by exactly the burn.
	let migrated = rc.execute_with(|| {
		crate::events::emit_rc_census("after");
		assert!(
			paras_registrar::Paras::<Rc>::iter().next().is_none(),
			"all registrar records must be drained from the RC"
		);
		assert!(
			HrmpChannels::<Rc>::iter().next().is_none(),
			"all HRMP channel records must be drained from the RC"
		);
		let tracker = RcMigratedBalance::<Rc>::get();
		assert_eq!(tracker.kept + tracker.migrated, rc_ti_before, "balance bookkeeping is exact");
		assert_eq!(pallet_balances::TotalIssuance::<Rc>::get(), tracker.kept);
		tracker.migrated
	});

	// AND Coretime holds every record, every deposit is re-attributed or parked, and issuance
	// grew by exactly what the RC burned.
	ct.execute_with(|| {
		use pallet_ct_migrator::*;
		crate::events::emit_ct_census("after");

		assert_eq!(CtMigrationStage::<Ct>::get(), MigrationStage::MigrationDone);
		assert_eq!(RcParas::<Ct>::iter().count(), paras_before.len(), "every para landed");
		assert!(FailedParas::<Ct>::iter().next().is_none(), "no para may fail to integrate");
		assert!(RcNextFreeParaId::<Ct>::get().is_some(), "NextFreeParaId migrated");
		assert_eq!(
			RcHrmpChannels::<Ct>::iter().count(),
			hrmp_before.len(),
			"every HRMP channel landed"
		);
		for id in &hrmp_before {
			assert!(
				RcHrmpChannels::<Ct>::contains_key((
					u32::from(id.sender),
					u32::from(id.recipient)
				)),
				"channel {id:?} must land under its (sender, recipient) key"
			);
		}

		// Reconciliation: re-attributed + parked shortfalls == the registrar-recorded total.
		// Nothing is invented and nothing is silently dropped.
		let reattributed = ReattributedDeposits::<Ct>::get();
		let parked: u128 = ParkedDepositShortfalls::<Ct>::iter().map(|(_, v)| v).sum();
		assert_eq!(reattributed + parked, recorded_deposits, "deposit reconciliation is exact");
		assert!(reattributed > 0, "at least some deposits must re-attribute");

		// Re-attribution must not create RegistrarDeposit holds out of thin air: the sum of all
		// such holds equals the re-attributed total.
		let held: u128 = frame_system::Account::<Ct>::iter_keys()
			.flat_map(|who| pallet_balances::Holds::<Ct>::get(&who))
			.filter(|h| {
				h.id ==
					coretime_polkadot_runtime::RuntimeHoldReason::CtMigrator(
						HoldReason::RegistrarDeposit,
					)
			})
			.map(|h| h.amount)
			.sum();
		assert_eq!(held, reattributed, "RegistrarDeposit holds match the re-attributed total");

		assert_eq!(CtMintedTotal::<Ct>::get(), migrated, "CT minted exactly what the RC burned");
		assert_eq!(pallet_balances::TotalIssuance::<Ct>::get(), ct_ti_before + migrated);
	});
}
