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

//! Unit tests for the manager multisig.
//!
//! The contract: only a member's own signature over (who, call, round) is a vote, a call is
//! dispatched as the multisig's account exactly when the threshold is met, and a dispatch ends the
//! round so nothing signed for it can be replayed.

use super::{mock::*, *};
use frame_support::assert_ok;
use sp_core::Pair;
use sp_runtime::{transaction_validity::TransactionValidityError, DispatchError};

type Multisig = ManagerMultisig<Test>;

#[test]
fn threshold_dispatches_as_the_multisig_and_advances_the_round() {
	new_test_ext().execute_with(|| {
		let (alice, alice_id) = member(1); // multisig member
		let (bob, bob_id) = member(2); // multisig member
		let (_carol, carol_id) = member(3); // multisig member who never votes
		MultisigMembers::set(vec![alice_id.clone(), bob_id.clone(), carol_id]);

		// GIVEN the runtime upgrade seeded this network's round.
		Multisig::init_round();
		assert_eq!(ManagerMultisigRound::<Test>::get(), MultisigStartRound::get());
		let call = remark(1);

		// WHEN one member votes. THEN the vote is recorded and nothing is dispatched.
		let (payload, sig) = vote(&alice, call.clone());
		assert_ok!(Multisig::vote(&payload, &sig));
		assert_eq!(votes_for(&call), vec![alice_id.clone()]);
		assert_eq!(migrator_events(), vec![Event::ManagerMultisigVoted { votes: 1 }]);
		assert_eq!(remark_senders(), vec![]);

		// WHEN the same member votes again for the same call. THEN it is refused.
		let (payload, sig) = vote(&alice, call.clone());
		assert_eq!(Multisig::vote(&payload, &sig), Err(Error::DuplicateVote));

		// WHEN a second member votes. THEN the threshold is met, the call is dispatched as the
		// multisig's account, and the round advances with its bookkeeping cleared.
		let (payload, sig) = vote(&bob, call.clone());
		assert_ok!(Multisig::vote(&payload, &sig));
		assert_eq!(remark_senders(), vec![Multisig::manager_multisig_id()]);
		assert_eq!(migrator_events(), vec![Event::ManagerMultisigDispatched { res: Ok(()) }]);
		assert_eq!(ManagerMultisigRound::<Test>::get(), MultisigStartRound::get() + 1);
		assert_eq!(ManagerMultisigs::<Test>::iter().count(), 0);
		assert_eq!(ManagerVotesInCurrentRound::<Test>::iter().count(), 0);

		// WHEN a vote from the previous round arrives. THEN it is refused as stale, at the pool
		// and at dispatch.
		let (payload, sig) = vote_in_round(&alice, call, MultisigStartRound::get());
		assert_eq!(
			Multisig::validate_unsigned(&payload, &sig),
			Err(TransactionValidityError::Invalid(InvalidTransaction::Stale))
		);
		assert_eq!(Multisig::vote(&payload, &sig), Err(Error::UnsignedValidationFailed));

		// AND the round is seeded once: a second upgrade does not reset it.
		Multisig::init_round();
		assert_eq!(ManagerMultisigRound::<Test>::get(), MultisigStartRound::get() + 1);
	});
}

#[test]
fn a_failed_dispatch_still_ends_the_round() {
	new_test_ext().execute_with(|| {
		let (alice, alice_id) = member(1); // multisig member
		let (bob, bob_id) = member(2); // multisig member
		MultisigMembers::set(vec![alice_id, bob_id]);
		Multisig::init_round();

		// GIVEN a call the multisig's signed origin may not dispatch.
		let call = RuntimeCall::System(frame_system::Call::<Test>::set_code { code: vec![] });

		// WHEN the threshold is met. THEN the failure is reported, not swallowed, and the round
		// still moves on so the same signatures cannot be resubmitted.
		for pair in [&alice, &bob] {
			let (payload, sig) = vote(pair, call.clone());
			assert_ok!(Multisig::vote(&payload, &sig));
		}
		assert!(migrator_events()
			.contains(&Event::ManagerMultisigDispatched { res: Err(DispatchError::BadOrigin) }));
		assert_eq!(ManagerMultisigRound::<Test>::get(), MultisigStartRound::get() + 1);
		assert_eq!(ManagerMultisigs::<Test>::iter().count(), 0);
	});
}

#[test]
fn only_members_with_a_valid_signature_may_vote() {
	new_test_ext().execute_with(|| {
		let (alice, alice_id) = member(1); // multisig member
		let (mallory, _) = member(9); // not a member
		MultisigMembers::set(vec![alice_id.clone()]);
		Multisig::init_round();
		let call = remark(1);

		// WHEN a non-member votes. THEN it is refused.
		let (payload, sig) = vote(&mallory, call.clone());
		assert_eq!(
			Multisig::validate_unsigned(&payload, &sig),
			Err(TransactionValidityError::Invalid(InvalidTransaction::BadSigner))
		);
		assert_eq!(Multisig::vote(&payload, &sig), Err(Error::UnsignedValidationFailed));

		// WHEN a member's vote carries somebody else's signature. THEN it is refused.
		let (payload, _) = vote(&alice, call.clone());
		let forged = MultiSignature::Sr25519(mallory.sign(&payload.encode_with_bytes_wrapper()));
		assert_eq!(
			Multisig::validate_unsigned(&payload, &forged),
			Err(TransactionValidityError::Invalid(InvalidTransaction::BadProof))
		);
		assert_eq!(Multisig::vote(&payload, &forged), Err(Error::UnsignedValidationFailed));

		// WHEN a member signs the bare payload without the wallet wrapper. THEN it is refused:
		// what is verified is exactly what `signRaw` produces.
		let bare = MultiSignature::Sr25519(alice.sign(&payload.encode()));
		assert_eq!(
			Multisig::validate_unsigned(&payload, &bare),
			Err(TransactionValidityError::Invalid(InvalidTransaction::BadProof))
		);

		// WHEN the member's own vote arrives. THEN it is valid, tagged so the pool keeps one per
		// member, and counted.
		let (payload, sig) = vote(&alice, call);
		let valid = Multisig::validate_unsigned(&payload, &sig).unwrap();
		assert_eq!(
			valid.provides,
			vec![("Ahm2Multisig", vec![("ahm2_multi", alice_id.clone()).encode()]).encode()]
		);
		assert_eq!(valid.longevity, 30);
		assert_ok!(Multisig::vote(&payload, &sig));
		assert_eq!(ManagerVotesInCurrentRound::<Test>::get(&alice_id), 1);
	});
}

#[test]
fn votes_per_round_are_capped_and_reset_on_dispatch() {
	new_test_ext().execute_with(|| {
		let (alice, alice_id) = member(1); // multisig member
		let (bob, bob_id) = member(2); // multisig member
		MultisigMembers::set(vec![alice_id.clone(), bob_id]);
		Multisig::init_round();

		// GIVEN a member that has used every vote this round (3 in this mock), on distinct calls.
		for n in 1..=3 {
			let (payload, sig) = vote(&alice, remark(n));
			assert_ok!(Multisig::vote(&payload, &sig));
		}
		assert_eq!(ManagerVotesInCurrentRound::<Test>::get(&alice_id), 3);

		// WHEN it votes once more. THEN it is refused at the pool and at dispatch.
		let (payload, sig) = vote(&alice, remark(4));
		assert_eq!(
			Multisig::validate_unsigned(&payload, &sig),
			Err(TransactionValidityError::Invalid(InvalidTransaction::Stale))
		);
		assert_eq!(Multisig::vote(&payload, &sig), Err(Error::UnsignedValidationFailed));

		// WHEN another member completes one of its calls. THEN the round ends and the member
		// can vote again.
		let (payload, sig) = vote(&bob, remark(2));
		assert_ok!(Multisig::vote(&payload, &sig));
		assert_eq!(remark_senders(), vec![Multisig::manager_multisig_id()]);
		assert_eq!(ManagerVotesInCurrentRound::<Test>::get(&alice_id), 0);
		let (payload, sig) = vote(&alice, remark(4));
		assert_ok!(Multisig::vote(&payload, &sig));
	});
}
