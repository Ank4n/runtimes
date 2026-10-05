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

//! Tests for the AHM v2 migration.
//!
//! Tests use the multi-thread tokio runtime because [`load`] spawns snapshot hydration onto a
//! worker; on the default single-thread runtime, `tokio::join!`-ed loads would run one after the
//! other.

use crate::mock::*;
use codec::Encode;
use core::{cell::RefCell, mem::discriminant};
use cumulus_primitives_core::{ParaId, UpwardMessage};
use frame_support::{
	assert_ok, dispatch::PostDispatchInfo, hypothetically, traits::fungible::Mutate,
};
use network::constants::{currency::UNITS, system_parachain, time::MINUTES};
use pallet_message_queue::Event::{Processed, ProcessingFailed};
use pallet_rc2_migrator::MigrationStage as RcStage;
use sp_io::TestExternalities;
use sp_runtime::{
	traits::{AccountIdConversion, Dispatchable},
	AccountId32, DispatchError,
};
use std::collections::HashSet;
use xcm::{latest::prelude::*, VersionedXcm};

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

// One block-production test per chain, so a failure names the chain that broke.
// 10 blocks run the hooks against whatever the live snapshot carries; `next_block_*` asserts on
// every block that nothing fails processing and that the weight stays under 80% of the block
// limit.
#[tokio::test(flavor = "multi_thread")]
async fn relay_chain_produces_blocks() {
	load(Chain::Relay).await.execute_with(|| {
		for _ in 0..10 {
			next_block_rc();
		}
	});
}

#[tokio::test(flavor = "multi_thread")]
async fn coretime_produces_blocks() {
	load(Chain::Coretime).await.execute_with(|| {
		for _ in 0..10 {
			next_block_para::<CoretimePara>();
		}
	});
}

#[tokio::test(flavor = "multi_thread")]
async fn rc_and_coretime_exchange_messages() {
	message_round_trip::<CoretimePara>().await;
}

/// Assert that a `System::Remarked` event was emitted on runtime `T`.
fn assert_remarked<T: frame_system::Config>(chain: Chain)
where
	T::RuntimeEvent: TryInto<frame_system::Event<T>>,
{
	assert!(
		frame_system::Pallet::<T>::events().into_iter().any(|record| matches!(
			record.event.try_into(),
			Ok(frame_system::Event::<T>::Remarked { .. })
		)),
		"remark did not execute on {}",
		chain.name()
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
	let call: RuntimeCallFor<P> =
		frame_system::Call::<P::Runtime>::remark_with_event { remark: b"ahmv2 dmp".to_vec() }
			.into();
	let xcm = unpaid_transact(call);
	let dmp = rc.execute_with(|| {
		send_dmp(P::PARA_ID.into(), xcm.clone());
		next_block_rc();
		take_dmp(P::PARA_ID.into())
	});
	// The live snapshot may have queued unrelated messages for this para, so only assert that
	// ours is among them.
	let encoded = VersionedXcm::from(xcm).encode();
	assert!(
		dmp.iter().any(|message| message.msg == encoded),
		"RC did not queue the DMP message for {}",
		P::CHAIN.name()
	);

	para.execute_with(|| {
		enqueue_dmp::<P>(dmp);
		next_block_para::<P>();
		assert_remarked::<P::Runtime>(P::CHAIN);
	});

	// para -> RC.
	let call: network::relay::RuntimeCall =
		frame_system::Call::remark_with_event { remark: b"ahmv2 ump".to_vec() }.into();
	let xcm = unpaid_transact(call);
	let ump = para.execute_with(|| {
		send_ump::<P>(xcm.clone());
		take_ump::<P>()
	});
	assert!(
		ump.contains(&VersionedXcm::from(xcm).encode()),
		"{} did not queue the UMP message for the RC",
		P::CHAIN.name()
	);

	rc.execute_with(|| {
		enqueue_ump(P::PARA_ID.into(), ump);
		next_block_rc();
		assert_remarked::<network::relay::Runtime>(Chain::Relay);
	});
}

/// The windows this suite schedules with.
const WARM_UP: u32 = 10;
const COOL_OFF: u32 = 10;

/// A para the relay chain's barrier turns away outright, because it is not a system chain.
const OUTSIDER_PARA: u32 = 4242;

/// A system para that is not the Coretime chain. The barrier lets its message in, so the only
/// thing standing between it and the migration is `CtOrigin`.
const SYSTEM_IMPOSTOR_PARA: u32 = system_parachain::ASSET_HUB_ID;

/// Checks a test runs inside one chain at every stage the migration walk stops at. Each runs in a
/// storage layer that is rolled back, so a probe may change state without moving the walk.
struct Probes<'a> {
	rc: &'a dyn Fn(),
	ct: &'a dyn Fn(),
}

impl Probes<'static> {
	const NONE: Self = Probes { rc: &noop, ct: &noop };
}

impl Probes<'_> {
	fn rc(&self) {
		hypothetically!((self.rc)())
	}

	fn ct(&self) {
		hypothetically!((self.ct)())
	}
}

fn noop() {}

/// Schedule the migration and walk both chains through the handshake over their real queues: the
/// relay chain sends its start signal at the scheduled time, the Coretime chain opens and answers.
///
/// Returns that answer undelivered, so a test can decide who delivers it. `enqueue_dmp` decodes
/// the start signal with the real Coretime `RuntimeCall`, so a stale pallet or call index fails
/// here.
fn run_handshake(
	rc: &mut TestExternalities,
	ct: &mut TestExternalities,
	probes: &Probes,
) -> Vec<UpwardMessage> {
	// The relay chain is inert until governance schedules the migration, and stays inert until
	// the start block.
	let dmp = rc.execute_with(|| {
		assert_eq!(rc_stage(), RcStage::Pending);
		probes.rc();
		next_block_rc();
		assert_eq!(rc_stage(), RcStage::Pending, "an unscheduled migration must not start");

		// Two blocks' worth of time ahead: the block whose hooks first see a clock at or past
		// `start` is the third from here, since hooks run before the timestamp inherent.
		let start = now_ms_rc() + 2 * RC_BLOCK_TIME_MS;
		assert_ok!(pallet_rc2_migrator::Pallet::<network::relay::Runtime>::schedule_migration(
			network::relay::RuntimeOrigin::root(),
			start,
			WARM_UP,
			COOL_OFF,
		));

		next_block_rc();
		next_block_rc();
		assert_eq!(rc_stage(), RcStage::Scheduled { start }, "must not start before its time");
		probes.rc();

		next_block_rc();
		assert_eq!(rc_stage(), RcStage::WaitingForCt);
		probes.rc();
		take_dmp(CoretimePara::PARA_ID.into())
	});
	assert!(!dmp.is_empty(), "the relay chain queued no start signal for the Coretime chain");

	let ump = ct.execute_with(|| {
		assert_eq!(ct_stage(), pallet_ct_migrator::MigrationStage::Pending);
		probes.ct();
		enqueue_dmp::<CoretimePara>(dmp);
		next_block_para::<CoretimePara>();

		assert_eq!(ct_stage(), pallet_ct_migrator::MigrationStage::DataMigrationOngoing);
		probes.ct();
		take_ump::<CoretimePara>()
	});
	assert!(!ump.is_empty(), "the Coretime chain queued no answer for the relay chain");
	ump
}

/// Deliver the Coretime chain's answer and walk both chains from the warm-up to `MigrationDone`.
fn run_to_completion(
	rc: &mut TestExternalities,
	ct: &mut TestExternalities,
	ump: Vec<UpwardMessage>,
	probes: &Probes,
) {
	// The answer admits the machine to the warm-up, then through every data stage and the
	// verification window to the finish.
	let dmp = rc.execute_with(|| {
		enqueue_ump(CoretimePara::PARA_ID.into(), ump);
		next_block_rc();

		let RcStage::WarmUp { end_at } = rc_stage() else {
			panic!("readiness did not admit the machine to the warm-up: {:?}", rc_stage())
		};
		let now = frame_system::Pallet::<network::relay::Runtime>::block_number();
		assert_eq!(end_at, now + WARM_UP, "the warm-up must run for the scheduled window");
		probes.rc();

		set_block_number_rc(end_at - 1);
		next_block_rc();
		assert_eq!(rc_stage(), RcStage::AccountsInit, "the warm-up did not open the data stages");

		// The data stages walk one block each and carry nothing yet, so the only message sent so
		// far is the start signal already taken above.
		let mut blocks = 0;
		let end_at = loop {
			probes.rc();
			if let RcStage::CoolOff { end_at } = rc_stage() {
				break end_at;
			}
			assert!(
				// 15 data stages that is not doing anything currently, so 30 blocks is enough
				blocks < 30,
				"the data stages did not reach the cool-off: {:?}",
				rc_stage()
			);
			next_block_rc();
			blocks += 1;
		};
		assert!(
			take_dmp(CoretimePara::PARA_ID.into()).is_empty(),
			"a data stage sent something before being filled in"
		);
		set_block_number_rc(end_at - 1);
		next_block_rc();
		assert_eq!(rc_stage(), RcStage::MigrationDone);
		probes.rc();
		take_dmp(CoretimePara::PARA_ID.into())
	});
	assert!(!dmp.is_empty(), "the relay chain queued no finish signal");

	ct.execute_with(|| {
		enqueue_dmp::<CoretimePara>(dmp);
		next_block_para::<CoretimePara>();
		assert_eq!(ct_stage(), pallet_ct_migrator::MigrationStage::MigrationDone);
		probes.ct();
	});
}

/// The migration's stage machine, driven end to end over live relay-chain and Coretime state.
#[tokio::test(flavor = "multi_thread")]
async fn the_migration_runs_to_completion() {
	let (mut rc, mut ct) = tokio::join!(load(Chain::Relay), load(CoretimePara::CHAIN));

	let rc_issuance_before =
		rc.execute_with(pallet_balances::Pallet::<network::relay::Runtime>::total_issuance);
	let ct_issuance_before =
		ct.execute_with(pallet_balances::Pallet::<network::ct::Runtime>::total_issuance);

	let ump = run_handshake(&mut rc, &mut ct, &Probes::NONE);
	run_to_completion(&mut rc, &mut ct, ump, &Probes::NONE);

	ct.execute_with(|| {
		// Nothing was migrated, so nothing was minted here.
		assert_eq!(
			pallet_balances::Pallet::<network::ct::Runtime>::total_issuance(),
			ct_issuance_before,
			"a migration with no data stages must not change Coretime issuance"
		);
	});

	rc.execute_with(|| {
		assert_eq!(
			pallet_balances::Pallet::<network::relay::Runtime>::total_issuance(),
			rc_issuance_before,
			"a migration with no data stages must not change relay-chain issuance"
		);
	});
}

/// Readiness is only accepted from the Coretime chain.
#[tokio::test(flavor = "multi_thread")]
async fn readiness_from_another_parachain_is_refused() {
	let (mut rc, mut ct) = tokio::join!(load(Chain::Relay), load(CoretimePara::CHAIN));

	// GIVEN a relay chain waiting for the Coretime chain to be ready
	let ump = run_handshake(&mut rc, &mut ct, &Probes::NONE);

	// WHEN a para that is not a system chain sends that same message. THEN the barrier turns it
	// away before it executes, and the relay chain is still waiting.
	rc.execute_with(|| {
		drain_inbound_queues(OUTSIDER_PARA);
		drain_inbound_queues(SYSTEM_IMPOSTOR_PARA);
		enqueue_ump(OUTSIDER_PARA.into(), ump.clone());
		assert_eq!(
			ump_outcome(OUTSIDER_PARA),
			Some(false),
			"the barrier must refuse a message from a para that is not a system chain"
		);
		assert_eq!(rc_stage(), RcStage::WaitingForCt);
	});

	// WHEN a system para sends it. THEN the barrier admits the message and `Transact` runs, so
	// this is `CtOrigin` refusing the call rather than the barrier refusing the message -- and
	// the `ExpectTransactStatus` that follows the call turns that refusal into a failed message.
	rc.execute_with(|| {
		enqueue_ump(SYSTEM_IMPOSTOR_PARA.into(), ump);
		assert_eq!(
			ump_outcome(SYSTEM_IMPOSTOR_PARA),
			Some(false),
			"a refused call must fail the message, not report success"
		);
		assert_eq!(rc_stage(), RcStage::WaitingForCt);
	});
}

/// Run relay-chain blocks until `para`'s upward queue is empty, and say how many it took.
/// `None` if it was still not empty after `limit` blocks.
fn blocks_to_drain_ump(para: u32, limit: u32) -> Option<u32> {
	for blocks in 0..limit {
		if !has_queued_ump(para) {
			return Some(blocks);
		}
		next_block_rc_unchecked();
	}
	None
}

/// Empty `para`'s queue
fn drain_inbound_queues(para: u32) {
	blocks_to_drain_ump(para, 5 * MINUTES)
		.unwrap_or_else(|| panic!("para {para}'s queue did not drain"));
}

/// Whether `para`'s upward queue still holds an undelivered page.
fn has_queued_ump(para: u32) -> bool {
	let queue = UmpOrigin::Ump(UmpQueue::Para(para.into()));
	pallet_message_queue::Pages::<network::relay::Runtime>::iter_keys()
		.any(|(origin, _page)| origin == queue)
}

/// Run relay-chain blocks until the message queue reports on a message from `para`'s upward queue,
/// and say whether the executor accepted it. `None` if none was reported at all.
fn ump_outcome(para: u32) -> Option<bool> {
	let queue = UmpOrigin::Ump(UmpQueue::Para(para.into()));

	for _ in 0..10 {
		next_block_rc_unchecked();
		for record in frame_system::Pallet::<network::relay::Runtime>::events() {
			match record.event {
				network::relay::RuntimeEvent::MessageQueue(Processed {
					origin, success, ..
				}) if origin == queue => return Some(success),
				network::relay::RuntimeEvent::MessageQueue(ProcessingFailed { origin, .. })
					if origin == queue =>
					return Some(false),
				_ => (),
			}
		}
	}
	None
}

/// Accounts the lockdown probes use. None of them exists on either snapshot.
const ALICE: AccountId32 = AccountId32::new([0xa1; 32]); // signs every call
const BOB: AccountId32 = AccountId32::new([0xb0; 32]); // receives; on Coretime, the account behind
													   // a proxy
const CAROL: AccountId32 = AccountId32::new([0xca; 32]); // receives through the proxy

/// A para that is not a system chain.
const ORDINARY_PARA: u32 = 2000;

/// The lockdown at every stage of a real run, through both chains' real call filters and XCM
/// configuration.
///
/// From the relay chain's start signal on: a signed transfer is filtered, the manager still drives
/// the migrator, only the Coretime chain and Asset Hub get past the barrier, and a teleport from
/// Asset Hub mints nothing. None of that reopens at the end. On the Coretime chain, adding a proxy
/// is refused exactly while its migration runs, and an existing proxy works throughout.
#[tokio::test(flavor = "multi_thread")]
async fn the_lockdown_holds_at_every_stage() {
	let (mut rc, mut ct) = tokio::join!(load(Chain::Relay), load(CoretimePara::CHAIN));

	// GIVEN no backlog from the snapshot, which would be processed alongside the probes' messages
	// and move the issuance they measure.
	rc.execute_with(drain_all_inbound_queues);

	// Each chain's stages the probes ran at, by variant, so the end of the test can check that none
	// was skipped.
	let rc_stages = RefCell::new(HashSet::new());
	let ct_stages = RefCell::new(HashSet::new());
	let probes = Probes {
		rc: &|| {
			rc_stages.borrow_mut().insert(discriminant(&rc_stage()));
			probe_rc_lockdown();
		},
		ct: &|| {
			ct_stages.borrow_mut().insert(discriminant(&ct_stage()));
			probe_ct_lockdown();
		},
	};

	// WHEN the migration runs from `Pending` to `MigrationDone`. THEN every probe holds at every
	// stage it stops at.
	let ump = run_handshake(&mut rc, &mut ct, &probes);
	run_to_completion(&mut rc, &mut ct, ump, &probes);

	// Every variant of the relay chain's `MigrationStage` was probed, and all three of Coretime's.
	assert_eq!(rc_stages.borrow().len(), 21);
	assert_eq!(ct_stages.borrow().len(), 3);
}

/// The relay chain's lockdown at its current stage: open before the start, closed from the start
/// on, including after `MigrationDone`.
fn probe_rc_lockdown() {
	type Rc = network::relay::Runtime;
	let stage = rc_stage();
	let started = stage.has_started();
	let filtered: DispatchError = frame_system::Error::<Rc>::CallFiltered.into();

	// A signed transfer.
	hypothetically!({
		assert_ok!(pallet_balances::Pallet::<Rc>::mint_into(&ALICE, 100 * UNITS));
		let transfer =
			network::relay::RuntimeCall::Balances(pallet_balances::Call::transfer_keep_alive {
				dest: BOB.into(),
				value: 10 * UNITS,
			});
		assert_eq!(
			dispatch_signed(&ALICE, transfer),
			if started { Err(filtered) } else { Ok(()) },
			"a signed transfer at {stage:?}"
		);
		assert_eq!(
			pallet_balances::Pallet::<Rc>::free_balance(&BOB),
			if started { 0 } else { 10 * UNITS },
			"the transfer's recipient at {stage:?}"
		);
	});

	// The manager drives the migration with signed calls, which the filter lets through.
	hypothetically!({
		assert_ok!(pallet_rc2_migrator::Pallet::<Rc>::set_manager(
			network::relay::RuntimeOrigin::root(),
			Some(ALICE),
		));
		let pause =
			network::relay::RuntimeCall::Rc2Migrator(pallet_rc2_migrator::Call::pause_migration {});
		assert_eq!(
			dispatch_signed(&ALICE, pause),
			if stage.is_ongoing() {
				Ok(())
			} else {
				Err(pallet_rc2_migrator::Error::<Rc>::NotRunning.into())
			},
			"the manager pausing at {stage:?}"
		);
	});

	// Unpaid messages from system chains: from the start, only Coretime and Asset Hub get in.
	for (para, admitted_once_started) in [
		(system_parachain::BROKER_ID, true),
		(system_parachain::ASSET_HUB_ID, true),
		(system_parachain::BRIDGE_HUB_ID, false),
	] {
		assert_eq!(
			hypothetically!(deliver_ump(para, unpaid_noop())),
			if started && !admitted_once_started { Delivery::Refused } else { Delivery::Executed },
			"an unpaid message from para {para} at {stage:?}"
		);
	}

	// An ordinary para paying from its sovereign account here.
	hypothetically!({
		let sovereign: AccountId32 = ParaId::from(ORDINARY_PARA).into_account_truncating();
		assert_ok!(pallet_balances::Pallet::<Rc>::mint_into(&sovereign, 100 * UNITS));
		assert_eq!(
			deliver_ump(ORDINARY_PARA, paid_transfer_to(&BOB)),
			if started { Delivery::Refused } else { Delivery::Executed },
			"a paid message from para {ORDINARY_PARA} at {stage:?}"
		);
	});

	// A teleport from Asset Hub mints here until the start, and nothing from then on.
	hypothetically!({
		let issuance = pallet_balances::Pallet::<Rc>::total_issuance();
		let landed = if started { 0 } else { 10 * UNITS };
		assert_eq!(
			deliver_ump(system_parachain::ASSET_HUB_ID, teleport_to(&BOB, 10 * UNITS)),
			if started {
				Delivery::Failed(XcmError::UntrustedTeleportLocation)
			} else {
				Delivery::Executed
			},
			"a teleport from Asset Hub at {stage:?}"
		);
		assert_eq!(pallet_balances::Pallet::<Rc>::free_balance(&BOB), landed, "at {stage:?}");
		assert_eq!(
			pallet_balances::Pallet::<Rc>::total_issuance(),
			issuance + landed,
			"issuance after a teleport at {stage:?}"
		);
	});
}

/// The Coretime chain's lockdown at its current stage: proxy changes closed only while the
/// migration runs.
fn probe_ct_lockdown() {
	type Ct = network::ct::Runtime;
	let stage = ct_stage();
	let filtered: DispatchError = frame_system::Error::<Ct>::CallFiltered.into();

	// Adding a proxy.
	hypothetically!({
		assert_ok!(pallet_balances::Pallet::<Ct>::mint_into(&ALICE, 100 * UNITS));
		let add = network::ct::RuntimeCall::Proxy(pallet_proxy::Call::add_proxy {
			delegate: BOB.into(),
			proxy_type: network::ct::ProxyType::Any,
			delay: 0,
		});
		assert_eq!(
			dispatch_signed(&ALICE, add),
			if stage.is_ongoing() { Err(filtered) } else { Ok(()) },
			"adding a proxy at {stage:?}"
		);
	});

	// Using an existing proxy, here to make a transfer.
	hypothetically!({
		assert_ok!(pallet_balances::Pallet::<Ct>::mint_into(&BOB, 100 * UNITS));
		assert_ok!(pallet_proxy::Pallet::<Ct>::add_proxy_delegate(
			&BOB,
			ALICE,
			network::ct::ProxyType::Any,
			0,
		));
		let transfer =
			network::ct::RuntimeCall::Balances(pallet_balances::Call::transfer_keep_alive {
				dest: CAROL.into(),
				value: 10 * UNITS,
			});
		let proxy = network::ct::RuntimeCall::Proxy(pallet_proxy::Call::proxy {
			real: BOB.into(),
			force_proxy_type: None,
			call: Box::new(transfer),
		});
		assert_eq!(dispatch_signed(&ALICE, proxy), Ok(()), "using a proxy at {stage:?}");
		// `proxy` succeeds even when the inner call fails, so check what the inner call did.
		assert_eq!(
			pallet_balances::Pallet::<Ct>::free_balance(&CAROL),
			10 * UNITS,
			"the proxied transfer at {stage:?}"
		);
	});

	// TODO(ahm-v2): probe `RegistrarPara` and `HrmpPara` once they are in the runtime: refused
	// until `MigrationDone`, open from then on.
}

/// Dispatch `call` from `who`, through the runtime's call filter.
fn dispatch_signed<C>(who: &AccountId32, call: C) -> Result<(), DispatchError>
where
	C: Dispatchable<PostInfo = PostDispatchInfo>,
	C::RuntimeOrigin: From<frame_system::RawOrigin<AccountId32>>,
{
	call.dispatch(frame_system::RawOrigin::Signed(who.clone()).into())
		.map(|_| ())
		.map_err(|e| e.error)
}

/// Run relay-chain blocks until no inbound queue holds an unprocessed message.
fn drain_all_inbound_queues() {
	for _ in 0..5 * MINUTES {
		if pallet_message_queue::ServiceHead::<network::relay::Runtime>::get().is_none() {
			return;
		}
		next_block_rc_unchecked();
	}
	panic!("the relay chain's inbound queues did not drain");
}

/// A message that asks for free execution and does nothing else, so whether it runs is the
/// barrier's verdict alone.
fn unpaid_noop() -> Xcm<()> {
	Xcm(vec![UnpaidExecution { weight_limit: Unlimited, check_origin: None }, ClearOrigin])
}

/// A message that pays for itself from the sender's account here and deposits the rest with `to`.
fn paid_transfer_to(to: &AccountId32) -> Xcm<()> {
	Xcm(vec![
		// Take 10 units out of the sender's account here.
		WithdrawAsset((Here, 10 * UNITS).into()),
		// Pay for this message's execution out of it, at most 1 unit.
		BuyExecution { fees: (Here, UNITS).into(), weight_limit: Unlimited },
		// Put the rest in `to`'s account. Withdrawing 10 keeps the rest above the existential
		// deposit, so the deposit cannot fail for that reason.
		DepositAsset { assets: AllCounted(1).into(), beneficiary: account(to) },
	])
}

/// A teleport of `amount` to `to`, unpaid as system chains send it.
fn teleport_to(to: &AccountId32, amount: u128) -> Xcm<()> {
	Xcm(vec![
		UnpaidExecution { weight_limit: Unlimited, check_origin: None },
		ReceiveTeleportedAsset((Here, amount).into()),
		DepositAsset { assets: AllCounted(1).into(), beneficiary: account(to) },
	])
}

fn account(who: &AccountId32) -> Location {
	Junction::AccountId32 { network: None, id: who.clone().into() }.into()
}

fn rc_stage() -> pallet_rc2_migrator::MigrationStageOf<network::relay::Runtime> {
	pallet_rc2_migrator::RcMigrationStage::<network::relay::Runtime>::get()
}

fn ct_stage() -> pallet_ct_migrator::MigrationStage {
	pallet_ct_migrator::CtMigrationStage::<network::ct::Runtime>::get()
}
