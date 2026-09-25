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

use crate::{
	checks::{
		ah_checking_paid, AccountsChecker, AhMigrationCheck, CtMigrationCheck, RcMigrationCheck,
		SanityChecks,
	},
	mock::*,
};
use codec::Encode;
use cumulus_primitives_core::{AggregateMessageOrigin, InboundDownwardMessage, UpwardMessage};
use frame_support::{assert_ok, traits::QueueFootprintQuery};
use network::constants::{system_parachain, time::MINUTES};
use pallet_message_queue::Event::{Processed, ProcessingFailed};
use pallet_rc2_migrator::MigrationStage as RcStage;
use sp_io::TestExternalities;
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

/// Schedule the migration and walk both chains through the handshake over their real queues: the
/// relay chain sends its start signal at the scheduled time, the Coretime chain opens and answers.
///
/// Returns that answer undelivered, so a test can decide who delivers it. `enqueue_dmp` decodes
/// the start signal with the real Coretime `RuntimeCall`, so a stale pallet or call index fails
/// here.
fn run_handshake(rc: &mut TestExternalities, ct: &mut TestExternalities) -> Vec<UpwardMessage> {
	// The relay chain is inert until governance schedules the migration, and stays inert until
	// the start block.
	let dmp = rc.execute_with(|| {
		assert_eq!(rc_stage(), RcStage::Pending);
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

		next_block_rc();
		assert_eq!(rc_stage(), RcStage::WaitingForCt);
		take_dmp(CoretimePara::PARA_ID.into())
	});
	assert!(!dmp.is_empty(), "the relay chain queued no start signal for the Coretime chain");

	let ump = ct.execute_with(|| {
		assert_eq!(ct_stage(), pallet_ct_migrator::MigrationStage::Pending);
		enqueue_dmp::<CoretimePara>(dmp);
		next_block_para::<CoretimePara>();

		assert_eq!(ct_stage(), pallet_ct_migrator::MigrationStage::DataMigrationOngoing);
		take_ump::<CoretimePara>()
	});
	assert!(!ump.is_empty(), "the Coretime chain queued no answer for the relay chain");
	ump
}

type RcChecks = (SanityChecks, AccountsChecker);
type CtChecks = (SanityChecks, AccountsChecker);
type AhChecks = (SanityChecks, AccountsChecker);

/// Deliver `dmp` to parachain `P` and run one of its blocks.
fn deliver<P: Para>(ext: &mut TestExternalities, dmp: Vec<InboundDownwardMessage>) {
	ext.execute_with(|| {
		enqueue_dmp::<P>(dmp);
		next_block_para::<P>();
	});
}

/// Run blocks on parachain `P` until its downward queue has nothing left to process. Pages that
/// only hold overweight messages stay, but are not processed again.
fn drain_dmp<P: Para>(ext: &mut TestExternalities) {
	ext.execute_with(|| {
		for _ in 0..100 {
			let queue = pallet_message_queue::Pallet::<P::Runtime>::footprint(
				AggregateMessageOrigin::Parent,
			);
			if queue.ready_pages == 0 {
				return;
			}
			next_block_para::<P>();
		}
		panic!("{}'s downward queue did not drain", P::CHAIN.name());
	});
}

/// The migration, driven end to end over live Relay Chain, Coretime and Asset Hub state, with the
/// checks of every stage run before and after.
#[tokio::test(flavor = "multi_thread")]
async fn the_migration_runs_to_completion() {
	let (mut rc, mut ct, mut ah) =
		tokio::join!(load(Chain::Relay), load(CoretimePara::CHAIN), load(AssetHubPara::CHAIN));

	let rc_pre = rc.execute_with(<RcChecks as RcMigrationCheck>::pre_check);
	let ct_pre = ct.execute_with(|| <CtChecks as CtMigrationCheck>::pre_check(rc_pre.clone()));
	let ah_pre = ah.execute_with(|| <AhChecks as AhMigrationCheck>::pre_check(rc_pre.clone()));

	let ump = run_handshake(&mut rc, &mut ct);

	// The answer admits the machine to the warm-up, and the warm-up to the data stages.
	let max_blocks = rc.execute_with(|| {
		enqueue_ump(CoretimePara::PARA_ID.into(), ump);
		next_block_rc();

		let RcStage::WarmUp { end_at } = rc_stage() else {
			panic!("readiness did not admit the machine to the warm-up: {:?}", rc_stage())
		};
		let now = frame_system::Pallet::<network::relay::Runtime>::block_number();
		assert_eq!(end_at, now + WARM_UP, "the warm-up must run for the scheduled window");

		set_block_number_rc(end_at - 1);
		next_block_rc();
		assert_eq!(rc_stage(), RcStage::AccountsInit, "the warm-up did not open the data stages");

		// One block per data stage, plus one per batch of accounts.
		let accounts = frame_system::Account::<network::relay::Runtime>::iter().count() as u32;
		accounts / pallet_rc2_migrator::accounts::MAX_ACCOUNTS_PER_BLOCK + 30
	});

	// The data stages run until the verification window opens, each block's messages delivered as
	// they are sent.
	let mut blocks = 0;
	let cool_off_end = loop {
		let (ct_dmp, ah_dmp, stage) = rc.execute_with(|| {
			next_block_rc();
			(
				take_dmp(CoretimePara::PARA_ID.into()),
				take_dmp(AssetHubPara::PARA_ID.into()),
				rc_stage(),
			)
		});
		deliver::<CoretimePara>(&mut ct, ct_dmp);
		deliver::<AssetHubPara>(&mut ah, ah_dmp);
		if let RcStage::CoolOff { end_at } = stage {
			break end_at;
		}
		blocks += 1;
		assert!(blocks < max_blocks, "the data stages did not reach the cool-off: {stage:?}");
	};
	drain_dmp::<CoretimePara>(&mut ct);
	drain_dmp::<AssetHubPara>(&mut ah);

	// The verification window elapses and the finish signal reaches the Coretime chain.
	let dmp = rc.execute_with(|| {
		set_block_number_rc(cool_off_end - 1);
		next_block_rc();
		assert_eq!(rc_stage(), RcStage::MigrationDone);
		take_dmp(CoretimePara::PARA_ID.into())
	});
	assert!(!dmp.is_empty(), "the relay chain queued no finish signal");
	deliver::<CoretimePara>(&mut ct, dmp);

	rc.execute_with(|| <RcChecks as RcMigrationCheck>::post_check(rc_pre.clone()));
	ct.execute_with(|| <CtChecks as CtMigrationCheck>::post_check(rc_pre.clone(), ct_pre));
	ah.execute_with(|| <AhChecks as AhMigrationCheck>::post_check(rc_pre, ah_pre.clone()));

	// Every unit the Relay Chain burned arrived on the chain its ledger says it went to.
	let ledger =
		rc.execute_with(pallet_rc2_migrator::RcMigratedBalance::<network::relay::Runtime>::get);
	ct.execute_with(|| {
		assert_eq!(
			pallet_ct_migrator::CtMintedTotal::<network::ct::Runtime>::get(),
			ledger.ct_reserved + ledger.ct_free,
			"Coretime minted a different amount than the Relay Chain sent"
		);
	});
	ah.execute_with(|| {
		assert_eq!(
			ah_checking_paid(&ah_pre.1),
			ledger.ah_free,
			"Asset Hub received a different amount than the Relay Chain teleported"
		);
	});
}

/// Readiness is only accepted from the Coretime chain.
#[tokio::test(flavor = "multi_thread")]
async fn readiness_from_another_parachain_is_refused() {
	let (mut rc, mut ct) = tokio::join!(load(Chain::Relay), load(CoretimePara::CHAIN));

	// GIVEN a relay chain waiting for the Coretime chain to be ready
	let ump = run_handshake(&mut rc, &mut ct);

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

fn rc_stage() -> pallet_rc2_migrator::MigrationStageOf<network::relay::Runtime> {
	pallet_rc2_migrator::RcMigrationStage::<network::relay::Runtime>::get()
}

fn ct_stage() -> pallet_ct_migrator::MigrationStage {
	pallet_ct_migrator::CtMigrationStage::<network::ct::Runtime>::get()
}
