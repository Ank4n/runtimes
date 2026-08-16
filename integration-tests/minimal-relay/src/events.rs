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

//! JSONL event sink for the migration monitor.
//!
//! Opt-in via `AHM_EVENTS=<path>`: every produced block appends one line per interesting
//! runtime event plus one `state` probe line, so a frontend can replay the migration by folding
//! the file. The schema is shared with the (future) live capture daemon:
//!
//! ```json
//! {"seq":0,"chain":"rc","block":123,"kind":"stage","payload":{"old":"Pending","new":"..."}}
//! {"seq":1,"chain":"rc","block":123,"kind":"state","payload":{"stage":"...","ti":"...planck"}}
//! ```
//!
//! Balances are emitted as decimal strings in planck to keep the JSON integer-safe.

use crate::mock::Chain;
use frame_support::traits::{PalletInfoData, PalletsInfoAccess};
use serde_json::{json, Value};
use std::{
	collections::HashMap,
	fs::File,
	io::Write,
	sync::{
		atomic::{AtomicU64, Ordering},
		Mutex, OnceLock,
	},
};

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

fn sink() -> Option<&'static Mutex<File>> {
	SINK.get_or_init(|| {
		let path = std::env::var("AHM_EVENTS").ok()?;
		let file = File::create(&path)
			.unwrap_or_else(|e| panic!("cannot create AHM_EVENTS file {path}: {e}"));
		println!("Writing migration events to {path}");
		Some(Mutex::new(file))
	})
	.as_ref()
}

/// Append one event line. No-op unless `AHM_EVENTS` is set.
pub fn emit(chain: &str, block: u32, kind: &str, payload: Value) {
	let Some(file) = sink() else { return };
	let line = json!({
		"seq": SEQ.fetch_add(1, Ordering::Relaxed),
		"chain": chain,
		"block": block,
		"kind": kind,
		"payload": payload,
	});
	let mut file = file.lock().unwrap();
	writeln!(file, "{line}").expect("can write event line");
}

fn planck(v: u128) -> Value {
	Value::String(v.to_string())
}

/// Debug-formatted stage, e.g. `AccountsOngoing { last_key: Some(...) }`. The frontend takes the
/// variant name from the prefix and the cursor detail is kept for humans.
fn stage_str<S: core::fmt::Debug>(stage: &S) -> String {
	format!("{stage:?}")
}

/// Emit a per-pallet storage census of the relay chain: one `storage_census` line per pallet
/// with its key count and total value bytes. Called before and after the migration so the
/// frontend can diff what each pallet held. Must run inside the RC `TestExternalities`.
pub fn emit_rc_census(phase: &str) {
	if sink().is_none() {
		return;
	}
	let block = frame_system::Pallet::<polkadot_runtime::Runtime>::block_number();
	census("rc", phase, block, polkadot_runtime::AllPalletsWithSystem::infos());
}

/// Like [`emit_rc_census`] for the Coretime chain.
pub fn emit_ct_census(phase: &str) {
	if sink().is_none() {
		return;
	}
	let block = frame_system::Pallet::<coretime_polkadot_runtime::Runtime>::block_number();
	census("ct", phase, block, coretime_polkadot_runtime::AllPalletsWithSystem::infos());
}

/// One pass over the whole top-level trie, bucketed by the 16-byte `twox128(pallet_name)`
/// prefix. Pallet names come from the runtime itself (`AllPalletsWithSystem`), so no list is
/// maintained by hand. Keys under no known pallet prefix (`:code`, retired pallets, …) are
/// aggregated as `(unattributed)`. Pallets with zero keys emit no line; the frontend treats
/// absence as zero.
fn census(chain: &str, phase: &str, block: u32, pallets: Vec<PalletInfoData>) {
	let mut by_prefix: HashMap<[u8; 16], (u64, u64)> = HashMap::new();
	let (mut other_keys, mut other_bytes) = (0u64, 0u64);

	let mut key = Vec::new();
	while let Some(next) = sp_io::storage::next_key(&key) {
		// Reading into an empty buffer returns the value's total length without copying it.
		let len = sp_io::storage::read(&next, &mut [], 0).unwrap_or(0) as u64;
		if next.len() >= 16 {
			let entry = by_prefix.entry(next[..16].try_into().expect("len checked")).or_default();
			entry.0 += 1;
			entry.1 += len;
		} else {
			other_keys += 1;
			other_bytes += len;
		}
		key = next;
	}

	for pallet in pallets {
		let prefix = sp_io::hashing::twox_128(pallet.name.as_bytes());
		if let Some((keys, bytes)) = by_prefix.remove(&prefix) {
			emit(
				chain,
				block,
				"storage_census",
				json!({ "phase": phase, "pallet": pallet.name, "keys": keys, "bytes": bytes }),
			);
		}
	}
	for (_, (keys, bytes)) in by_prefix {
		other_keys += keys;
		other_bytes += bytes;
	}
	if other_keys > 0 {
		emit(
			chain,
			block,
			"storage_census",
			json!({
				"phase": phase, "pallet": "(unattributed)",
				"keys": other_keys, "bytes": other_bytes,
			}),
		);
	}
}

/// Emit the events and state probe of the relay-chain block that was just produced.
/// Must run inside the RC `TestExternalities`.
pub fn emit_rc_block() {
	if sink().is_none() {
		return;
	}
	type Rc = polkadot_runtime::Runtime;
	use pallet_rc2_migrator::Event as MigEvent;

	let block = frame_system::Pallet::<Rc>::block_number();
	for record in frame_system::Pallet::<Rc>::events() {
		match record.event {
			polkadot_runtime::RuntimeEvent::Rc2Migrator(e) => match e {
				MigEvent::StageTransition { old, new } => emit(
					"rc",
					block,
					"stage",
					json!({ "old": stage_str(&old), "new": stage_str(&new) }),
				),
				MigEvent::AccountsBatchSent { count } =>
					emit("rc", block, "accounts_batch_sent", json!({ "count": count })),
				MigEvent::DepositRefunded { who, amount } => emit(
					"rc",
					block,
					"deposit_refunded",
					json!({ "who": format!("{who:?}"), "amount": planck(amount) }),
				),
				MigEvent::ProxyBatchSent { count } =>
					emit("rc", block, "proxy_batch_sent", json!({ "count": count })),
				MigEvent::HrmpRequestDropped { sender, recipient, deposit } => emit(
					"rc",
					block,
					"hrmp_request_dropped",
					json!({
						"sender": sender,
						"recipient": recipient,
						"deposit": planck(deposit),
					}),
				),
				MigEvent::AccountsTeleported { count, amount } => emit(
					"rc",
					block,
					"accounts_teleported",
					json!({ "count": count, "amount": planck(amount) }),
				),
				MigEvent::AccountHeldBack { who, free, reserved } => emit(
					"rc",
					block,
					"account_held_back",
					json!({
						"who": format!("{who:?}"),
						"free": planck(free),
						"reserved": planck(reserved),
					}),
				),
				MigEvent::TiCorrected { expected, unaccounted, burned } => emit(
					"rc",
					block,
					"ti_corrected",
					json!({
						"expected": planck(expected),
						"unaccounted": planck(unaccounted),
						"burned": planck(burned),
					}),
				),
				MigEvent::TiCorrectionAnomaly { expected, unaccounted } => emit(
					"rc",
					block,
					"ti_correction_anomaly",
					json!({ "expected": planck(expected), "unaccounted": planck(unaccounted) }),
				),
				MigEvent::RegistrarBatchSent { count } =>
					emit("rc", block, "registrar_batch_sent", json!({ "count": count })),
				MigEvent::HrmpBatchSent { count } =>
					emit("rc", block, "hrmp_batch_sent", json!({ "count": count })),
				_ => (),
			},
			polkadot_runtime::RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { success, .. },
			) if !success => emit("rc", block, "mq_failed", json!({})),
			_ => (),
		}
	}

	let tracker = pallet_rc2_migrator::RcMigratedBalance::<Rc>::get();
	emit(
		"rc",
		block,
		"state",
		json!({
			"stage": stage_str(&pallet_rc2_migrator::RcMigrationStage::<Rc>::get()),
			"ti": planck(pallet_balances::TotalIssuance::<Rc>::get()),
			"kept": planck(tracker.kept),
			"ct_reserved": planck(tracker.ct_reserved),
			"ct_free": planck(tracker.ct_free),
			"ah_free": planck(tracker.ah_free),
			"ti_corrected": planck(tracker.ti_corrected),
			"paras": polkadot_runtime_common::paras_registrar::Paras::<Rc>::iter().count(),
			"hrmp_channels": runtime_parachains::hrmp::HrmpChannels::<Rc>::iter().count(),
			"proxies": pallet_proxy::Proxies::<Rc>::iter().count(),
		}),
	);
}

/// Emit the events and state probe of the parachain block that was just produced.
/// Must run inside that chain's `TestExternalities`.
pub fn emit_para_block(chain: Chain) {
	if sink().is_none() {
		return;
	}
	match chain {
		Chain::Coretime => emit_ct_block(),
		// Only a light state probe for AH; it is not a destination of this migration's data.
		Chain::AssetHub => {
			type Ah = asset_hub_polkadot_runtime::Runtime;
			let block = frame_system::Pallet::<Ah>::block_number();
			// The checking account is AH's ledger of "DOT out on the relay chain"; teleports
			// from the RC drain it, so its delta is the AH-side receipt confirmation.
			let checking = pallet_xcm::Pallet::<Ah>::check_account();
			let checking_balance = frame_system::Account::<Ah>::get(&checking).data.free;
			emit(
				"ah",
				block,
				"state",
				json!({
					"ti": planck(pallet_balances::TotalIssuance::<Ah>::get()),
					"checking": planck(checking_balance),
				}),
			);
		},
		Chain::Relay => unreachable!("relay blocks go through emit_rc_block"),
	}
}

fn emit_ct_block() {
	type Ct = coretime_polkadot_runtime::Runtime;
	use pallet_ct_migrator::Event as MigEvent;

	let block = frame_system::Pallet::<Ct>::block_number();
	for record in frame_system::Pallet::<Ct>::events() {
		match record.event {
			coretime_polkadot_runtime::RuntimeEvent::CtMigrator(e) => match e {
				MigEvent::StageTransition { old, new } => emit(
					"ct",
					block,
					"stage",
					json!({ "old": stage_str(&old), "new": stage_str(&new) }),
				),
				MigEvent::AccountsReceived { count_good, count_bad } => emit(
					"ct",
					block,
					"accounts_received",
					json!({ "good": count_good, "bad": count_bad }),
				),
				MigEvent::RegistrarReceived { count_good, count_bad } => emit(
					"ct",
					block,
					"registrar_received",
					json!({ "good": count_good, "bad": count_bad }),
				),
				MigEvent::DepositShortfallParked { para_id, shortfall } => emit(
					"ct",
					block,
					"deposit_shortfall_parked",
					json!({ "para_id": para_id, "shortfall": planck(shortfall) }),
				),
				MigEvent::HrmpReceived { count } =>
					emit("ct", block, "hrmp_received", json!({ "count": count })),
				MigEvent::ProxiesReceived { count_good, count_bad } => emit(
					"ct",
					block,
					"proxies_received",
					json!({ "good": count_good, "bad": count_bad }),
				),
				MigEvent::HrmpShortfallParked { sender, recipient, shortfall } => emit(
					"ct",
					block,
					"hrmp_shortfall_parked",
					json!({
						"sender": sender,
						"recipient": recipient,
						"shortfall": planck(shortfall),
					}),
				),
				MigEvent::MigrationFinished { rc_kept, rc_migrated, ct_minted } => emit(
					"ct",
					block,
					"migration_finished",
					json!({
						"rc_kept": planck(rc_kept),
						"rc_migrated": planck(rc_migrated),
						"ct_minted": planck(ct_minted),
					}),
				),
			},
			coretime_polkadot_runtime::RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { success, .. },
			) if !success => emit("ct", block, "mq_failed", json!({})),
			_ => (),
		}
	}

	emit(
		"ct",
		block,
		"state",
		json!({
			"stage": stage_str(&pallet_ct_migrator::CtMigrationStage::<Ct>::get()),
			"ti": planck(pallet_balances::TotalIssuance::<Ct>::get()),
			"minted": planck(pallet_ct_migrator::CtMintedTotal::<Ct>::get()),
			"reattributed": planck(pallet_ct_migrator::ReattributedDeposits::<Ct>::get()),
			"reattributed_hrmp": planck(pallet_ct_migrator::ReattributedHrmpDeposits::<Ct>::get()),
			"paras": pallet_ct_migrator::RcParas::<Ct>::iter().count(),
			"hrmp_channels": pallet_ct_migrator::RcHrmpChannels::<Ct>::iter().count(),
			"failed_accounts": pallet_ct_migrator::FailedAccounts::<Ct>::iter().count(),
			"failed_paras": pallet_ct_migrator::FailedParas::<Ct>::iter().count(),
			"failed_hrmp": pallet_ct_migrator::FailedHrmpChannels::<Ct>::iter().count(),
			"failed_proxies": pallet_ct_migrator::FailedProxies::<Ct>::iter().count(),
			"parked_shortfalls": pallet_ct_migrator::ParkedDepositShortfalls::<Ct>::iter().count(),
			"parked_hrmp_shortfalls": pallet_ct_migrator::ParkedHrmpShortfalls::<Ct>::iter().count(),
		}),
	);
}
