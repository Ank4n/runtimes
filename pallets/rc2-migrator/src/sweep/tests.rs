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

//! Unit tests for the sweep stage.
//!
//! The stage's contract: every configured pot ends at zero with its inactive issuance
//! reactivated, every reapable below-ED account and husk is gone, module accounts and referenced
//! records survive, and the conservation ledger books exactly what was burned. The tests pin that
//! contract with exact values; Asset Hub appears only as the returned amounts.

use super::{mock::*, *};
use frame_support::hypothetically;

type Migrator = SweepMigrator<Test>;

/// The ledger must account for every planck: what is still on this chain is `kept`, and what
/// the sweep burned is booked as teleported.
fn assert_ledger(kept: u128, ah_free: u128) {
	let ledger = RcMigratedBalance::<Test>::get();
	assert_eq!(ledger, MigratedBalances { kept, ah_free, ..Default::default() });
	assert_eq!(total_issuance(), kept);
}

#[test]
fn sweep_pots_empties_every_configured_pot_and_reactivates_its_issuance() {
	new_test_ext().execute_with(|| {
		let treasury = pot(); // book-kept as inactive issuance
		let empty = acc(9); // configured, but holds nothing
		SweepAccounts::set(vec![treasury.clone(), empty]);

		// GIVEN the treasury pot, whose balance the balances pallet counts as inactive.
		fund(&treasury, 500);
		pallet_balances::InactiveIssuance::<Test>::put(500);
		seed_ledger();

		// WHEN the pots are swept.
		assert_eq!(Migrator::sweep_pots(), Ok(500));

		// THEN the pot is gone, its issuance is active again, and the ledger books the 500 as
		// teleported. The empty pot is skipped without an event.
		assert!(!exists(&treasury));
		assert_eq!(pallet_balances::InactiveIssuance::<Test>::get(), 0);
		assert_ledger(0, 500);
		assert_eq!(migrator_events(), vec![Event::AccountSwept { who: treasury, amount: 500 }]);

		// Hypothetically, a pot that was never deactivated: reactivating saturates at zero
		// instead of underflowing.
		hypothetically!({
			let other = acc(8);
			SweepAccounts::set(vec![other.clone()]);
			fund(&other, 300);
			seed_ledger();

			assert_eq!(Migrator::sweep_pots(), Ok(300));
			assert_eq!(pallet_balances::InactiveIssuance::<Test>::get(), 0);
			assert_ledger(0, 300);
		});
	});
}

#[test]
fn sweep_dust_reaps_below_ed_accounts_and_husks_and_leaves_the_rest() {
	new_test_ext().execute_with(|| {
		let alice = acc(1); // regular account at the ED: not dust
		let dusty = acc(9); // reapable below-ED account
		let backed_dust = acc(14); // below-ED with a broken reserve (holds a consumer ref)
		let husk = acc(15); // zero balance, alive only via a stale provider ref
		let referenced_husk = acc(16); // zero balance, but something still references it
		let modl_dust = {
			let mut bytes = [0u8; 32];
			bytes[..8].copy_from_slice(b"modlxyz\0");
			AccountId32::new(bytes)
		};

		// GIVEN one account of every shape the dust pass has to tell apart.
		fund(&alice, ED);
		force_anomalous_account(&dusty, 4, 0, 0);
		force_anomalous_account(&backed_dust, 2, 3, 1);
		force_anomalous_account(&husk, 0, 0, 0);
		force_anomalous_account(&referenced_husk, 0, 0, 1);
		force_anomalous_account(&modl_dust, 4, 0, 0);
		seed_ledger();
		let kept_before = total_issuance();

		// WHEN one block of the dust pass runs (everything fits one page).
		let block = Migrator::sweep_dust(None).unwrap();

		// THEN the dust and the husk are gone — the backed dust with its consumer reference —
		// and 4 + 5 planck are burned and booked as teleported.
		assert_eq!(block, BlockSweep { amount: 4 + 5, last_key: None });
		assert!(!exists(&dusty));
		assert!(!exists(&backed_dust));
		assert!(!exists(&husk));
		assert_ledger(kept_before - 9, 9);
		assert_eq!(
			migrator_events(),
			vec![Event::HusksReaped { count: 1 }, Event::DustSwept { count: 2, amount: 9 }]
		);

		// AND everything else survives: an account at the ED, a module account's dust (the sweep
		// only empties the configured pots), and a zero-balance record something still
		// references.
		assert_eq!(free(&alice), ED);
		assert_eq!(free(&modl_dust), 4);
		assert!(exists(&referenced_husk));
		assert_eq!(frame_system::Pallet::<Test>::consumers(&referenced_husk), 1);
	});
}

#[test]
fn sweep_dust_is_paged_at_the_block_limit() {
	new_test_ext().execute_with(|| {
		let count = MAX_SWEPT_PER_BLOCK + 1;

		// GIVEN one more dust account than a block scans.
		for n in 0..count {
			let mut bytes = [0u8; 32];
			bytes[..4].copy_from_slice(&n.to_le_bytes());
			force_anomalous_account(&AccountId32::new(bytes), 1, 0, 0);
		}
		seed_ledger();

		// WHEN the pass runs block by block.
		let first = Migrator::sweep_dust(None).unwrap();
		assert_eq!(first.amount, MAX_SWEPT_PER_BLOCK as u128);
		assert!(first.last_key.is_some());

		let second = Migrator::sweep_dust(first.last_key).unwrap();

		// THEN the second block picks up exactly where the first stopped and exhausts the map,
		// with the ledger exact after both.
		assert_eq!(second, BlockSweep { amount: 1, last_key: None });
		assert_eq!(frame_system::Account::<Test>::iter().count(), 0);
		assert_ledger(0, count as u128);
	});
}
