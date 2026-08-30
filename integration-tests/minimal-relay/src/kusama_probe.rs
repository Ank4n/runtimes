// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! What is on Kusama's relay chain that Polkadot's does not have.
//!
//! Kusama carries `pallet-society`, which reserves user funds for bids and vouches. No migrator
//! touches it and the post-AHM call filter closes every Society call, so anything it holds would
//! be frozen with no way to release it. This measures the exposure against a real snapshot.
//!
//! Run with `SNAP_RC` pointed at the Kusama relay snapshot:
//! `SNAP_RC=snapshots/snap_rc_ksm.snap cargo test -p polkadot-integration-tests-minimal-relay
//!  kusama_probe -- --nocapture`

use crate::mock::{load, Chain};

type Ksm = kusama_runtime::Runtime;

fn ksm(v: u128) -> String {
	format!("{:.4} KSM", v as f64 / 1_000_000_000_000f64)
}

#[tokio::test]
async fn society_exposure_on_kusama() {
	let mut rc = load(Chain::Relay).await;

	rc.execute_with(|| {
		let members = pallet_society::Members::<Ksm>::iter().count();
		let member_count = pallet_society::MemberCount::<Ksm>::get();
		let candidates = pallet_society::Candidates::<Ksm>::iter().count();
		let bids = pallet_society::Bids::<Ksm>::get();
		let pot = pallet_society::Pot::<Ksm>::get();
		let founder = pallet_society::Founder::<Ksm>::get();

		println!("\n### pallet-society on the Kusama relay chain");
		println!("founder:      {founder:?}");
		println!("members:      {members} (MemberCount says {member_count})");
		println!("candidates:   {candidates}");
		println!("bids:         {}", bids.len());
		println!("pot:          {}", ksm(pot));

		// Payouts are what members are owed; the pallet reserves them on the member's account
		// until claimed.
		let mut payout_accounts = 0usize;
		let mut payout_total = 0u128;
		for (_who, record) in pallet_society::Payouts::<Ksm>::iter() {
			if record.paid > 0 || !record.payouts.is_empty() {
				payout_accounts += 1;
				payout_total += record.payouts.iter().map(|(_, v)| *v).sum::<u128>();
			}
		}
		println!("payouts:      {payout_accounts} accounts owed {}", ksm(payout_total));

		// The pot is an ordinary account; its free balance is what a sweep would move.
		use sp_runtime::traits::AccountIdConversion;
		let pot_account: sp_runtime::AccountId32 =
			kusama_runtime::SocietyPalletId::get().into_account_truncating();
		let pot_free = pallet_balances::Pallet::<Ksm>::free_balance(&pot_account);
		println!("pot account:  {} free", ksm(pot_free));

		println!(
			"\nVERDICT: {}",
			if members == 0 && bids.is_empty() && candidates == 0 && payout_accounts == 0 {
				"Society holds no user state. Only the pot matters, and sweeping it is enough."
			} else {
				"Society holds live user state. Reserves here are frozen by the post-AHM filter \
				 unless the migration handles them."
			}
		);
	});
}

/// What phase 2 would actually have to move off Kusama's relay chain.
///
/// The Polkadot equivalents are asserted by the migration test; Kusama has never been run, so this
/// is a census rather than a check. It exists to size the work and to surface anything Polkadot
/// does not have.
#[tokio::test]
async fn what_kusama_has_to_migrate() {
	let mut rc = load(Chain::Relay).await;

	rc.execute_with(|| {
		use polkadot_runtime_common::paras_registrar;
		use runtime_parachains::hrmp;

		println!("\n### registrar");
		let mut paras = 0usize;
		let mut registered = 0usize;
		let mut locked = 0usize;
		let mut deposits = 0u128;
		let mut unbacked = 0usize;
		let mut unbacked_value = 0u128;
		let mut owed: std::collections::BTreeMap<sp_runtime::AccountId32, u128> =
			Default::default();
		for (id, info) in paras_registrar::Paras::<Ksm>::iter() {
			paras += 1;
			deposits += info.deposit;
			if runtime_parachains::paras::Pallet::<Ksm>::lifecycle(id).is_some() {
				registered += 1;
			}
			if info.locked.unwrap_or(false) {
				locked += 1;
			}
			// Managers often hold several paras, so the backing check has to be per manager
			// against the sum, not per para.
			*owed.entry(info.manager.clone()).or_default() += info.deposit;
		}
		for (manager, want) in &owed {
			let acc = frame_system::Account::<Ksm>::get(manager);
			if *want > acc.data.reserved {
				unbacked += 1;
				unbacked_value += want.saturating_sub(acc.data.reserved);
			}
		}
		println!("paras:        {paras} ({registered} onboarded, {locked} locked)");
		println!("deposits:     {}", ksm(deposits));
		println!(
			"managers:     {} total, {unbacked} whose reserve cannot cover what the registrar \
			 records ({} short)",
			owed.len(),
			ksm(unbacked_value),
		);

		println!("\n### hrmp");
		let channels = hrmp::HrmpChannels::<Ksm>::iter().count();
		let requests = hrmp::HrmpOpenChannelRequests::<Ksm>::iter().count();
		let hrmp_deposits: u128 = hrmp::HrmpChannels::<Ksm>::iter()
			.map(|(_, c)| c.sender_deposit + c.recipient_deposit)
			.sum();
		println!("channels:     {channels}");
		println!("requests:     {requests}");
		println!("deposits:     {}", ksm(hrmp_deposits));

		println!("\n### proxies, by type");
		use std::collections::BTreeMap;
		let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
		let mut proxy_deposits = 0u128;
		let mut delegators = 0usize;
		for (_who, (defs, deposit)) in pallet_proxy::Proxies::<Ksm>::iter() {
			delegators += 1;
			proxy_deposits += deposit;
			for d in defs.iter() {
				*by_type.entry(format!("{:?}", d.proxy_type)).or_default() += 1;
			}
		}
		println!("delegators:   {delegators}, deposits {}", ksm(proxy_deposits));
		for (t, n) in &by_type {
			println!("  {t:<40} {n}");
		}

		println!("\n### balances");
		let mut accounts = 0usize;
		let mut free = 0u128;
		let mut reserved = 0u128;
		for (_who, acc) in frame_system::Account::<Ksm>::iter() {
			accounts += 1;
			free += acc.data.free;
			reserved += acc.data.reserved;
		}
		println!("accounts:     {accounts}");
		println!("free:         {}", ksm(free));
		println!("reserved:     {}", ksm(reserved));
		println!("issuance:     {}", ksm(pallet_balances::TotalIssuance::<Ksm>::get()));
	});
}
