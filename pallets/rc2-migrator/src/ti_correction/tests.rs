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

//! Unit tests for the total-issuance correction.
//!
//! The stage's contract: it burns `min(audited, measured)` of the issuance no account holds,
//! never a planck that an account does hold, reports rather than burns when the two disagree, and
//! books exactly what it burned into the conservation ledger.

use super::{mock::*, *};
use frame_support::hypothetically;

type Corrector = TiCorrector<Test>;

/// The ledger must account for every planck: what is still on this chain is `kept`, and what
/// the correction burned is `ti_corrected`.
fn assert_ledger(kept: u128, ti_corrected: u128) {
	let ledger = RcMigratedBalance::<Test>::get();
	assert_eq!(ledger, MigratedBalances { kept, ti_corrected, ..Default::default() });
	assert_eq!(total_issuance(), kept);
}

#[test]
fn burns_the_audited_phantom_and_nothing_an_account_holds() {
	new_test_ext().execute_with(|| {
		let manager = acc(1); // the one account still funded at this stage

		// GIVEN 50 planck of issuance that no account holds, beside the manager's 100.
		fund(&manager, 100);
		pallet_balances::TotalIssuance::<Test>::mutate(|ti| *ti += 50);
		TiCorrection::set(50);
		seed_ledger();

		// WHEN the correction runs with the manager's balance excluded.
		let outcome = Corrector::correct_total_issuance(100).unwrap();

		// THEN exactly the phantom is gone: the manager's balance is untouched, the ledger and
		// total issuance agree, and the event carries the three figures.
		assert_eq!(outcome, Correction { expected: 50, unaccounted: 50, burned: 50 });
		assert_eq!(total_issuance(), 100);
		assert_ledger(100, 50);
		assert_eq!(
			migrator_events(),
			vec![Event::TiCorrected { expected: 50, unaccounted: 50, burned: 50 }]
		);
	});
}

#[test]
fn never_burns_more_than_measured_and_reports_anomalies() {
	new_test_ext().execute_with(|| {
		// GIVEN a measured phantom (30) below the audited expectation (50).
		pallet_balances::TotalIssuance::<Test>::put(30);
		TiCorrection::set(50);
		seed_ledger();

		// WHEN the correction runs.
		let outcome = Corrector::correct_total_issuance(0).unwrap();

		// THEN the 30 burn and the shortfall is reported loudly.
		assert_eq!(outcome, Correction { expected: 50, unaccounted: 30, burned: 30 });
		assert_ledger(0, 30);
		assert_eq!(
			migrator_events(),
			vec![
				Event::TiCorrectionAnomaly { expected: 50, unaccounted: 30 },
				Event::TiCorrected { expected: 50, unaccounted: 30, burned: 30 },
			]
		);

		// Hypothetically, with MORE unaccounted than audited, only the audited amount burns;
		// the excess stays on the books for investigation.
		hypothetically!({
			pallet_balances::TotalIssuance::<Test>::put(80);
			seed_ledger();

			let outcome = Corrector::correct_total_issuance(0).unwrap();

			assert_eq!(outcome, Correction { expected: 50, unaccounted: 80, burned: 50 });
			assert_ledger(30, 50);
			assert_eq!(
				migrator_events(),
				vec![Event::TiCorrected { expected: 50, unaccounted: 80, burned: 50 }]
			);
		});

		// Hypothetically, with nothing audited, nothing burns whatever is measured.
		hypothetically!({
			pallet_balances::TotalIssuance::<Test>::put(80);
			TiCorrection::set(0);
			seed_ledger();

			let outcome = Corrector::correct_total_issuance(0).unwrap();

			assert_eq!(outcome, Correction { expected: 0, unaccounted: 80, burned: 0 });
			assert_ledger(80, 0);
		});
	});
}
