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

//! The manager multisig: `Config::MultisigMembers` vote by unsigned extrinsic, and once
//! `Config::MultisigThreshold` of them have voted for the same call it is dispatched as the
//! multisig's account. Members need no funded account on this chain, which is the point: this
//! chain is being drained.
//!
//! A vote is signed offline over (who, call, round), with the `<Bytes>` wrapper that wallets
//! prepend to `signRaw`. The round advances on every dispatch, so an old round's signatures
//! cannot be replayed; each network starts its counter at `Config::MultisigStartRound`, so two
//! chains at the same round do not accept each other's votes. Each member has
//! `Config::MultisigMaxVotesPerRound` votes per round and one vote in the pool at a time, which
//! bounds what the unsigned path lets one member put into blocks.
//!
//! The `vote_manager_multisig` call, its `ValidateUnsigned`, a call that ends a stuck round, and
//! accepting the multisig's account wherever the manager is accepted all live in `lib.rs`
//! (TODO). [`ManagerMultisig::vote`], [`ManagerMultisig::validate_unsigned`] and
//! [`ManagerMultisig::end_round`] are what they invoke.

#[cfg(test)]
mod tests;

use crate::{
	Config, Error, Event, ManagerMultisigRound, ManagerMultisigs, ManagerVotesInCurrentRound,
	Pallet,
};
use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode};
use core::marker::PhantomData;
use frame_support::{
	ensure, traits::Get, CloneNoBound, DebugNoBound, EqNoBound, PalletId, PartialEqNoBound,
};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{AccountIdConversion, Dispatchable, Hash, IdentifyAccount, Verify},
	transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionValidity, ValidTransaction,
	},
	AccountId32, DispatchResult, MultiSignature, MultiSigner,
};

/// One member's vote for `call`, signed offline and submitted by anyone.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	DebugNoBound,
	CloneNoBound,
	PartialEqNoBound,
	EqNoBound,
	TypeInfo,
)]
#[scale_info(skip_type_params(T))]
pub struct ManagerMultisigVote<T: Config> {
	pub who: MultiSigner,
	pub call: <T as frame_system::Config>::RuntimeCall,
	pub round: u32,
}

impl<T: Config> ManagerMultisigVote<T> {
	/// The bytes a member signs. The wrapper is what wallet `signRaw` prepends.
	pub fn encode_with_bytes_wrapper(&self) -> Vec<u8> {
		(b"<Bytes>", self, b"</Bytes>").encode()
	}
}

/// How the pool reports a vote that [`ManagerMultisig::vote`] would refuse with `e`.
pub(crate) fn invalid<T: Config>(e: &Error<T>) -> InvalidTransaction {
	match e {
		Error::NotMultisigMember => InvalidTransaction::BadSigner,
		Error::BadMultisigSignature => InvalidTransaction::BadProof,
		_ => InvalidTransaction::Stale,
	}
}

pub struct ManagerMultisig<T>(PhantomData<T>);

impl<T: Config> ManagerMultisig<T> {
	/// The account the manager multisig dispatches as, once it reaches its threshold.
	// TODO(ahm-v2): `ensure_admin_or_manager` accepts a signed origin of this account.
	pub fn manager_multisig_id() -> T::AccountId {
		PalletId(*b"rc2migmt").into_account_truncating()
	}

	/// Vote on behalf of any of the members in `MultisigMembers`.
	///
	/// Each vote adds the member to `ManagerMultisigs` under the call's hash. Once
	/// [`Config::MultisigThreshold`] members have voted for the same call it is dispatched as the
	/// multisig's account and the round ends.
	// TODO(ahm-v2): `vote_manager_multisig` (call index 7) does `ensure_none` and then this.
	pub fn vote(payload: &ManagerMultisigVote<T>, sig: &MultiSignature) -> DispatchResult {
		let call_hash = T::Hashing::hash_of(&payload.call);
		let who = Self::check(payload, sig, &call_hash)?;
		let mut votes_for_call = ManagerMultisigs::<T>::get(call_hash);
		votes_for_call.push(who.clone());
		let votes = votes_for_call.len() as u32;
		Pallet::<T>::deposit_event(Event::ManagerMultisigVoted {
			who: who.clone(),
			call_hash,
			votes,
		});

		if votes < T::MultisigThreshold::get() {
			ManagerVotesInCurrentRound::<T>::mutate(&who, |n| *n = n.saturating_add(1));
			ManagerMultisigs::<T>::insert(call_hash, votes_for_call);
			return Ok(());
		}

		// Dispatched like any signed call, so it goes through the origin's call filter: the
		// lockdown has to leave this pallet's calls open.
		let res = payload
			.call
			.clone()
			.dispatch(frame_system::RawOrigin::Signed(Self::manager_multisig_id()).into());
		Pallet::<T>::deposit_event(Event::ManagerMultisigDispatched {
			call_hash,
			res: res.map(|_| ()).map_err(|e| e.error),
		});
		Self::end_round();
		Ok(())
	}

	/// End the current round: forget every vote and advance the counter, so nothing signed for
	/// it can be replayed. Runs after a threshold dispatch, and by hand for a round that can no
	/// longer reach the threshold because too few members have votes left.
	// TODO(ahm-v2): `end_manager_multisig_round` lets the admin or the manager call this, so
	// the manager can unstick a round without a referendum.
	pub fn end_round() {
		let _ = ManagerMultisigs::<T>::clear(u32::MAX, None);
		let _ = ManagerVotesInCurrentRound::<T>::clear(u32::MAX, None);
		let round = ManagerMultisigRound::<T>::mutate(|round| {
			let ended = *round;
			*round = round.saturating_add(1);
			ended
		});
		Pallet::<T>::deposit_event(Event::ManagerMultisigRoundEnded { round });
	}

	/// Whether an unsigned vote may enter the pool: what [`Self::check`] accepts, tagged so the
	/// pool holds one pending vote per member, at the highest priority.
	// TODO(ahm-v2): the `#[pallet::validate_unsigned]` impl delegates `vote_manager_multisig` here.
	pub fn validate_unsigned(
		payload: &ManagerMultisigVote<T>,
		sig: &MultiSignature,
	) -> TransactionValidity {
		let call_hash = T::Hashing::hash_of(&payload.call);
		let account = Self::check(payload, sig, &call_hash).map_err(|e| invalid(&e))?;

		ValidTransaction::with_tag_prefix("Ahm2Multisig")
			.priority(TransactionPriority::MAX)
			.and_provides(account)
			.propagate(true)
			.longevity(30)
			.build()
	}

	/// What makes a vote valid, for the pool and for dispatch alike: a member's own signature
	/// over the wrapped payload, for the current round, with a vote left this round and none
	/// yet for this call. Returns the member.
	fn check(
		payload: &ManagerMultisigVote<T>,
		sig: &MultiSignature,
		call_hash: &T::Hash,
	) -> Result<AccountId32, Error<T>> {
		let who = payload.who.clone().into_account();
		ensure!(T::MultisigMembers::get().contains(&who), Error::<T>::NotMultisigMember);
		ensure!(
			sig.verify(&payload.encode_with_bytes_wrapper()[..], &who),
			Error::<T>::BadMultisigSignature
		);
		ensure!(ManagerMultisigRound::<T>::get() == payload.round, Error::<T>::MultisigRoundStale);
		ensure!(
			ManagerVotesInCurrentRound::<T>::get(&who) < T::MultisigMaxVotesPerRound::get(),
			Error::<T>::MultisigMaxVotesPerRound
		);
		ensure!(
			!ManagerMultisigs::<T>::get(call_hash).contains(&who),
			Error::<T>::MultisigDuplicateVote
		);
		Ok(who)
	}
}
