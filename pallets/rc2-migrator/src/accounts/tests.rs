// Copyright (C) Polkadot Fellows.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot. If not, see <http://www.gnu.org/licenses/>.

use super::*;
use crate::mock::*;
use sp_runtime::AccountId32;

type Migrator = AccountsMigrator<Test>;

fn total_issuance() -> u128 {
	pallet_balances::TotalIssuance::<Test>::get()
}

/// The `i`th of many accounts.
fn account(i: u32) -> AccountId {
	let mut id = [0u8; 32];
	id[..4].copy_from_slice(&i.to_le_bytes());
	AccountId32::new(id)
}

/// One account per row of the table in `accounts.md`.
#[test]
fn migrate_one_account_per_case() {
	new_test_ext().execute_with(|| {
		// GIVEN one account per case.
		let alice = ALICE; // 1: free balance
		fund(&alice, 100);
		let issuance = total_issuance();

		// WHEN one block withdraws them all.
		let out = Migrator::migrate_many(None);
		assert_eq!(out.last_key, None);
		let mut to_ah = out.to_ah;
		// sorting just to make it easier to assert
		to_ah.sort();

		// THEN each lands where its row says.
		assert_eq!(to_ah, vec![(alice.clone(), 100)], "case 1");
		assert!(!frame_system::Account::<Test>::contains_key(&alice), "case 1");

		// AND what was burned is exactly what left.
		let left: u128 = to_ah.iter().map(|(_, amount)| amount).sum();
		assert_eq!(total_issuance(), issuance - left);
	});
}

#[test]
fn migrate_many_resumes_from_cursor() {
	new_test_ext().execute_with(|| {
		// GIVEN one account more than a block withdraws.
		for i in 0..=MAX_ACCOUNTS_PER_BLOCK {
			fund(&account(i), 100);
		}

		// WHEN the first block runs. THEN it withdraws the limit and leaves a cursor.
		let first = Migrator::migrate_many(None);
		assert_eq!(first.to_ah.len(), MAX_ACCOUNTS_PER_BLOCK as usize);
		assert_eq!(total_issuance(), 100);
		let last_key = first.last_key.expect("one account is left");

		// WHEN the next block continues after the cursor. THEN it withdraws the rest.
		let second = Migrator::migrate_many(Some(last_key));
		assert_eq!(second.to_ah.len(), 1);
		assert_eq!(second.last_key, None);
		assert_eq!(total_issuance(), 0);
	});
}

#[test]
fn migrate_many_skips_unhandled_accounts() {
	new_test_ext().execute_with(|| {
		// GIVEN a plain account and one with a reserve.
		fund(&ALICE, 100);
		fund(&BOB, 100);
		reserve(&BOB, 40);

		// WHEN a block runs. THEN only the plain account is withdrawn; the other is untouched.
		let out = Migrator::migrate_many(None);
		assert_eq!(out.to_ah, vec![(ALICE, 100)]);
		assert_eq!(pallet_balances::Pallet::<Test>::free_balance(&BOB), 60);
		assert_eq!(pallet_balances::Pallet::<Test>::reserved_balance(&BOB), 40);
		assert_eq!(total_issuance(), 100);
	});
}
