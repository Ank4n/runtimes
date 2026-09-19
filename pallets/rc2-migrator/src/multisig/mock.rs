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

//! Mock relay-chain runtime for the manager multisig.

use crate as pallet_rc2_migrator;
use crate::{
	multisig::{ManagerMultisigRound, ManagerMultisigVote},
	ManagerMultisigs,
};
use frame_support::{derive_impl, parameter_types};
use sp_core::{sr25519, Pair};
use sp_runtime::{
	traits::{IdentifyAccount, IdentityLookup},
	AccountId32, BuildStorage, MultiSignature, MultiSigner,
};

type Block = frame_system::mocking::MockBlockU32<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Rc2Migrator: pallet_rc2_migrator,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountId = AccountId32;
	type Lookup = IdentityLookup<AccountId32>;
	type Block = Block;
}

parameter_types! {
	/// Members of the manager multisig; set per test.
	pub static MultisigMembers: Vec<AccountId32> = vec![];
	pub const MultisigThreshold: u32 = 2;
	pub const MultisigMaxVotesPerRound: u32 = 3;
	pub const MultisigStartRound: u32 = 7;
}

impl pallet_rc2_migrator::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type MultisigMembers = MultisigMembers;
	type MultisigThreshold = MultisigThreshold;
	type MultisigMaxVotesPerRound = MultisigMaxVotesPerRound;
	type MultisigStartRound = MultisigStartRound;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	// Block 1 so deposited events are recorded.
	ext.execute_with(|| System::set_block_number(1));
	ext
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// A multisig member: its signing key and its account id.
pub fn member(n: u8) -> (sr25519::Pair, AccountId32) {
	let pair = sr25519::Pair::from_seed(&[n; 32]);
	let who = MultiSigner::Sr25519(pair.public()).into_account();
	(pair, who)
}

/// One member's signed vote for `call` in the current round, ready to submit.
pub fn vote(
	pair: &sr25519::Pair,
	call: RuntimeCall,
) -> (ManagerMultisigVote<Test>, MultiSignature) {
	vote_in_round(pair, call, ManagerMultisigRound::<Test>::get())
}

pub fn vote_in_round(
	pair: &sr25519::Pair,
	call: RuntimeCall,
	round: u32,
) -> (ManagerMultisigVote<Test>, MultiSignature) {
	let payload =
		ManagerMultisigVote::<Test>::new(MultiSigner::Sr25519(pair.public()), call, round);
	let sig = MultiSignature::Sr25519(pair.sign(&payload.encode_with_bytes_wrapper()));
	(payload, sig)
}

/// A call any signed origin may dispatch, distinguishable by its bytes.
pub fn remark(n: u8) -> RuntimeCall {
	RuntimeCall::System(frame_system::Call::<Test>::remark_with_event { remark: vec![n] })
}

pub fn votes_for(call: &RuntimeCall) -> Vec<AccountId32> {
	ManagerMultisigs::<Test>::get(call)
}

/// All `pallet-rc2-migrator` events since the last call to this function.
pub fn migrator_events() -> Vec<crate::Event<Test>> {
	let events = System::events()
		.into_iter()
		.filter_map(|r| match r.event {
			RuntimeEvent::Rc2Migrator(e) => Some(e),
			_ => None,
		})
		.collect();
	System::reset_events();
	events
}

/// Who `System::remark_with_event` recorded as the sender, for every remark so far.
pub fn remark_senders() -> Vec<AccountId32> {
	System::events()
		.into_iter()
		.filter_map(|r| match r.event {
			RuntimeEvent::System(frame_system::Event::Remarked { sender, .. }) => Some(sender),
			_ => None,
		})
		.collect()
}
