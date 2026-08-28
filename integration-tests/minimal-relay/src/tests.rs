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
use frame_support::{assert_ok, traits::fungible::Inspect};
use migrator_types::PortableProxyType;
use pallet_rc2_migrator::{RcMigratedBalance, RcMigrationStage};
use polkadot_runtime_common::paras_registrar;
use runtime_parachains::hrmp::HrmpChannels;
use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use sp_runtime::{
	traits::{AccountIdConversion, Dispatchable},
	AccountId32,
};
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

/// Mirror of the accounts-stage split rule, for assertions: how much of an account's free
/// balance goes to the Coretime chain (working buffer) versus Asset Hub (teleport). Valid for
/// keyed (`nonce > 0`) accounts whose reserve is fully CT-bound (which is what
/// [`find_clean_manager`] selects); never-signed `Any`-delegators route everything to CT instead.
fn expected_split(free: u128, reserved: u128) -> (u128, u128) {
	let buffer: u128 = polkadot_runtime::CtFreeBuffer::get();
	let ah_ed: u128 = polkadot_runtime::AhExistentialDeposit::get();
	let mut ct_free = if reserved == 0 { 0 } else { free.min(buffer) };
	let mut ah_free = free - ct_free;
	if ah_free > 0 && ah_free < ah_ed && reserved > 0 {
		ct_free += ah_free;
		ah_free = 0;
	}
	(ct_free, ah_free)
}

/// Polkadot-format SS58 of an account, for census output.
fn ss58(a: &AccountId32) -> String {
	a.to_ss58check_with_version(Ss58AddressFormat::custom(0))
}

fn dot(v: u128) -> f64 {
	v as f64 / 1e10
}

/// Decompose every account on the RC by why the migration leaves it behind, with per-account
/// lines for everything that is not aggregate dust.
///
/// With `only_referenced`, regular accounts are printed only when extra consumer references
/// would block their withdrawal — the pre-migration prediction, where printing the millions of
/// migratable accounts would be noise. Without it, every remaining account is printed: the
/// post-migration measurement of the "RC → 0" gap.
fn print_remaining_on_rc(only_referenced: bool) {
	type Rc = polkadot_runtime::Runtime;
	let ed = pallet_balances::Pallet::<Rc>::minimum_balance();

	let (mut dust_n, mut dust_amt) = (0u32, 0u128);
	// Why each dust account is not reapable by the sweep stage (a reapable one would be).
	let mut dust_blockers = BTreeMap::<&str, (u32, u128)>::new();
	let (mut accounts, mut total_sum) = (0u32, 0u128);
	for (who, info) in frame_system::Account::<Rc>::iter() {
		let d = &info.data;
		let total = d.free + d.reserved;
		accounts += 1;
		total_sum += total;
		let bytes: &[u8] = who.as_ref();
		if bytes.starts_with(b"modl") {
			let name = String::from_utf8_lossy(&bytes[4..12]);
			println!(
				"module `{}`: {} | free {:.4} reserved {:.4}",
				name.trim_end_matches('\0'),
				ss58(&who),
				dot(d.free),
				dot(d.reserved),
			);
		// Child sovereigns migrate (translated to sibl); pre-migration they are not leftovers,
		// so only the post-migration mode prints the ones that stayed (held back / anomalies).
		} else if bytes.starts_with(b"sibl") || (!only_referenced && bytes.starts_with(b"para")) {
			let kind = if bytes.starts_with(b"sibl") { "sibl" } else { "para (child)" };
			let para = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
			println!(
				"{kind} sovereign of para {para}: {} | free {:.4} reserved {:.4}",
				ss58(&who),
				dot(d.free),
				dot(d.reserved),
			);
		} else if total < ed {
			dust_n += 1;
			dust_amt += total;
			let blocker = if !pallet_balances::Holds::<Rc>::get(&who).is_empty() {
				"holds"
			} else if info.consumers != 0 {
				"consumer refs"
			} else if d.reserved != 0 {
				"reserved only"
			} else if d.free == 0 {
				"zero balance (provider-ref husk)"
			} else {
				"none (sweep reaps it)"
			};
			let e = dust_blockers.entry(blocker).or_default();
			e.0 += 1;
			e.1 += total;
		} else {
			// Extra consumer refs (beyond the one a reserve accounts for) mean some pallet
			// still references the account and withdrawal would fail — session keys being the
			// known case.
			let expected = u32::from(d.reserved > 0);
			let referenced = info.consumers > expected;
			if referenced || !only_referenced {
				let keys = pallet_session::NextKeys::<Rc>::get(&who).is_some();
				println!(
					"{}: {} | free {:.4} reserved {:.4} consumers {} session-keys {}",
					if referenced { "referenced" } else { "held back" },
					ss58(&who),
					dot(d.free),
					dot(d.reserved),
					info.consumers,
					keys,
				);
			}
		}
	}
	println!("below-ED dust: {dust_n} accounts, {:.4} DOT", dot(dust_amt));
	for (blocker, (n, amt)) in &dust_blockers {
		println!("  dust blocked by {blocker}: {n} accounts, {:.4} DOT", dot(*amt));
	}
	println!("all accounts on the RC: {accounts}, {:.4} DOT", dot(total_sum));
}

/// Find a parachain manager that migrates cleanly: a live registrar deposit that fully accounts
/// for the account's reserve, a signing key (so the buffer split applies, not the pure-proxy
/// routing), and nothing else attached. Returns `(manager, free, reserved)`.
/// Must run inside the RC externalities.
fn find_clean_manager() -> (AccountId32, u128, u128) {
	type Rc = polkadot_runtime::Runtime;
	let mut recorded = BTreeMap::<AccountId32, u128>::new();
	for (_, info) in paras_registrar::Paras::<Rc>::iter() {
		*recorded.entry(info.manager).or_default() += info.deposit;
	}
	paras_registrar::Paras::<Rc>::iter()
		.find_map(|(_, info)| {
			let account = frame_system::Account::<Rc>::get(&info.manager);
			(account.nonce > 0 &&
				account.data.reserved > 0 &&
				account.data.reserved <= *recorded.get(&info.manager).unwrap_or(&0) &&
				account.data.frozen == 0 &&
				pallet_balances::Holds::<Rc>::get(&info.manager).is_empty() &&
				pallet_balances::Locks::<Rc>::get(&info.manager).is_empty())
			.then(|| (info.manager, account.data.free, account.data.reserved))
		})
		.expect("live RC snapshot has a cleanly migrating parachain manager")
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
	let (mut rc, mut para) = tokio::join!(load(Chain::Relay), load(P::CHAIN));

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
	// The exact planck value is the source of truth for `TiCorrection` in the RC runtime config.
	let in_accounts = free.total + reserved.total;
	let phantom = pallet_balances::TotalIssuance::<T>::get().saturating_sub(in_accounts);
	println!("| issuance not held by any account | — | {:.4} |", dot(phantom));
	println!("phantom issuance, exact planck: {phantom}");
}

/// Print the balance census of the Relay Chain snapshot. Run with `--nocapture` to see it.
#[tokio::test(flavor = "multi_thread")]
async fn balance_census() {
	let mut rc = remote_ext(Chain::Relay).await;
	rc.execute_with(|| {
		print_balance_census::<polkadot_runtime::Runtime>("Polkadot Relay Chain");

		// Decompose which accounts the accounts stage keeps on the RC, with amounts — the
		// direct answer to "where does the kept balance sit".
		{
			type Rc = polkadot_runtime::Runtime;
			let ed = pallet_balances::Pallet::<Rc>::minimum_balance();
			let mut cats: BTreeMap<&str, (u32, u128)> = BTreeMap::new();
			for (who, info) in frame_system::Account::<Rc>::iter() {
				let d = &info.data;
				let total = d.free + d.reserved;
				let bytes: &[u8] = who.as_ref();
				let cat = if bytes.starts_with(b"para") {
					"para sovereign"
				} else if bytes.starts_with(b"sibl") {
					"sibl sovereign"
				} else if bytes.starts_with(b"modl") {
					"module account"
				} else if total < ed {
					"below-ED"
				} else if d.frozen > 0 ||
					!pallet_balances::Locks::<Rc>::get(&who).is_empty() ||
					!pallet_balances::Freezes::<Rc>::get(&who).is_empty() ||
					!pallet_balances::Holds::<Rc>::get(&who).is_empty()
				{
					"locks/freezes/holds"
				} else {
					continue
				};
				let e = cats.entry(cat).or_default();
				e.0 += 1;
				e.1 += total;
			}
			println!("\n### kept-account decomposition (accounts the migrator skips)");
			let mut sum = 0u128;
			for (cat, (n, amt)) in &cats {
				println!("{cat}: {n} accounts, {:.4} DOT", *amt as f64 / 1e10);
				sum += amt;
			}
			println!("skipped-account total: {:.4} DOT", sum as f64 / 1e10);
		}

		// Registrar reconciliation census: for every manager, compare the recorded deposits
		// against the live reserve. Classifies the deposit-shortfall causes: `zero`/`partial`
		// are genuine on-chain anomalies (reserve reduced out-of-band, record never updated);
		// `over` are accounts with additional unattributable reserves (proxy deposits) that the
		// migration holds back whole.
		{
			type Rc = polkadot_runtime::Runtime;
			let mut recorded = BTreeMap::<AccountId32, u128>::new();
			let mut paras_of = BTreeMap::<AccountId32, Vec<(u32, u128, Option<bool>)>>::new();
			for (id, info) in paras_registrar::Paras::<Rc>::iter() {
				*recorded.entry(info.manager.clone()).or_default() += info.deposit;
				paras_of.entry(info.manager).or_default().push((
					id.into(),
					info.deposit,
					info.locked,
				));
			}
			let mut cls = BTreeMap::<&str, (u32, u32, u128)>::new(); // managers, paras, recorded
			println!("\n### registrar deposit anomalies (recorded vs live reserve, per manager)");
			for (manager, expected) in &recorded {
				let reserved = frame_system::Account::<Rc>::get(manager).data.reserved;
				let cat = if reserved == *expected {
					"exact"
				} else if reserved == 0 {
					"zero reserve (anomaly)"
				} else if reserved < *expected {
					"partial reserve (anomaly)"
				} else {
					"over-reserved (held back: proxy etc.)"
				};
				if cat != "exact" {
					println!(
						"{cat}: {} | paras {:?} | recorded {:.4} DOT, live reserve {:.4} DOT, gap {:.4} DOT",
						ss58(manager),
						paras_of[manager],
						dot(*expected),
						dot(reserved),
						dot(expected.saturating_sub(reserved)),
					);
				}
				let e = cls.entry(cat).or_default();
				e.0 += 1;
				e.1 += paras_of[manager].len() as u32;
				e.2 += expected;
			}
			println!("\n### registrar reconciliation summary");
			for (cat, (managers, paras, dep)) in &cls {
				println!(
					"{cat}: {managers} managers / {paras} paras, recorded {:.4} DOT",
					dot(*dep)
				);
			}

			// HRMP: per (child) sovereign, channel deposits vs live reserve — including pending
			// open-channel-request deposits, which are reserved but attached to no channel yet.
			let mut channel_dep = BTreeMap::<AccountId32, (u128, Vec<u32>)>::new();
			for (id, ch) in HrmpChannels::<Rc>::iter() {
				let s = channel_dep
					.entry(id.sender.into_account_truncating())
					.or_insert((0, vec![u32::from(id.sender)]));
				s.0 += ch.sender_deposit;
				let r = channel_dep
					.entry(id.recipient.into_account_truncating())
					.or_insert((0, vec![u32::from(id.recipient)]));
				r.0 += ch.recipient_deposit;
			}
			let mut request_dep = BTreeMap::<AccountId32, u128>::new();
			for (id, req) in runtime_parachains::hrmp::HrmpOpenChannelRequests::<Rc>::iter() {
				*request_dep.entry(id.sender.into_account_truncating()).or_default() +=
					req.sender_deposit;
			}
			println!(
				"\n### hrmp sovereign reconciliation (channel + request deposits vs live reserve)"
			);
			let (mut exact, mut short, mut over) = (0u32, 0u32, 0u32);
			for (sov, (chan, paras)) in &channel_dep {
				let requests = request_dep.get(sov).copied().unwrap_or(0);
				let reserved = frame_system::Account::<Rc>::get(sov).data.reserved;
				let full = chan + requests;
				if reserved == full {
					exact += 1;
				} else if reserved < full {
					short += 1;
					println!(
						"short: para {:?} | channels {:.4} + requests {:.4} DOT recorded, live reserve {:.4} DOT",
						paras, dot(*chan), dot(requests), dot(reserved),
					);
				} else {
					over += 1;
					println!(
						"over: para {:?} | channels {:.4} + requests {:.4} DOT recorded, live reserve {:.4} DOT",
						paras, dot(*chan), dot(requests), dot(reserved),
					);
				}
			}
			println!(
				"hrmp sovereigns: {exact} exact, {short} short (anomaly), {over} over-reserved"
			);
		}

		// Who would remain on the RC after the migration and why — the "RC → 0" gap list.
		{
			println!("\n### remaining-on-RC gap list (accounts the migration cannot move)");
			print_remaining_on_rc(true);
		}

		// The Balances pallet's own storage keys: how many are empty leftovers, and how many
		// belong to accounts that no longer exist (v1 reaped the account, the key survived).
		{
			type Rc = polkadot_runtime::Runtime;
			let report = |name: &str, entries: Vec<(AccountId32, bool)>| {
				let n = entries.len();
				let empty = entries.iter().filter(|(_, e)| *e).count();
				let orphan = entries
					.iter()
					.filter(|(who, _)| !frame_system::Account::<Rc>::contains_key(who))
					.count();
				println!(
					"{name}: {n} keys, {empty} empty-value, {orphan} for nonexistent accounts"
				);
			};
			report(
				"Balances::Locks",
				pallet_balances::Locks::<Rc>::iter().map(|(w, v)| (w, v.is_empty())).collect(),
			);
			report(
				"Balances::Reserves",
				pallet_balances::Reserves::<Rc>::iter()
					.map(|(w, v)| (w, v.is_empty()))
					.collect(),
			);
			report(
				"Balances::Freezes",
				pallet_balances::Freezes::<Rc>::iter().map(|(w, v)| (w, v.is_empty())).collect(),
			);
			report(
				"Balances::Holds",
				pallet_balances::Holds::<Rc>::iter().map(|(w, v)| (w, v.is_empty())).collect(),
			);
		}

		// Unclaimed pre-genesis claims are part of total issuance but sit in no account — the
		// prime suspect for the "not held by any account" row above.
		let unclaimed = polkadot_runtime_common::claims::Total::<polkadot_runtime::Runtime>::get();
		println!();
		println!(
			"claims::Total (unclaimed pre-genesis claims): {:.4} DOT",
			unclaimed as f64 / 1e10
		);

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

/// Dump every proxy entry on the Relay Chain as JSON lines, cross-referenced against registrar
/// managers and para sovereigns — the data source for the proxy migration report
/// (`ahm-phase-2/simulation/proxy-report.html`).
///
/// Writes to the file named by `AHM_PROXY_DUMP`; without it, prints only aggregates.
#[tokio::test(flavor = "multi_thread")]
async fn proxy_census() {
	type Rc = polkadot_runtime::Runtime;

	let mut rc = remote_ext(Chain::Relay).await;
	rc.execute_with(|| {
		// Registrar managers and what they manage.
		let mut manages = BTreeMap::<AccountId32, Vec<u32>>::new();
		for (id, info) in paras_registrar::Paras::<Rc>::iter() {
			manages.entry(info.manager).or_default().push(id.into());
		}

		let mut out = Vec::new();
		let (mut entries, mut deposit_total) = (0u32, 0u128);
		for (who, (defs, deposit)) in pallet_proxy::Proxies::<Rc>::iter() {
			entries += 1;
			deposit_total += deposit;
			let account = frame_system::Account::<Rc>::get(&who);
			let bytes: &[u8] = who.as_ref();
			let sovereign = bytes.starts_with(b"para") || bytes.starts_with(b"sibl");
			let delegates: Vec<String> = defs
				.iter()
				.map(|d| {
					format!(
						r#"{{"delegate":"{}","type":"{:?}","delay":{},"delegate_manages":{:?}}}"#,
						ss58(&d.delegate),
						d.proxy_type,
						d.delay,
						manages.get(&d.delegate).cloned().unwrap_or_default(),
					)
				})
				.collect();
			// `consumers` decides the delegator's fate in the migration: withdrawal fails when
			// consumer references remain after the reserve is released (session references
			// being the known case), keeping the entry on the RC. Session keys themselves are
			// never touched either way — they are live session state managed from AH via XCM,
			// and a zero-balance key-holder is the intended end state.
			out.push(format!(
				r#"{{"who":"{}","deposit":"{}","free":"{}","reserved":"{}","nonce":{},"consumers":{},"sovereign":{},"session_keys":{},"exists":{},"manages":{:?},"delegates":[{}]}}"#,
				ss58(&who),
				deposit,
				account.data.free,
				account.data.reserved,
				account.nonce,
				account.consumers,
				sovereign,
				pallet_session::NextKeys::<Rc>::get(&who).is_some(),
				frame_system::Account::<Rc>::contains_key(&who),
				manages.get(&who).cloned().unwrap_or_default(),
				delegates.join(","),
			));
		}
		println!("proxy census: {entries} delegators, {:.2} DOT deposits", deposit_total as f64 / 1e10);
		if let Ok(path) = std::env::var("AHM_PROXY_DUMP") {
			std::fs::write(&path, out.join("\n")).expect("can write proxy dump");
			println!("wrote {path}");
		}
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

	let (mut rc, mut ct) = tokio::join!(load(Chain::Relay), load(Chain::Coretime));

	// GIVEN a parachain manager holding a live registrar deposit (reserved balance) on the RC.
	let (manager, rc_free, rc_reserved, rc_issuance_before) = rc.execute_with(|| {
		let (manager, free, reserved) = find_clean_manager();
		(manager, free, reserved, pallet_balances::TotalIssuance::<Rc>::get())
	});
	// What the split rule should do with this manager.
	let (ct_free_exp, _ah_free_exp) = expected_split(rc_free, rc_reserved);

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
	let total_migrated = migrated.ct_reserved + migrated.ct_free + migrated.ah_free;
	rc.execute_with(|| {
		assert!(
			!frame_system::Account::<Rc>::contains_key(&manager),
			"the manager account must be reaped on the RC"
		);
		let rc_issuance = pallet_balances::TotalIssuance::<Rc>::get();
		assert_eq!(rc_issuance, rc_issuance_before - total_migrated);
		assert_eq!(rc_issuance, migrated.kept);
	});

	// AND the Coretime chain integrates every CT-bound piece: the registrar deposit arrives as a
	// hold, the working buffer arrives free, and issuance grows by exactly the CT-bound burn.
	// (The teleported remainder is asserted end-to-end in `full_migration_rc_to_ct`.)
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
		assert_eq!(ct_account.data.free, ct_account_before.data.free + ct_free_exp);
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
			ct_issuance_before + migrated.ct_reserved + migrated.ct_free,
			"Coretime must mint exactly the CT-bound burn"
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
	type Ah = asset_hub_polkadot_runtime::Runtime;

	let (mut rc, mut ct, mut ah) =
		tokio::join!(load(Chain::Relay), load(Chain::Coretime), load(Chain::AssetHub));

	// GIVEN the live registrar and HRMP state of the snapshot, and one cleanly migrating
	// manager to spot-check the AH teleport leg with.
	// `paras_before_detail` and `requests_before_detail` carry the facts only this chain can
	// see — the lifecycle that tells Reserved from Registered, the lock, and whether a request
	// was confirmed. Coretime cannot rederive any of them, so they are what the post-migration
	// assertions compare against.
	let (
		paras_before,
		paras_before_detail,
		hrmp_before,
		requests_before,
		requests_before_detail,
		rc_ti_before,
		sample,
	) = rc.execute_with(|| {
			crate::events::emit_rc_census("before");
			crate::events::emit_pre_facts();
			let paras: Vec<(u32, u128)> = paras_registrar::Paras::<Rc>::iter()
				.map(|(id, info)| (id.into(), info.deposit))
				.collect();
			let detail: Vec<(u32, bool, bool)> = paras_registrar::Paras::<Rc>::iter()
				.map(|(id, info)| {
					(
						id.into(),
						runtime_parachains::paras::Pallet::<Rc>::lifecycle(id).is_some(),
						info.locked.unwrap_or(false),
					)
				})
				.collect();
			let channels: Vec<(_, u128, u128)> = HrmpChannels::<Rc>::iter()
				.map(|(id, ch)| (id, ch.sender_deposit, ch.recipient_deposit))
				.collect();
			let requests: Vec<u128> =
				runtime_parachains::hrmp::HrmpOpenChannelRequests::<Rc>::iter()
					.map(|(_, r)| r.sender_deposit)
					.collect();
			let requests_detail: Vec<(u32, u32, bool)> =
				runtime_parachains::hrmp::HrmpOpenChannelRequests::<Rc>::iter()
					.map(|(id, r)| (id.sender.into(), id.recipient.into(), r.confirmed))
					.collect();

			(
				paras,
				detail,
				channels,
				requests,
				requests_detail,
				pallet_balances::TotalIssuance::<Rc>::get(),
				find_clean_manager(),
			)
		});

	// Pre-migration sanity: the snapshot must actually contain the shapes this test claims to
	// exercise, or a green run would prove nothing.
	assert!(
		paras_before_detail.iter().any(|(_, registered, _)| *registered),
		"the snapshot must contain at least one onboarded para"
	);
	assert!(
		paras_before_detail.iter().any(|(_, _, locked)| *locked),
		"the snapshot must contain at least one locked para, or the lock carry is untested"
	);

	// The sweep stage's inputs — the configured pots plus reapable below-ED dust, all landing
	// on the sweep beneficiary — and the sibl-format sovereigns, whose balances migrate
	// untranslated to the same bytes on AH.
	let (treasury, sweep_pots, sweep_dust, sibl_before) = rc.execute_with(|| {
		let treasury: AccountId32 =
			polkadot_runtime::TreasuryPalletId::get().into_account_truncating();
		let pots: Vec<(AccountId32, u128)> = polkadot_runtime::SweepAccounts::get()
			.into_iter()
			.map(|who| {
				let amount = frame_system::Account::<Rc>::get(&who).data.free;
				(who, amount)
			})
			.collect();
		let ed = pallet_balances::Pallet::<Rc>::minimum_balance();
		let mut dust = 0u128;
		for (_, info) in frame_system::Account::<Rc>::iter() {
			let total = info.data.free + info.data.reserved;
			if total > 0 && total < ed {
				dust += total;
			}
		}
		// A sibl account's expected AH arrival includes its para's CHILD sovereign: the child
		// migrates translated to the same sibl bytes, its AH-bound free landing on one address.
		let sibl: Vec<(AccountId32, u128)> = frame_system::Account::<Rc>::iter()
			.filter(|(who, _)| AsRef::<[u8]>::as_ref(who).starts_with(b"sibl"))
			.map(|(who, info)| {
				let bytes: &[u8] = who.as_ref();
				let para = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
				let child: AccountId32 =
					polkadot_primitives::Id::from(para).into_account_truncating();
				let child_data = frame_system::Account::<Rc>::get(&child).data;
				let (_, child_ah) = expected_split(child_data.free, child_data.reserved);
				(who, info.data.free + info.data.reserved + child_ah)
			})
			.collect();
		(treasury, pots, dust, sibl)
	});
	assert!(sweep_pots.iter().any(|(_, a)| *a > 0), "the snapshot has pots to sweep");
	assert!(!sibl_before.is_empty(), "the snapshot has sibl-format sovereigns");
	assert!(!paras_before.is_empty(), "live RC snapshot has registered paras");
	assert!(!hrmp_before.is_empty(), "live RC snapshot has HRMP channels");
	let recorded_deposits: u128 = paras_before.iter().map(|(_, d)| d).sum();
	let recorded_hrmp: u128 = hrmp_before.iter().map(|(_, s, r)| s + r).sum::<u128>() +
		requests_before.iter().sum::<u128>();

	// Proxy state before: the delegators with portable definitions (which travel to CT), one
	// delegator with both an Any and a ParaRegistration delegate for the dispatch checks, and
	// every never-signed delegator for the funds-follow-control invariant.
	let (ct_bound_proxies, nonce0_delegators, proxy_dispatch) = rc.execute_with(|| {
		// The portable type a definition translates to, if the CT chain represents it.
		let portable = |t: &<Rc as pallet_proxy::Config>::ProxyType| {
			PortableProxyType::try_from(t.clone()).ok()
		};
		let mut ct_bound = Vec::new();
		let mut nonce0 = Vec::new();
		let mut dispatch = None;
		for (who, (defs, _)) in pallet_proxy::Proxies::<Rc>::iter() {
			let account = frame_system::Account::<Rc>::get(&who);
			let had_any =
				defs.iter().any(|d| portable(&d.proxy_type) == Some(PortableProxyType::Any));
			if account.nonce == 0 {
				// A never-signed delegator. With an `Any` def it is (or must be treated as) a
				// keyless pure whose whole balance follows the recreated definitions to CT. A
				// nonce-0 delegator WITHOUT `Any` defs necessarily created its entry via
				// `as_multi` — no other dispatch path exists at nonce 0 with no prior defs and
				// no post-v1 `create_pure` (verified by events and the spawner-reserve
				// signature) — so it is a multisig, controllable on every chain by its members.
				let existed = frame_system::Account::<Rc>::contains_key(&who);
				nonce0.push((
					who.clone(),
					had_any,
					existed,
					account.data.free + account.data.reserved,
				));
			}
			if defs.iter().any(|d| portable(&d.proxy_type).is_some()) {
				ct_bound.push(who.clone());
			}
			let any =
				defs.iter().find(|d| portable(&d.proxy_type) == Some(PortableProxyType::Any));
			let reg = defs
				.iter()
				.find(|d| portable(&d.proxy_type) == Some(PortableProxyType::ParaRegistration));
			if let (Some(any), Some(reg), None) = (any, reg, dispatch.as_ref()) {
				dispatch = Some((who.clone(), any.delegate.clone(), reg.delegate.clone()));
			}
		}
		let dispatch =
			dispatch.expect("snapshot has a delegator with Any + ParaRegistration proxies");
		(ct_bound, nonce0, dispatch)
	});
	assert!(!ct_bound_proxies.is_empty(), "live snapshot has portable proxies");

	let (ct_ti_before, ct_pures_before) = ct.execute_with(|| {
		crate::events::emit_ct_census("before");
		// Pre-migration CT balances of the possible pures, so their arrival asserts exactly.
		let pures: BTreeMap<AccountId32, u128> = nonce0_delegators
			.iter()
			.filter(|(_, had_any, existed, _)| *had_any && *existed)
			.map(|(who, ..)| {
				let d = frame_system::Account::<Ct>::get(who).data;
				(who.clone(), d.free + d.reserved)
			})
			.collect();
		(pallet_balances::TotalIssuance::<Ct>::get(), pures)
	});
	let (ah_ti_before, ah_checking_before, ah_sample_before) = ah.execute_with(|| {
		// Baseline probe so the event stream carries the checking account's pre-migration
		// value; the monitor measures the teleport receipts as the drain from this baseline.
		crate::events::emit_para_block(Chain::AssetHub);

		let checking = pallet_xcm::Pallet::<Ah>::check_account();
		(
			pallet_balances::TotalIssuance::<Ah>::get(),
			frame_system::Account::<Ah>::get(&checking).data.free,
			frame_system::Account::<Ah>::get(&sample.0).data.free,
		)
	});
	// Pre-migration AH balances of every address the sweep and sibl legs pay into.
	let ah_arrivals_before: BTreeMap<AccountId32, u128> = ah.execute_with(|| {
		sweep_pots
			.iter()
			.map(|(who, _)| who)
			.chain(sibl_before.iter().map(|(who, _)| who))
			.map(|who| (who.clone(), frame_system::Account::<Ah>::get(who).data.free))
			.collect()
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

		let (ct_dmp, ah_dmp, rc_stage) = rc.execute_with(|| {
			for _ in 0..3 {
				next_block_rc();
			}
			(
				take_dmp(CoretimePolkadot::PARA_ID.into()),
				take_dmp(AssetHubPolkadot::PARA_ID.into()),
				RcMigrationStage::<Rc>::get(),
			)
		});
		rc.commit_all().unwrap();

		ct.execute_with(|| {
			enqueue_dmp::<CoretimePolkadot>(ct_dmp);
			for _ in 0..3 {
				next_block_para::<CoretimePolkadot>();
			}
		});
		ct.commit_all().unwrap();

		ah.execute_with(|| {
			enqueue_dmp::<AssetHubPolkadot>(ah_dmp);
			for _ in 0..3 {
				next_block_para::<AssetHubPolkadot>();
				// A trapped asset means a teleport failed half-way; that must never pass.
				assert!(
					!frame_system::Pallet::<Ah>::events().into_iter().any(|r| matches!(
						r.event,
						asset_hub_polkadot_runtime::RuntimeEvent::PolkadotXcm(
							pallet_xcm::Event::AssetsTrapped { .. }
						)
					)),
					"assets were trapped on Asset Hub"
				);
			}
		});
		ah.commit_all().unwrap();

		// --- during-migration sanity -------------------------------------------------------
		// Checked every round rather than only at the end, so a transient breach is caught
		// where it happens rather than being papered over by the final state.
		let ct_has_para: BTreeMap<u32, bool> = ct.execute_with(|| {
			// The receiving pallets' invariants must hold at every intermediate step, not just
			// once the machine stops. A migration that passes through a state where a channel
			// holds the wrong deposits has a bug, even if it tidies up afterwards.
			pallet_hrmp_para::Pallet::<Ct>::do_try_state()
				.expect("HRMP invariants broke mid-migration");
			pallet_registrar_para::Pallet::<Ct>::do_try_state()
				.expect("registrar invariants broke mid-migration");

			// Nothing may be handed over twice. Records arrive in batches across many blocks,
			// and a retried batch that re-inserted would double-charge a deposit.
			let paras = pallet_registrar_para::Paras::<Ct>::iter().count();
			assert!(
				paras <= paras_before.len(),
				"more paras on CT ({paras}) than the relay chain ever had"
			);
			let channels = pallet_hrmp_para::Channels::<Ct>::iter().count();
			assert!(
				channels <= hrmp_before.len() + requests_before_detail.len(),
				"more channels on CT ({channels}) than the relay chain ever had"
			);

			paras_before_detail
				.iter()
				.map(|(id, _, _)| (*id, pallet_registrar_para::Paras::<Ct>::contains_key(*id)))
				.collect()
		});
		ct.commit_all().unwrap();

		// A record that reaches CT must have left the relay chain: the two sides must never
		// both claim the same para, or there are two control planes for it at once.
		rc.execute_with(|| {
			for (para_id, _, _) in &paras_before_detail {
				let on_rc = paras_registrar::Paras::<Rc>::contains_key(
					polkadot_primitives::Id::from(*para_id),
				);
				let on_ct = ct_has_para.get(para_id).copied().unwrap_or(false);
				assert!(!(on_rc && on_ct), "para {para_id} is claimed by both chains at once");
			}
		});
		rc.commit_all().unwrap();

		if rc_stage == RcStage::MigrationDone {
			break;
		}
	}

	// THEN the RC is drained: registrar and HRMP gone, issuance reduced by exactly the burn.
	let (tracker, migrated_nonce0) = rc.execute_with(|| {
		crate::events::emit_rc_census("after");
		assert!(
			paras_registrar::Paras::<Rc>::iter().next().is_none(),
			"all registrar records must be drained from the RC"
		);
		assert!(
			HrmpChannels::<Rc>::iter().next().is_none(),
			"all HRMP channel records must be drained from the RC"
		);
		assert!(
			runtime_parachains::hrmp::HrmpOpenChannelRequests::<Rc>::iter().next().is_none(),
			"all pending HRMP requests must be dropped"
		);

		// No ghost proxy records: every remaining entry's recorded deposit is backed by an
		// actual reserve, and the dispatch-check manager's translatable defs left for CT.
		for (who, (_, deposit)) in pallet_proxy::Proxies::<Rc>::iter() {
			assert!(
				deposit <= frame_system::Account::<Rc>::get(&who).data.reserved,
				"proxy entry of {who:?} claims a deposit that is not reserved"
			);
			assert!(
				frame_system::Account::<Rc>::contains_key(&who),
				"fund-less proxy entries (v1 husks) must be deleted, found {who:?}"
			);
		}
		let residual = pallet_proxy::Proxies::<Rc>::get(&proxy_dispatch.0).0;
		assert!(
			residual
				.iter()
				.all(|d| PortableProxyType::try_from(d.proxy_type.clone()).is_err()),
			"the manager's translatable defs must have left the RC"
		);

		// Every funded pot is gone; its balance teleported to the same address on AH.
		for (who, amount) in &sweep_pots {
			if *amount > 0 {
				assert!(
					!frame_system::Account::<Rc>::contains_key(who),
					"pot {who:?} must be swept"
				);
			}
		}

		// The RC end state: not a single planck remains anywhere, and every surviving record
		// earns its place — something still references it (session key-holders being the known
		// case) or it is a module account. Unreferenced husks are reaped.
		for (who, info) in frame_system::Account::<Rc>::iter() {
			assert_eq!(
				info.data.free + info.data.reserved,
				0,
				"only zero-balance shells may stay on the RC, found {who:?}"
			);
			let bytes: &[u8] = who.as_ref();
			assert!(
				info.consumers > 0 || bytes.starts_with(b"modl"),
				"unreferenced husk {who:?} must have been reaped"
			);
		}

		// Never-signed delegators whose accounts migrated away (fund-less husks never had an
		// account to migrate): their CT-side control is asserted below.
		let migrated_nonce0: Vec<_> = nonce0_delegators
			.iter()
			.filter(|(who, _, existed, _)| {
				*existed && !frame_system::Account::<Rc>::contains_key(who)
			})
			.cloned()
			.collect();
		// The measured counterpart of `balance_census`'s pre-migration gap list: every account
		// still here, per line. Visible with `--nocapture`.
		println!("\n### remaining on RC after the migration");
		print_remaining_on_rc(false);

		// The proxy entries that survive the proxy stage, with why their delegator is still
		// here (session keys / unattributable reserve).
		println!("\n### proxy entries remaining on RC");
		for (who, (defs, deposit)) in pallet_proxy::Proxies::<Rc>::iter() {
			let account = frame_system::Account::<Rc>::get(&who);
			let types: Vec<String> =
				defs.iter().map(|d| format!("{:?}/{}", d.proxy_type, d.delay)).collect();
			println!(
				"{} | defs [{}] deposit {:.4} | free {:.4} reserved {:.4} nonce {} \
				 session-keys {}",
				ss58(&who),
				types.join(", "),
				dot(deposit),
				dot(account.data.free),
				dot(account.data.reserved),
				account.nonce,
				pallet_session::NextKeys::<Rc>::get(&who).is_some(),
			);
		}

		let tracker = RcMigratedBalance::<Rc>::get();
		assert_eq!(
			tracker.kept +
				tracker.ct_reserved +
				tracker.ct_free +
				tracker.ah_free +
				tracker.ti_corrected,
			rc_ti_before,
			"balance bookkeeping is exact"
		);
		assert_eq!(pallet_balances::TotalIssuance::<Rc>::get(), tracker.kept);
		// The headline: the relay chain ends the migration with ZERO issuance.
		assert_eq!(tracker.kept, 0, "the RC must drain to exactly zero DOT");
		// The audited phantom issuance was burned in full: the runtime constant equals the
		// measured unaccounted issuance on this snapshot, so nothing remains and no anomaly.
		assert_eq!(
			tracker.ti_corrected,
			polkadot_runtime::TiCorrection::get(),
			"TI correction burned exactly the audited amount"
		);
		(tracker, migrated_nonce0)
	});
	let migrated_ct = tracker.migrated_ct();

	// AND Coretime holds every record, every deposit is re-attributed or parked, and issuance
	// grew by exactly what the RC burned.
	ct.execute_with(|| {
		use pallet_ct_migrator::*;
		crate::events::emit_ct_census("after");

		use pallet_hrmp_para::ChannelState;
		use pallet_registrar_para::RegistrationState;

		assert_eq!(CtMigrationStage::<Ct>::get(), MigrationStage::MigrationDone);
		let failed_paras: Vec<u32> = FailedParas::<Ct>::iter_keys().collect();
		assert!(
			failed_paras.is_empty(),
			"{} of {} paras failed to integrate: {failed_paras:?}",
			failed_paras.len(),
			paras_before.len(),
		);
		let failed_channels: Vec<_> = FailedHrmpChannels::<Ct>::iter_keys().collect();
		assert!(
			failed_channels.is_empty(),
			"{} channels failed to integrate: {failed_channels:?}",
			failed_channels.len(),
		);

		// --- the registrar pallet actually owns the paras now -----------------------------
		assert_eq!(
			pallet_registrar_para::Paras::<Ct>::iter().count(),
			paras_before.len(),
			"every para landed in the registrar pallet"
		);
		assert!(
			pallet_registrar_para::NextFreeParaId::<Ct>::get() > 0,
			"the id counter migrated"
		);
		for (para_id, registered, locked) in &paras_before_detail {
			let info = pallet_registrar_para::Paras::<Ct>::get(*para_id)
				.unwrap_or_else(|| panic!("para {para_id} must land"));

			// A live para must arrive locked. Miss this and its manager silently regains the
			// control the relay chain's lock existed to remove — the whole point of carrying
			// the flag across.
			assert_eq!(
				info.locked, *locked,
				"para {para_id} must arrive with the relay chain's lock"
			);

			// Reserved and Registered are told apart by `paras::ParaLifecycles`, which stays on
			// the relay chain. If that classification were lost, a merely reserved id would
			// arrive holding a registration deposit it never paid.
			match (registered, &info.state) {
				(true, RegistrationState::Registered { .. }) => {},
				(false, RegistrationState::Reserved) => {},
				(_, other) => panic!(
					"para {para_id}: registered={registered} but arrived as {other:?}"
				),
			}
		}

		// --- and the HRMP pallet owns the channels ----------------------------------------
		assert_eq!(
			pallet_hrmp_para::Channels::<Ct>::iter().count(),
			hrmp_before.len() + requests_before_detail.len(),
			"every channel and pending request landed in the HRMP pallet"
		);
		for (id, _, _) in &hrmp_before {
			let key = hrmp_primitives::ChannelId {
				sender: id.sender.into(),
				recipient: id.recipient.into(),
			};
			let info = pallet_hrmp_para::Channels::<Ct>::get(key)
				.unwrap_or_else(|| panic!("channel {key:?} must land"));
			// An existing channel means both ends paid, so it arrives fully open.
			assert_eq!(info.state, ChannelState::Open, "channel {key:?} must arrive open");
		}
		for (sender, recipient, confirmed) in &requests_before_detail {
			let key = hrmp_primitives::ChannelId { sender: *sender, recipient: *recipient };
			let info = pallet_hrmp_para::Channels::<Ct>::get(key)
				.unwrap_or_else(|| panic!("request {key:?} must land"));
			// An unconfirmed request is the sender's deposit alone, which is exactly what
			// `Pending` means here. Getting this wrong would hold money nobody paid.
			let want = if *confirmed { ChannelState::Open } else { ChannelState::Pending };
			assert_eq!(info.state, want, "request {key:?} must arrive in the right state");
		}

		// --- the id counter cannot hand out something already taken ----------------------
		// After the drain the relay chain's counter is gone, so Coretime's is the only one. If
		// it arrived below a live id, the next `reserve` would collide with a real parachain.
		let highest = paras_before_detail.iter().map(|(id, _, _)| *id).max().unwrap_or(0);
		assert!(
			pallet_registrar_para::NextFreeParaId::<Ct>::get() > highest,
			"the id counter must sit above every migrated para"
		);

		// --- migrated state must be usable, not merely present ---------------------------
		// The strongest thing this test can say: pick real migrated paras out of the snapshot
		// and drive them through the pallet as their manager would.
		let locked_para = paras_before_detail.iter().find(|(_, _, locked)| *locked);
		if let Some((para_id, _, _)) = locked_para {
			let manager = pallet_registrar_para::Paras::<Ct>::get(*para_id).unwrap().manager;
			// A locked para's manager must be shut out. This is the whole reason the lock is
			// carried across; if it silently failed, the manager would regain control of a live
			// parachain.
			let refused = pallet_registrar_para::Pallet::<Ct>::deregister(
				coretime_polkadot_runtime::RuntimeOrigin::signed(manager.clone()),
				*para_id,
			);
			assert!(refused.is_err(), "a locked migrated para must refuse its manager");

			// And the manager may still lock further: locking is never blocked by a lock.
			assert_ok!(pallet_registrar_para::Pallet::<Ct>::add_lock(
				coretime_polkadot_runtime::RuntimeOrigin::signed(manager),
				*para_id,
			));
		}

		let reserved_para = paras_before_detail.iter().find(|(_, registered, locked)| {
			!*registered && !*locked
		});
		if let Some((para_id, _, _)) = reserved_para {
			let info = pallet_registrar_para::Paras::<Ct>::get(*para_id).unwrap();
			let before = pallet_balances::Pallet::<Ct>::free_balance(&info.manager);
			// A reserved id is dropped locally, deposit returned, without ever touching the
			// relay chain — so it works even though nothing is listening up there any more.
			assert_ok!(pallet_registrar_para::Pallet::<Ct>::deregister(
				coretime_polkadot_runtime::RuntimeOrigin::signed(info.manager.clone()),
				*para_id,
			));
			assert!(pallet_registrar_para::Paras::<Ct>::get(*para_id).is_none());
			assert!(
				pallet_balances::Pallet::<Ct>::free_balance(&info.manager) > before,
				"dropping a reserved id must return its deposit"
			);
		}

		// --- the pallets' own invariants hold across the whole live snapshot --------------
		// This is the strongest check in the test: it says every channel holds exactly the
		// deposits its state claims, for real migrated data rather than a fixture.
		assert_ok!(pallet_hrmp_para::Pallet::<Ct>::do_try_state());
		assert_ok!(pallet_registrar_para::Pallet::<Ct>::do_try_state());

		// Reconciliation: re-attributed + parked shortfalls == the recorded totals, for both
		// deposit kinds. Nothing is invented and nothing is silently dropped.
		let reattributed = ReattributedDeposits::<Ct>::get();
		let parked: u128 = ParkedDepositShortfalls::<Ct>::iter().map(|(_, v)| v).sum();
		assert_eq!(reattributed + parked, recorded_deposits, "deposit reconciliation is exact");
		assert!(reattributed > 0, "at least some deposits must re-attribute");
		let reattributed_hrmp = ReattributedHrmpDeposits::<Ct>::get();
		let parked_hrmp: u128 = ParkedHrmpShortfalls::<Ct>::iter().map(|(_, v)| v).sum();
		assert_eq!(
			reattributed_hrmp + parked_hrmp,
			recorded_hrmp,
			"HRMP deposit reconciliation is exact"
		);

		// Re-attribution must not create holds out of thin air: the sum of holds under each
		// attributed reason equals its re-attributed total.
		let reg_id =
			coretime_polkadot_runtime::RuntimeHoldReason::CtMigrator(HoldReason::RegistrarDeposit);
		let hrmp_id =
			coretime_polkadot_runtime::RuntimeHoldReason::CtMigrator(HoldReason::HrmpDeposit);
		let proxy_id =
			coretime_polkadot_runtime::RuntimeHoldReason::CtMigrator(HoldReason::ProxyDeposit);
		let unattributed_id = coretime_polkadot_runtime::RuntimeHoldReason::CtMigrator(
			HoldReason::UnattributedReserve,
		);
		let (mut reg_held, mut hrmp_held, mut proxy_held, mut unattributed_held) =
			(0u128, 0u128, 0u128, 0u128);
		for who in frame_system::Account::<Ct>::iter_keys() {
			for hold in pallet_balances::Holds::<Ct>::get(&who) {
				if hold.id == reg_id {
					reg_held += hold.amount;
				} else if hold.id == hrmp_id {
					hrmp_held += hold.amount;
				} else if hold.id == proxy_id {
					proxy_held += hold.amount;
				} else if hold.id == unattributed_id {
					unattributed_held += hold.amount;
				}
			}
		}
		// The migrator no longer re-labels these holds: it releases them and the owning pallet
		// takes its own `Consideration` at this chain's rates. A remaining hold under the
		// migrator's reason would mean a record never reached its pallet.
		assert_eq!(reg_held, 0, "no migrator-held registrar deposit may remain");
		assert_eq!(hrmp_held, 0, "no migrator-held HRMP deposit may remain");

		// What the pallets took instead. Priced at this chain's rates, so it is not comparable
		// to the relay chain's recorded amounts — only its existence and attribution are.
		let mut pallet_held = 0u128;
		for who in frame_system::Account::<Ct>::iter_keys() {
			for hold in pallet_balances::Holds::<Ct>::get(&who) {
				let is_control_plane = matches!(
					hold.id,
					coretime_polkadot_runtime::RuntimeHoldReason::RegistrarPara(_) |
						coretime_polkadot_runtime::RuntimeHoldReason::HrmpPara(_)
				);
				if is_control_plane {
					pallet_held += hold.amount;
				}
			}
		}
		assert!(pallet_held > 0, "the control-plane pallets must hold their own deposits");
		println!("control-plane deposits held on CT: {:.4} DOT", dot(pallet_held));
		// Every migrated proxy deposit is resized (released and re-reserved at this chain's
		// rates) when its definitions arrive; a remaining hold would mean defs never followed.
		assert_eq!(proxy_held, 0, "no unresized proxy deposit may remain");
		// The RC's anomalous reserves (backed by no deposit record) must arrive parked rather
		// than stay behind; the snapshot is known to carry some.
		assert!(unattributed_held > 0, "unattributed reserves must arrive parked on CT");
		println!("unattributed reserves parked on CT: {:.4} DOT", dot(unattributed_held));

		assert_eq!(CtMintedTotal::<Ct>::get(), migrated_ct, "CT minted exactly the CT-bound burn");
		assert_eq!(pallet_balances::TotalIssuance::<Ct>::get(), ct_ti_before + migrated_ct);

		// AND every portable definition was recreated in the REAL proxy pallet, so keyless
		// (pure) delegators can dispatch here from day one.
		assert!(FailedProxies::<Ct>::iter().next().is_none(), "no proxy set may fail");
		for who in &ct_bound_proxies {
			assert!(
				!pallet_proxy::Proxies::<Ct>::get(who).0.is_empty(),
				"delegator {who:?} must have proxies on CT"
			);
		}

		// Funds-follow-control invariant: every never-signed delegator with an `Any` definition
		// that migrated moved WHOLE to this chain — its balance and an `Any` def — so the
		// delegate keeps full control. A violation would strand money nobody can use.
		for (who, had_any, _, rc_total) in &migrated_nonce0 {
			if !*had_any {
				continue;
			}
			let d = frame_system::Account::<Ct>::get(who).data;
			let before = ct_pures_before.get(who).copied().unwrap_or_default();
			assert_eq!(
				d.free + d.reserved,
				before + rc_total,
				"possible pure {who:?}'s whole RC balance must arrive on CT"
			);
			assert!(
				pallet_proxy::Proxies::<Ct>::get(who)
					.0
					.iter()
					.any(|def| def.proxy_type == coretime_polkadot_runtime::ProxyType::Any),
				"possible pure {who:?} must have an Any definition on CT"
			);
		}

		// Dispatch checks through the recreated definitions: the Any delegate acts for the
		// manager; the ParaRegistration delegate is accepted but its filter allows nothing yet.
		let (manager, any_delegate, reg_delegate) = proxy_dispatch.clone();
		let remark = || {
			Box::new(coretime_polkadot_runtime::RuntimeCall::System(
				frame_system::Call::remark_with_event { remark: b"via proxy".to_vec() },
			))
		};
		let proxy_call = |force, delegate: &AccountId32| {
			coretime_polkadot_runtime::RuntimeCall::Proxy(pallet_proxy::Call::proxy {
				real: sp_runtime::MultiAddress::Id(manager.clone()),
				force_proxy_type: force,
				call: remark(),
			})
			.dispatch(coretime_polkadot_runtime::RuntimeOrigin::signed(delegate.clone()))
		};
		let executed = |records: &[frame_system::EventRecord<_, _>]| {
			records.iter().rev().find_map(|r| match &r.event {
				coretime_polkadot_runtime::RuntimeEvent::Proxy(
					pallet_proxy::Event::ProxyExecuted { result },
				) => Some(result.clone()),
				_ => None,
			})
		};

		proxy_call(Some(coretime_polkadot_runtime::ProxyType::Any), &any_delegate)
			.expect("Any delegate may dispatch for the pure manager");
		assert_eq!(
			executed(&frame_system::Pallet::<Ct>::events()),
			Some(Ok(())),
			"the Any-proxied call must execute"
		);

		proxy_call(Some(coretime_polkadot_runtime::ProxyType::ParaRegistration), &reg_delegate)
			.expect("ParaRegistration delegate is recognised");
		assert!(
			matches!(executed(&frame_system::Pallet::<Ct>::events()), Some(Err(_))),
			"the ParaRegistration filter must allow nothing until the registrar lands"
		);
	});

	// AND Asset Hub received the teleports: issuance unchanged, the checking account (the "DOT
	// out on the RC" ledger) drained by exactly the teleported total, the sample manager's free
	// balance arrived, and nothing was trapped (asserted per block in the loop above).
	let (_, ah_free_exp) = expected_split(sample.1, sample.2);
	ah.execute_with(|| {
		assert_eq!(
			pallet_balances::TotalIssuance::<Ah>::get(),
			ah_ti_before,
			"teleports must not change AH issuance"
		);
		let checking = pallet_xcm::Pallet::<Ah>::check_account();
		let checking_now = frame_system::Account::<Ah>::get(&checking).data.free;
		assert_eq!(
			ah_checking_before - checking_now,
			tracker.ah_free,
			"AH checking account drained by exactly the teleported total"
		);
		assert_eq!(
			frame_system::Account::<Ah>::get(&sample.0).data.free,
			ah_sample_before + ah_free_exp,
			"the manager's free balance arrived on AH"
		);

		// The swept pots and reaped dust landed on the sweep beneficiary, exactly.
		let pots_total: u128 = sweep_pots.iter().map(|(_, amount)| amount).sum();
		assert_eq!(
			frame_system::Account::<Ah>::get(&treasury).data.free,
			ah_arrivals_before[&treasury] + pots_total + sweep_dust,
			"pots and dust must arrive on the sweep beneficiary"
		);

		// The sibl-format sovereigns' balances arrived on the same bytes — on AH those ARE the
		// paras' sovereign accounts, so the paras regain control. The expectation includes the
		// child sovereign's AH leg, which translates to the same address.
		for (who, total) in &sibl_before {
			assert_eq!(
				frame_system::Account::<Ah>::get(who).data.free,
				ah_arrivals_before[who] + total,
				"sibl sovereign {who:?}'s balance must arrive on its AH sovereign account"
			);
		}

		// Never-signed delegators with `Any` defs went whole to CT (asserted there); the ones
		// without are multisigs, controllable on every chain by their members, so their teleport
		// here needs no per-account control check.
	});
}

/// The hand-written cross-chain call encodings must match the runtimes that receive them.
///
/// Each chain encodes the other's calls by hand, as a pallet index followed by a call index and the
/// SCALE payload (`para_control.rs` on both sides). The compiler checks none of it. Reorder a
/// `construct_runtime!`, renumber a `#[pallet::call_index]`, or change a message's shape, and every
/// message becomes an undecodable blob at the destination — which surfaces as XCM that silently
/// does nothing, not as a build failure.
///
/// So decode what each chain would actually send, using the real `RuntimeCall` of the chain that
/// receives it. These are pure encoding tests and need no snapshot.
mod call_encoding {
	use codec::{Decode, Encode};
	use coretime_polkadot_runtime::para_control::{
		HrmpRelayCalls, RegistrarRelayCalls, RelayRuntimePallets,
	};
	use hrmp_primitives::{ChannelId, MessageToRelayV1 as HrmpToRelay};
	use polkadot_runtime::para_control::{
		CoretimeRuntimePallets, HrmpParaCalls, RegistrarParaCalls,
	};
	use registrar_primitives::MessageToRelayV1 as RegistrarToRelay;
	use sp_core::H256;

	type RelayCall = polkadot_runtime::RuntimeCall;
	type CoretimeCall = coretime_polkadot_runtime::RuntimeCall;
	type AccountId = sp_runtime::AccountId32;

	const PARA: u32 = 2000;
	const OTHER: u32 = 2001;
	const MSG_ID: u64 = 7;

	fn channel() -> ChannelId {
		ChannelId { sender: PARA, recipient: OTHER }
	}

	fn manager() -> AccountId {
		AccountId::new([1u8; 32])
	}

	/// Every registrar message Coretime can send, paired with the relay call it must decode as.
	#[test]
	fn coretime_registrar_calls_decode_on_the_relay_chain() {
		let cases: Vec<(RegistrarRelayCalls, &str)> = vec![
			(
				RegistrarRelayCalls::AuthorizeCode(registrar_primitives::MessageToRelay::V1(
					RegistrarToRelay::Register {
						para_id: PARA,
						message_id: MSG_ID,
						manager: manager(),
						genesis_head: vec![1, 2, 3],
						code_hash: H256::repeat_byte(9),
						code_len: 42,
					},
				)),
				"authorize_code",
			),
			(
				RegistrarRelayCalls::CancelAuthorization(registrar_primitives::MessageToRelay::V1(
					RegistrarToRelay::CancelRegistration { para_id: PARA, message_id: MSG_ID },
				)),
				"cancel_authorization",
			),
			(
				RegistrarRelayCalls::Deregister(registrar_primitives::MessageToRelay::V1(
					RegistrarToRelay::Deregister { para_id: PARA, message_id: MSG_ID },
				)),
				"deregister",
			),
			(
				RegistrarRelayCalls::CancelDeregistration(
					registrar_primitives::MessageToRelay::V1(
						RegistrarToRelay::CancelDeregistration { para_id: PARA, message_id: MSG_ID },
					),
				),
				"cancel_deregistration",
			),
			(
				RegistrarRelayCalls::AuthorizeCodeUpgrade(registrar_primitives::MessageToRelay::V1(
					RegistrarToRelay::AuthorizeCodeUpgrade {
						para_id: PARA,
						message_id: MSG_ID,
						code_hash: H256::repeat_byte(3),
						code_len: 11,
					},
				)),
				"authorize_code_upgrade",
			),
			(
				RegistrarRelayCalls::SetCurrentHead(registrar_primitives::MessageToRelay::V1(
					RegistrarToRelay::SetCurrentHead {
						para_id: PARA,
						message_id: MSG_ID,
						head: vec![4, 5],
					},
				)),
				"set_current_head",
			),
			(
				RegistrarRelayCalls::RemoveUpgradeCooldown(
					registrar_primitives::MessageToRelay::V1(
						RegistrarToRelay::RemoveUpgradeCooldown {
							para_id: PARA,
							message_id: MSG_ID,
						},
					),
				),
				"remove_upgrade_cooldown",
			),
		];

		for (call, expected) in cases {
			let encoded = RelayRuntimePallets::RegistrarRelay(call).encode();
			let decoded = RelayCall::decode(&mut &encoded[..])
				.unwrap_or_else(|e| panic!("{expected} does not decode on the relay chain: {e:?}"));
			let RelayCall::RegistrarRelay(inner) = decoded else {
				panic!("{expected} decoded into the wrong pallet: {decoded:?}");
			};
			let got = match inner {
				pallet_registrar_relay::Call::authorize_code { .. } => "authorize_code",
				pallet_registrar_relay::Call::cancel_authorization { .. } =>
					"cancel_authorization",
				pallet_registrar_relay::Call::deregister { .. } => "deregister",
				pallet_registrar_relay::Call::cancel_deregistration { .. } =>
					"cancel_deregistration",
				pallet_registrar_relay::Call::authorize_code_upgrade { .. } =>
					"authorize_code_upgrade",
				pallet_registrar_relay::Call::set_current_head { .. } => "set_current_head",
				pallet_registrar_relay::Call::remove_upgrade_cooldown { .. } =>
					"remove_upgrade_cooldown",
				other => panic!("{expected} decoded as an unexpected call: {other:?}"),
			};
			assert_eq!(got, expected, "call index drift: {expected} arrives as {got}");
		}
	}

	/// Every HRMP message Coretime can send, paired with the relay call it must decode as.
	#[test]
	fn coretime_hrmp_calls_decode_on_the_relay_chain() {
		let cases: Vec<(HrmpRelayCalls, &str)> = vec![
			(
				HrmpRelayCalls::InitOpenChannel(hrmp_primitives::MessageToRelay::V1(
					HrmpToRelay::InitOpenChannel {
						channel: channel(),
						message_id: MSG_ID,
						max_capacity: 8,
						max_message_size: 1024,
					},
				)),
				"init_open_channel",
			),
			(
				HrmpRelayCalls::AcceptOpenChannel(hrmp_primitives::MessageToRelay::V1(
					HrmpToRelay::AcceptOpenChannel { channel: channel(), message_id: MSG_ID },
				)),
				"accept_open_channel",
			),
			(
				HrmpRelayCalls::CloseChannel(hrmp_primitives::MessageToRelay::V1(
					HrmpToRelay::CloseChannel {
						channel: channel(),
						message_id: MSG_ID,
						initiator: PARA,
					},
				)),
				"close_channel",
			),
			(
				HrmpRelayCalls::CancelOpenRequest(hrmp_primitives::MessageToRelay::V1(
					HrmpToRelay::CancelOpenRequest { channel: channel(), message_id: MSG_ID },
				)),
				"cancel_open_request",
			),
			(
				HrmpRelayCalls::EstablishSystemChannel(hrmp_primitives::MessageToRelay::V1(
					HrmpToRelay::EstablishSystemChannel { channel: channel(), message_id: MSG_ID },
				)),
				"establish_system_channel",
			),
		];

		for (call, expected) in cases {
			let encoded = RelayRuntimePallets::HrmpRelay(call).encode();
			let decoded = RelayCall::decode(&mut &encoded[..])
				.unwrap_or_else(|e| panic!("{expected} does not decode on the relay chain: {e:?}"));
			let RelayCall::HrmpRelay(inner) = decoded else {
				panic!("{expected} decoded into the wrong pallet: {decoded:?}");
			};
			let got = match inner {
				pallet_hrmp_relay::Call::init_open_channel { .. } => "init_open_channel",
				pallet_hrmp_relay::Call::accept_open_channel { .. } => "accept_open_channel",
				pallet_hrmp_relay::Call::close_channel { .. } => "close_channel",
				pallet_hrmp_relay::Call::cancel_open_request { .. } => "cancel_open_request",
				pallet_hrmp_relay::Call::establish_system_channel { .. } =>
					"establish_system_channel",
				other => panic!("{expected} decoded as an unexpected call: {other:?}"),
			};
			assert_eq!(got, expected, "call index drift: {expected} arrives as {got}");
		}
	}

	/// The relay chain's replies. Both pallets take every response through a single `receive`, so
	/// what matters here is that the pallet and call indices land, and that the payload survives.
	#[test]
	fn relay_reports_decode_on_coretime() {
		let registrar_report = registrar_primitives::MessageToPara::V1(
			registrar_primitives::MessageToParaV1::RegisterResponse {
				para_id: PARA,
				message_id: MSG_ID,
				outcome: Err(registrar_primitives::FailureReason::CannotUpgrade),
			},
		);
		let encoded = CoretimeRuntimePallets::RegistrarPara(RegistrarParaCalls::Receive(
			registrar_report.clone(),
		))
		.encode();
		match CoretimeCall::decode(&mut &encoded[..]).expect("registrar report does not decode") {
			CoretimeCall::RegistrarPara(pallet_registrar_para::Call::receive { message }) =>
				assert_eq!(message, registrar_report),
			other => panic!("registrar report decoded as {other:?}"),
		}

		let hrmp_report =
			hrmp_primitives::MessageToPara::V1(hrmp_primitives::MessageToParaV1::OpenResponse {
				channel: channel(),
				message_id: MSG_ID,
				outcome: Ok(()),
			});
		let encoded =
			CoretimeRuntimePallets::HrmpPara(HrmpParaCalls::Receive(hrmp_report.clone())).encode();
		match CoretimeCall::decode(&mut &encoded[..]).expect("HRMP report does not decode") {
			CoretimeCall::HrmpPara(pallet_hrmp_para::Call::receive { message }) =>
				assert_eq!(message, hrmp_report),
			other => panic!("HRMP report decoded as {other:?}"),
		}
	}
}
