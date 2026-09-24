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
//!
//! The mock's members are Alice, Bob and Charlie from the dev keyring, with a threshold of 2.

use super::*;
use crate::mock::*;
use frame_support::{assert_noop, assert_ok};
use sp_keyring::Sr25519Keyring::{self, Alice, Bob, Eve};
use sp_runtime::{transaction_validity::TransactionValidityError, DispatchError};

type Multisig = ManagerMultisig<Test>;

/// `who`'s signed vote for `call` in `round`.
fn vote_in_round(
	who: Sr25519Keyring,
	call: RuntimeCall,
	round: u32,
) -> (ManagerMultisigVote<Test>, MultiSignature) {
	let payload =
		ManagerMultisigVote::<Test> { who: MultiSigner::Sr25519(who.public()), call, round };
	let sig = MultiSignature::Sr25519(who.sign(&payload.encode_with_bytes_wrapper()));
	(payload, sig)
}

/// `who`'s signed vote for `call` in the current round.
fn vote(who: Sr25519Keyring, call: RuntimeCall) -> (ManagerMultisigVote<Test>, MultiSignature) {
	vote_in_round(who, call, ManagerMultisigRound::<Test>::get())
}

/// Sign `who`'s vote for `call` and submit it.
fn cast(who: Sr25519Keyring, call: RuntimeCall) -> DispatchResult {
	let (payload, sig) = vote(who, call);
	Multisig::vote(&payload, &sig)
}

/// The vote is refused with `e` at the pool and at dispatch, and leaves storage untouched.
fn assert_refused(payload: &ManagerMultisigVote<Test>, sig: &MultiSignature, e: Error<Test>) {
	assert_eq!(
		Multisig::validate_unsigned(payload, sig),
		Err(TransactionValidityError::Invalid(invalid(&e)))
	);
	assert_noop!(Multisig::vote(payload, sig), e);
}

/// `ended` rounds have ended since the start, and the open one has nothing recorded against it.
fn assert_rounds_ended(ended: u32) {
	assert_eq!(ManagerMultisigRound::<Test>::get(), MultisigStartRound::get() + ended);
	assert_eq!(ManagerMultisigs::<Test>::iter().count(), 0);
	assert_eq!(ManagerVotesInCurrentRound::<Test>::iter().count(), 0);
}

/// A call any signed origin may dispatch, distinguishable by its bytes.
fn remark(n: u8) -> RuntimeCall {
	RuntimeCall::System(frame_system::Call::<Test>::remark_with_event { remark: vec![n] })
}

/// The hash the multisig keys and names a call by.
fn call_hash(call: &RuntimeCall) -> <Test as frame_system::Config>::Hash {
	<Test as frame_system::Config>::Hashing::hash_of(call)
}

/// Who `System::remark_with_event` recorded as the sender, for every remark so far.
fn remark_senders() -> Vec<AccountId> {
	System::events()
		.into_iter()
		.filter_map(|record| match record.event {
			RuntimeEvent::System(frame_system::Event::Remarked { sender, .. }) => Some(sender),
			_ => None,
		})
		.collect()
}

#[test]
fn threshold_dispatches_as_the_multisig_and_advances_the_round() {
	new_test_ext().execute_with(|| {
		// GIVEN the round counter starts where this network's config puts it.
		assert_eq!(ManagerMultisigRound::<Test>::get(), MultisigStartRound::get());
		let call = remark(1);

		// WHEN one member votes. THEN the vote is recorded and nothing is dispatched.
		let (payload, sig) = vote(Alice, call.clone());
		assert_ok!(Multisig::vote(&payload, &sig));
		assert_eq!(ManagerMultisigs::<Test>::get(call_hash(&call)), vec![Alice.to_account_id()]);
		System::assert_last_event(
			Event::ManagerMultisigVoted {
				who: Alice.to_account_id(),
				call_hash: call_hash(&call),
				votes: 1,
			}
			.into(),
		);
		assert_eq!(remark_senders(), vec![]);

		// WHEN the same vote is submitted again. THEN it is refused, and it does not use up one
		// of the member's votes.
		assert_refused(&payload, &sig, Error::MultisigDuplicateVote);

		// WHEN a second member votes. THEN the threshold is met, the call is dispatched as the
		// multisig's account, and the round advances with its bookkeeping cleared.
		assert_ok!(cast(Bob, call.clone()));
		assert_eq!(remark_senders(), vec![Multisig::manager_multisig_id()]);
		System::assert_has_event(
			Event::ManagerMultisigVoted {
				who: Bob.to_account_id(),
				call_hash: call_hash(&call),
				votes: 2,
			}
			.into(),
		);
		System::assert_has_event(
			Event::ManagerMultisigDispatched { call_hash: call_hash(&call), res: Ok(()) }.into(),
		);
		System::assert_last_event(
			Event::ManagerMultisigRoundEnded { round: MultisigStartRound::get() }.into(),
		);
		assert_rounds_ended(1);

		// WHEN a vote from the previous round arrives. THEN it is refused as stale.
		let (payload, sig) = vote_in_round(Alice, call, MultisigStartRound::get());
		assert_refused(&payload, &sig, Error::MultisigRoundStale);
	});
}

#[test]
fn a_failed_dispatch_still_ends_the_round() {
	new_test_ext().execute_with(|| {
		// GIVEN a call the multisig's signed origin may not dispatch.
		let call = RuntimeCall::System(frame_system::Call::<Test>::set_code { code: vec![] });

		// WHEN the threshold is met. THEN the failure is reported, not swallowed, and the round
		// still moves on so the same signatures cannot be resubmitted.
		assert_ok!(cast(Alice, call.clone()));
		assert_ok!(cast(Bob, call.clone()));
		System::assert_has_event(
			Event::ManagerMultisigDispatched {
				call_hash: call_hash(&call),
				res: Err(DispatchError::BadOrigin),
			}
			.into(),
		);
		assert_rounds_ended(1);
	});
}

#[test]
fn only_members_with_a_valid_signature_may_vote() {
	new_test_ext().execute_with(|| {
		let call = remark(1);

		// WHEN a non-member votes. THEN it is refused.
		let (payload, sig) = vote(Eve, call.clone());
		assert_refused(&payload, &sig, Error::NotMultisigMember);

		// WHEN a member's vote carries somebody else's signature. THEN it is refused.
		let (payload, _) = vote(Alice, call.clone());
		let forged = MultiSignature::Sr25519(Eve.sign(&payload.encode_with_bytes_wrapper()));
		assert_refused(&payload, &forged, Error::BadMultisigSignature);

		// WHEN a member signs the bare payload without the wallet wrapper. THEN it is refused:
		// what is verified is exactly what `signRaw` produces.
		let bare = MultiSignature::Sr25519(Alice.sign(&payload.encode()));
		assert_refused(&payload, &bare, Error::BadMultisigSignature);

		// WHEN the member's own vote arrives. THEN it is valid, tagged so the pool keeps one per
		// member, and counted.
		let (payload, sig) = vote(Alice, call);
		let valid = Multisig::validate_unsigned(&payload, &sig).unwrap();
		assert_eq!(valid.provides, vec![("Ahm2Multisig", Alice.to_account_id()).encode()]);
		assert_eq!(valid.longevity, 30);
		assert_ok!(Multisig::vote(&payload, &sig));
		assert_eq!(ManagerVotesInCurrentRound::<Test>::get(Alice.to_account_id()), 1);
	});
}

#[test]
fn votes_per_round_are_capped_and_reset_on_dispatch() {
	new_test_ext().execute_with(|| {
		// GIVEN a member that has used every vote this round (3 in this mock), on distinct calls.
		for n in 1..=3 {
			assert_ok!(cast(Alice, remark(n)));
		}
		assert_eq!(ManagerVotesInCurrentRound::<Test>::get(Alice.to_account_id()), 3);

		// WHEN it votes once more. THEN it is refused.
		let (payload, sig) = vote(Alice, remark(4));
		assert_refused(&payload, &sig, Error::MultisigMaxVotesPerRound);

		// WHEN another member completes one of its calls. THEN the round ends and the member
		// can vote again.
		assert_ok!(cast(Bob, remark(2)));
		assert_eq!(remark_senders(), vec![Multisig::manager_multisig_id()]);
		assert_rounds_ended(1);
		assert_ok!(cast(Alice, remark(4)));
	});
}

#[test]
fn a_round_that_cannot_reach_the_threshold_is_ended_by_hand() {
	new_test_ext().execute_with(|| {
		// GIVEN only two members, and both spent every vote on calls the other did not vote
		// for. Nothing can reach the threshold of 2 now, and neither has a vote left to change
		// that.
		MultisigMembers::set(vec![Alice.to_account_id(), Bob.to_account_id()]);
		for (who, calls) in [(Alice, 1..=3), (Bob, 4..=6)] {
			for n in calls {
				assert_ok!(cast(who, remark(n)));
			}
		}
		let (payload, sig) = vote(Alice, remark(4));
		assert_refused(&payload, &sig, Error::MultisigMaxVotesPerRound);

		// WHEN the round is ended by hand. THEN every vote is forgotten, the counter advances,
		// and the members can vote again.
		Multisig::end_round();
		System::assert_last_event(
			Event::ManagerMultisigRoundEnded { round: MultisigStartRound::get() }.into(),
		);
		assert_rounds_ended(1);
		assert_ok!(cast(Alice, remark(4)));
		assert_eq!(
			ManagerMultisigs::<Test>::get(call_hash(&remark(4))),
			vec![Alice.to_account_id()]
		);
	});
}
