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

//! Test runtime for `pallet-rc2-migrator`.

use crate as pallet_rc2_migrator;
use crate::{multisig::ManagerMultisigVote, ManagerMultisigRound, ManagerMultisigs};
use codec::Decode;
use frame_support::{
	derive_impl, ord_parameter_types, parameter_types,
	traits::{OnInitialize, Time},
};
use frame_system::EnsureSignedBy;
use sp_core::{sr25519, Pair};
use sp_runtime::{
	traits::{IdentifyAccount, IdentityLookup},
	AccountId32, BuildStorage, MultiSignature, MultiSigner,
};
use xcm::prelude::*;

type Block = frame_system::mocking::MockBlock<Test>;
pub type AccountId = AccountId32;

frame_support::construct_runtime! {
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Rc2Migrator: pallet_rc2_migrator,
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountId = AccountId;
	type Lookup = IdentityLookup<AccountId>;
	type AccountData = pallet_balances::AccountData<u128>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type AccountStore = System;
}

/// The account the mock treats as the Coretime chain's dispatch origin.
pub const CORETIME: AccountId = AccountId32::new([5; 32]);

/// Somebody
pub const ALICE: AccountId = AccountId32::new([1; 32]);

/// The account behind the admin origin.
pub const ADMIN: AccountId = AccountId32::new([2; 32]);

pub const CT_PARA_ID: u32 = 1005;
pub const WARM_UP: u64 = 4;
pub const COOL_OFF: u64 = 10;
/// Relay-chain block time
pub const BLOCK_TIME_MS: u64 = 6_000;

parameter_types! {
	pub const CtParaId: u32 = CT_PARA_ID;

	/// Every message the pallet successfully sent, in order.
	pub static SentXcm: Vec<(Location, Xcm<()>)> = vec![];
	/// Makes the router reject everything, to exercise the retry-next-block path.
	pub static SendFails: bool = false;

	/// The mock wall clock, in milliseconds. Advanced by [`run_blocks`].
	pub static MockNow: u64 = BLOCK_TIME_MS;
}

/// Stands in for `pallet_timestamp`.
pub struct MockTime;

impl Time for MockTime {
	type Moment = u64;

	fn now() -> Self::Moment {
		MockNow::get()
	}
}

/// Records what the pallet sends instead of delivering it.
pub struct RecordingRouter;

impl SendXcm for RecordingRouter {
	type Ticket = (Location, Xcm<()>);

	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<Self::Ticket> {
		if SendFails::get() {
			return Err(SendError::Transport("send forced to fail"));
		}
		let ticket =
			(dest.take().expect("destination is set"), msg.take().expect("message is set"));
		Ok((ticket, Assets::new()))
	}

	fn deliver(ticket: Self::Ticket) -> Result<XcmHash, SendError> {
		let mut sent = SentXcm::get();
		sent.push(ticket);
		SentXcm::set(sent);
		Ok([0; 32])
	}
}

ord_parameter_types! {
	pub const CoretimeAccount: AccountId = CORETIME;
	pub const AdminAccount: AccountId = ADMIN;
}

parameter_types! {
	/// Members of the manager multisig; set per test.
	pub static MultisigMembers: Vec<AccountId> = vec![];
	pub const MultisigThreshold: u32 = 2;
	pub const MultisigMaxVotesPerRound: u32 = 3;
	pub const MultisigStartRound: u32 = 7;
}

impl pallet_rc2_migrator::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type SendXcm = RecordingRouter;
	type CtParaId = CtParaId;
	type TimeProvider = MockTime;
	type CtOrigin = EnsureSignedBy<CoretimeAccount, AccountId>;
	type AdminOrigin = EnsureSignedBy<AdminAccount, AccountId>;
	type RuntimeCall = RuntimeCall;
	type MultisigMembers = MultisigMembers;
	type MultisigThreshold = MultisigThreshold;
	type MultisigMaxVotesPerRound = MultisigMaxVotesPerRound;
	type MultisigStartRound = MultisigStartRound;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	SentXcm::set(vec![]);
	SendFails::set(false);
	MockNow::set(BLOCK_TIME_MS);

	let storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(storage);
	// Block 0 does not record events.
	ext.execute_with(|| System::set_block_number(1));
	ext
}

/// Run the next `n` blocks, advancing the clock like a real chain does.
///
/// The clock moves *after* the block's hooks, mirroring the timestamp inherent, which is an
/// extrinsic and so runs after `on_initialize`.
pub fn run_blocks(n: u64) {
	for _ in 0..n {
		let now = System::block_number() + 1;
		System::set_block_number(now);
		<Rc2Migrator as OnInitialize<u64>>::on_initialize(now);
		MockNow::set(now.saturating_mul(BLOCK_TIME_MS));
	}
}

/// The mock clock's current value.
pub fn now_ms() -> u64 {
	MockTime::now()
}

/// The messages sent so far, destination and all.
pub fn sent() -> Vec<(Location, Xcm<()>)> {
	SentXcm::get()
}

/// Decode the `Transact` payload of the `n`th sent message as a Coretime runtime call.
pub fn sent_call(n: usize) -> crate::CtRuntimeCall {
	let (_, Xcm(instructions)) = sent().get(n).expect("message was sent").clone();
	for instruction in instructions {
		if let Instruction::Transact { call, .. } = instruction {
			return crate::CtRuntimeCall::decode(&mut &call.into_encoded()[..])
				.expect("payload decodes as a Coretime call");
		}
	}
	panic!("message {n} carried no Transact");
}

/// A multisig member: its signing key and its account id.
pub fn member(n: u8) -> (sr25519::Pair, AccountId) {
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

pub fn votes_for(call: &RuntimeCall) -> Vec<AccountId> {
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
pub fn remark_senders() -> Vec<AccountId> {
	System::events()
		.into_iter()
		.filter_map(|r| match r.event {
			RuntimeEvent::System(frame_system::Event::Remarked { sender, .. }) => Some(sender),
			_ => None,
		})
		.collect()
}
