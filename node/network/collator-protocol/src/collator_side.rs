// Copyright 2020 Parity Technologies (UK) Ltd.
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
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::HashMap;

use futures::channel::oneshot;
use log::{trace, warn};
use polkadot_primitives::v1::{
	CollatorId, CoreIndex, CoreState, Hash, Id as ParaId, CandidateReceipt,
	PoV, ValidatorId,
};
use super::{TARGET,  Result};
use polkadot_subsystem::{
	FromOverseer, OverseerSignal, SubsystemContext,
	messages::{
		AllMessages, CollatorProtocolMessage, RuntimeApiMessage, RuntimeApiRequest,
		NetworkBridgeMessage,
	},
	metrics::{self, prometheus},
};
use polkadot_node_network_protocol::{
	v1 as protocol_v1, View, PeerId, PeerSet, NetworkBridgeEvent, RequestId,
};
use polkadot_node_subsystem_util::{
	request_validators_ctx,
	request_validator_groups_ctx,
};

#[derive(Clone, Default)]
pub(super) struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_advertisment_made(&self) {
		if let Some(metrics) = &self.0 {
			metrics.advertisments_made.inc();
		}
	}

	fn on_collation_sent(&self) {
		if let Some(metrics) = &self.0 {
			metrics.collations_sent.inc();
		}
	}
}

#[derive(Clone)]
struct MetricsInner {
	advertisments_made: prometheus::Counter<prometheus::U64>,
	collations_sent: prometheus::Counter<prometheus::U64>,
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry)
		-> std::result::Result<Self, prometheus::PrometheusError>
	{
		let metrics = MetricsInner {
			advertisments_made: prometheus::register(
				prometheus::Counter::new(
					"parachain_advertisments_made_total",
					"A number of advertisments sent to validators.",
				)?,
				registry,
			)?,
			collations_sent: prometheus::register(
				prometheus::Counter::new(
					"parachain_collations_sent_total",
					"A number of collations sent to validators.",
				)?,
				registry,
			)?,
		};

		Ok(Metrics(Some(metrics)))
	}
}

#[derive(Default)]
struct State {
	/// Our id.
	our_id: CollatorId,

	/// The para this collator is collating on.
	/// Starts as `None` and is updated with every `CollateOn` message.
	collating_on: Option<ParaId>,

	/// Track all active peers and their views
	/// to determine what is relevant to them.
	peer_views: HashMap<PeerId, View>,

	/// Our own view.
	view: View,

	/// Possessed collations.
	///
	/// We will keep up to one local collation per relay-parent.
	collations: HashMap<Hash, (CandidateReceipt, PoV)>,

	/// Our validator groups active leafs. 
	our_validators_groups: HashMap<Hash, Vec<ValidatorId>>,

	/// Validators we know about via `ConnectToValidators` message.
	/// 
	/// These are the only validators we are interested in talking to and as such
	/// all actions from peers not in this map will be ignored.
	/// Entries in this map will be cleared as validator groups in `our_validator_groups`
	/// go out of scope with their respective deactivated leafs.
	known_validators: HashMap<PeerId, ValidatorId>,

	/// Metrics.
	metrics: Metrics,
}

/// Distribute a collation.
/// 
/// Figure out the core our para is assigned to and the relevant validators.
/// Issue a connection request to these validators.
/// If the para is not scheduled or next up on any core, at the relay-parent,
/// or the relay-parent isn't in the active-leaves set, we ignore the message
/// as it must be invalid in that case - although this indicates a logic error
/// elsewhere in the node.
async fn distribute_collation<Context>(
	ctx: &mut Context,
	state: &mut State,
	id: ParaId,
	receipt: CandidateReceipt,
	pov: PoV,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	let relay_parent = receipt.descriptor.relay_parent;

	// This collation is not in the active-leaves set.
	if !state.view.contains(&relay_parent) {
		warn!(
			target: TARGET,
			"Distribute collation message parent {:?} is outside of our view",
			relay_parent,
		);

		return Ok(());
	}

	// We have already seen collation for this relay parent.
	if state.collations.contains_key(&relay_parent) {
		return Ok(());
	}

	// Determine which core the para collated-on is assigned to.
	// If it is not scheduled then ignore the message.
	let (our_core, num_cores) = match determine_core(ctx, id, relay_parent).await? {
	    Some(core) => core,
	    None => {
			warn!(
				target: TARGET,
				"Looks like no core is assigned to {:?} at {:?}", id, relay_parent,
			);
			return Ok(());
		}
	};

	// Determine the group on that core and the next group on that core.
	let our_validators = match determine_our_validators(ctx, our_core, num_cores, relay_parent).await? {
	    Some(validators) => validators,
	    None => {
			warn!(
				target: TARGET,
				"There are no validators assigned to {:?} core", our_core,
			);

			return Ok(());
		}
	};

	state.our_validators_groups.insert(relay_parent, our_validators.clone());

	// Issue a discovery request for the validators of the current group and the next group.
	connect_to_validators(ctx, state, our_validators).await?;

	state.collations.insert(relay_parent, (receipt, pov));

	Ok(())
}

/// Get the Id of the Core that is assigned to the para being collated on if any
/// and the total number of cores.
async fn determine_core<Context>(
	ctx: &mut Context,
	para_id: ParaId,
	relay_parent: Hash,
) -> Result<Option<(CoreIndex, usize)>>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	let (tx, rx) = oneshot::channel();

	ctx.send_message(AllMessages::RuntimeApi(
		RuntimeApiMessage::Request(
			relay_parent,
			RuntimeApiRequest::AvailabilityCores(tx),
		)
	)).await?;

	let cores = rx.await??;

	for (idx, core) in cores.iter().enumerate() {
		if let CoreState::Scheduled(occupied) = core {
			if occupied.para_id == para_id {
				return Ok(Some(((idx as u32).into(), cores.len())));
			}
		}
	}

	Ok(None)
}

/// Figure out a group of validators assigned to the para being collated on.
/// 
/// This returns validators for the current group and the next group.
async fn determine_our_validators<Context>(
	ctx: &mut Context,
	core_index: CoreIndex,
	cores: usize,
	relay_parent: Hash,
) -> Result<Option<Vec<ValidatorId>>>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	let groups = request_validator_groups_ctx(relay_parent, ctx).await?;

	let groups = groups.await??;

	let current_group_index = groups.1.group_for_core(core_index, cores);

	let mut connect_to_validators = match groups.0.get(current_group_index.0 as usize) {
		Some(group) => group.clone(),
		None => return Ok(None),
	};

	let next_group_idx = (current_group_index.0 as usize + 1) % groups.0.len();

	if let Some(next_group) = groups.0.get(next_group_idx) {
		connect_to_validators.extend_from_slice(&next_group);
	}

	let validators = request_validators_ctx(relay_parent, ctx).await?;

	let validators = validators.await??;

	let validators = connect_to_validators 
		.into_iter()
		.map(|idx| validators[idx as usize].clone())
		.collect();

	Ok(Some(validators))
}

/// Issue a `Declare` collation message to a set of peers.
async fn declare<Context>(
	ctx: &mut Context,
	state: &mut State,
	to: Vec<PeerId>,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	let wire_message = protocol_v1::CollatorProtocolMessage::Declare(state.our_id.clone());

	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::SendCollationMessage(
			to,
			protocol_v1::CollationProtocol::CollatorProtocol(wire_message),
		)
	)).await?;

	Ok(())
}

/// Issue a connection request to a set of validators.
async fn connect_to_validators<Context>(
	ctx: &mut Context,
	state: &mut State,
	validators: Vec<ValidatorId>,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	let (tx, rx) = oneshot::channel();

	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::ConnectToValidators(PeerSet::Collation, validators, tx),
	)).await?;

	let mut validators_ids = rx.await?;

	for id in validators_ids.drain(..) {
		state.known_validators.insert(id.1, id.0);
	}

	Ok(())
}

/// Advertise collation to a set of relay chain validators.
async fn advertise_collation<Context>(
	ctx: &mut Context,
	state: &mut State,
	relay_parent: Hash,
	to: Vec<PeerId>,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	let collating_on = match state.collating_on {
	    Some(collating_on) => collating_on,
	    None => {
			return Ok(());
		}
	};

	let wire_message = protocol_v1::CollatorProtocolMessage::AdvertiseCollation(relay_parent, collating_on);

	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::SendCollationMessage(
			to,
			protocol_v1::CollationProtocol::CollatorProtocol(wire_message),
		)
	)).await?;

	state.metrics.on_advertisment_made();

	Ok(())
}

/// The main incoming message dispatching switch.
async fn process_msg<Context>(
	ctx: &mut Context,
	state: &mut State,
	msg: CollatorProtocolMessage,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	use CollatorProtocolMessage::*;

	match msg {
		CollateOn(id) => {
			state.collating_on = Some(id);
		}
		DistributeCollation(receipt, pov) => {
			match state.collating_on {
				Some(id) if receipt.descriptor.para_id != id => {
					// If the ParaId of a collation requested to be distributed does not match
					// the one we expect, we ignore the message.
					warn!(
						target: TARGET,
						"DistributeCollation message for para {:?} while collating on {:?}",
						receipt.descriptor.para_id,
						id,
					);
				}
				Some(id) => {
					distribute_collation(ctx, state, id, receipt, pov).await?;
				}
				None => {
					warn!(
						target: TARGET,
						"DistributeCollation message for para {:?} while not collating on any",
						receipt.descriptor.para_id,
					);
				}
			}
		}
		FetchCollation(_, _, _, _) => {
			warn!(
				target: TARGET,
				"FetchCollation message is not expected on the collator side of the protocol",
			);
		}
		ReportCollator(_) => {
			warn!(
				target: TARGET,
				"ReportCollator message is not expected on the collator side of the protocol",
			);
		}
		NoteGoodCollation(_) => {
			warn!(
				target: TARGET,
				"NoteGoodCollation message is not expected on the collator side of the protocol",
			);
		}
		NetworkBridgeUpdateV1(event) => {
			if let Err(e) = handle_network_msg(
				ctx,
				state,
				event,
			).await {
				warn!(
					target: TARGET,
					"Failed to handle incoming network message: {:?}", e,
				);
			}
		},
	}

	Ok(())
}

/// Issue a response to a previously requested collation.
async fn send_collation<Context>(
	ctx: &mut Context,
	state: &mut State,
	request_id: RequestId,
	origin: PeerId,
	receipt: CandidateReceipt,
	pov: PoV,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	let wire_message = protocol_v1::CollatorProtocolMessage::Collation(
		request_id,
		receipt,
		pov,
	);

	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::SendCollationMessage(
			vec![origin],
			protocol_v1::CollationProtocol::CollatorProtocol(wire_message),
		)
	)).await?;

	state.metrics.on_collation_sent();

	Ok(())
}

/// A networking messages switch.
async fn handle_incoming_peer_message<Context>(
	ctx: &mut Context,
	state: &mut State,
	origin: PeerId,
	msg: protocol_v1::CollatorProtocolMessage,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	use protocol_v1::CollatorProtocolMessage::*;

	match msg {
	    Declare(_) => {
			warn!(
				target: TARGET,
				"Declare message is not expected on the collator side of the protocol",
			);
		}
	    AdvertiseCollation(_, _) => {
			warn!(
				target: TARGET,
				"AdvertiseCollation message is not expected on the collator side of the protocol",
			);
		}
	    RequestCollation(request_id, relay_parent, para_id) => {
			match state.collating_on {
				Some(our_para_id) => {
					if our_para_id == para_id {
						if let Some(collation) = state.collations.get(&relay_parent).cloned() {
							send_collation(ctx, state, request_id, origin, collation.0, collation.1).await?;
						}
					} else {
						warn!(
							target: TARGET,
							"Received a RequestCollation for {:?} while collating on {:?}",
							para_id, our_para_id,
						);
					}
				}
				None => {
					warn!(
						target: TARGET,
						"Received a RequestCollation for {:?} while not collating on any para",
						para_id,
					);
				}
			}
		}
	    Collation(_, _, _) => {
			warn!(
				target: TARGET,
				"Collation message is not expected on the collator side of the protocol",
			);
		}
	}

	Ok(())
}

/// Our view has changed.
async fn handle_peer_view_change<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer_id: PeerId,
	view: View,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	let current = state.peer_views.entry(peer_id.clone()).or_default();

	let added: Vec<Hash> = view.difference(&*current).cloned().collect();

	*current = view;

	for added in added.into_iter() {
		if state.collations.contains_key(&added) {
			advertise_collation(ctx, state, added.clone(), vec![peer_id.clone()]).await?;
		}
	}

	Ok(())
}

/// A peer is connected.
///
/// We first want to check if this is a validator we are expecting to talk to
/// and if so `Declare` that we are a collator with a given `CollatorId`.
async fn handle_peer_connected<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer_id: PeerId,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	if !state.known_validators.contains_key(&peer_id) {
		trace!(target: TARGET, "An unknown peer has connected {:?}", peer_id);

		return Ok(())
	}

	state.peer_views.entry(peer_id.clone()).or_default();

	declare(ctx, state, vec![peer_id]).await?;

	Ok(())
}

/// Bridge messages switch.
async fn handle_network_msg<Context>(
	ctx: &mut Context,
	state: &mut State,
	bridge_message: NetworkBridgeEvent<protocol_v1::CollatorProtocolMessage>,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	use NetworkBridgeEvent::*;

	match bridge_message {
	    PeerConnected(peer_id, _observed_role) => {
			handle_peer_connected(ctx, state, peer_id).await?;
		}
		PeerViewChange(peer_id, view) => {
			handle_peer_view_change(ctx, state, peer_id, view).await?;
		}
		PeerDisconnected(peer_id) => {
			state.peer_views.remove(&peer_id);
		}
		OurViewChange(view) => {
			handle_our_view_change(state, view).await?;
		}
	    PeerMessage(remote, msg) => {
			handle_incoming_peer_message(ctx, state, remote, msg).await?;
		}
	}

	Ok(())
}

/// Handles our view changes.
async fn handle_our_view_change(
	state: &mut State,
	view: View,
) -> Result<()> {
	let old_view = std::mem::replace(&mut (state.view), view);

	let view = state.view.clone();

	let removed = old_view.difference(&view).collect::<Vec<_>>();

	for removed in removed.into_iter() {
		state.collations.remove(&removed);
		if let Some(group) = state.our_validators_groups.remove(&removed) {
			state.known_validators.retain(|_, v| !group.contains(v));
		}
	}

	Ok(())
}

/// The collator protocol collator side main loop.
pub(crate) async fn run<Context>(
	mut ctx: Context,
	our_id: CollatorId,
	metrics: Metrics,
) -> Result<()>
where
	Context: SubsystemContext<Message = CollatorProtocolMessage>
{
	use FromOverseer::*;
	use OverseerSignal::*;

	let mut state = State {
		metrics,
		..Default::default()
	};

	state.our_id = our_id;

	loop {
		match ctx.recv().await? {
			Communication { msg } => process_msg(&mut ctx, &mut state, msg).await?,
			Signal(ActiveLeaves(_update)) => {}
			Signal(BlockFinalized(_)) => {}
			Signal(Conclude) => break,
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	use log::trace;
	use std::time::Duration;
	use futures::{executor, future, Future};
	use assert_matches::assert_matches;
	use smallvec::smallvec;

	use sp_core::crypto::Pair;
	use sp_keyring::Sr25519Keyring;

	use polkadot_primitives::v1::{
		BlockData, CandidateDescriptor, CollatorPair, ScheduledCore,
		ValidatorIndex, GroupRotationInfo,
	};
	use polkadot_subsystem::ActiveLeavesUpdate;
	use polkadot_node_subsystem_util::TimeoutExt;
	use polkadot_subsystem_testhelpers as test_helpers;
	use polkadot_node_network_protocol::ObservedRole;

	#[derive(Default)]
	struct TestCandidateBuilder {
		para_id: ParaId,
		pov_hash: Hash,
		relay_parent: Hash,
		commitments_hash: Hash,
	}

	impl TestCandidateBuilder {
		fn build(self) -> CandidateReceipt {
			CandidateReceipt {
				descriptor: CandidateDescriptor {
					para_id: self.para_id,
					pov_hash: self.pov_hash,
					relay_parent: self.relay_parent,
					..Default::default()
				},
				commitments_hash: self.commitments_hash,
			}
		}
	}

	#[derive(Clone)]
	struct TestState {
		chain_ids: Vec<ParaId>,
		validators: Vec<Sr25519Keyring>,
		validator_public: Vec<ValidatorId>,
		validator_peer_id: Vec<PeerId>,
		validator_groups: (Vec<Vec<ValidatorIndex>>, GroupRotationInfo),
		relay_parent: Hash,
		availability_cores: Vec<CoreState>,
		our_collator_pair: CollatorPair,
	}

	fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> Vec<ValidatorId> {
		val_ids.iter().map(|v| v.public().into()).collect()
	}

	impl Default for TestState {
		fn default() -> Self {
			let chain_a = ParaId::from(1);
			let chain_b = ParaId::from(2);

			let chain_ids = vec![chain_a, chain_b];

			let validators = vec![
				Sr25519Keyring::Alice,
				Sr25519Keyring::Bob,
				Sr25519Keyring::Charlie,
				Sr25519Keyring::Dave,
				Sr25519Keyring::Ferdie,
			];

			let validator_public = validator_pubkeys(&validators);

			let validator_peer_id = std::iter::repeat_with(|| PeerId::random())
				.take(validator_public.len())
				.collect();

			let validator_groups = vec![vec![2, 0, 4], vec![1], vec![3]];
			let group_rotation_info = GroupRotationInfo {
				session_start_block: 0,
				group_rotation_frequency: 100,
				now: 1,
			};
			let validator_groups = (validator_groups, group_rotation_info);

			let availability_cores = vec![
				CoreState::Scheduled(ScheduledCore {
					para_id: chain_ids[0],
					collator: None,
				}),
				CoreState::Scheduled(ScheduledCore {
					para_id: chain_ids[1],
					collator: None,
				}),
			];

			let relay_parent = Hash::repeat_byte(0x05);

			let our_collator_pair = CollatorPair::generate().0;

			Self {
				chain_ids,
				validators,
				validator_public,
				validator_peer_id,
				validator_groups,
				relay_parent,
				availability_cores,
				our_collator_pair,
			}
		}
	}

	struct TestHarness {
		virtual_overseer: test_helpers::TestSubsystemContextHandle<CollatorProtocolMessage>,
	}

	fn test_harness<T: Future<Output = ()>>(
		collator_id: CollatorId,
		test: impl FnOnce(TestHarness) -> T,
	) {
		let _ = env_logger::builder()
			.is_test(true)
			.filter(
				Some("polkadot_collator_protocol"),
				log::LevelFilter::Trace,
			)
			.filter(
				Some(TARGET),
				log::LevelFilter::Trace,
			)
			.try_init();

		let pool = sp_core::testing::TaskExecutor::new();

		let (context, virtual_overseer) = test_helpers::make_subsystem_context(pool.clone());

		let subsystem = run(context, collator_id, Metrics::default());

		let test_fut = test(TestHarness { virtual_overseer });

		futures::pin_mut!(test_fut);
		futures::pin_mut!(subsystem);

		executor::block_on(future::select(test_fut, subsystem));
	}

	const TIMEOUT: Duration = Duration::from_millis(100);

	async fn overseer_send(
		overseer: &mut test_helpers::TestSubsystemContextHandle<CollatorProtocolMessage>,
		msg: CollatorProtocolMessage,
	) {
		trace!("Sending message:\n{:?}", &msg);
		overseer
			.send(FromOverseer::Communication { msg })
			.timeout(TIMEOUT)
			.await
			.expect(&format!("{:?} is more than enough for sending messages.", TIMEOUT));
	}

	async fn overseer_recv(
		overseer: &mut test_helpers::TestSubsystemContextHandle<CollatorProtocolMessage>,
	) -> AllMessages {
		let msg = overseer_recv_with_timeout(overseer, TIMEOUT)
			.await
			.expect(&format!("{:?} is more than enough to receive messages", TIMEOUT));

		trace!("Received message:\n{:?}", &msg);

		msg
	}

	async fn overseer_recv_with_timeout(
		overseer: &mut test_helpers::TestSubsystemContextHandle<CollatorProtocolMessage>,
		timeout: Duration,
	) -> Option<AllMessages> {
		trace!("Waiting for message...");
		overseer
			.recv()
			.timeout(timeout)
			.await
	}

	async fn overseer_signal(
		overseer: &mut test_helpers::TestSubsystemContextHandle<CollatorProtocolMessage>,
		signal: OverseerSignal,
	) {
		overseer
			.send(FromOverseer::Signal(signal))
			.timeout(TIMEOUT)
			.await
			.expect(&format!("{:?} is more than enough for sending signals.", TIMEOUT));
	}

	#[test]
	fn advertise_and_send_collation() {
		let test_state = TestState::default();

		test_harness(test_state.our_collator_pair.public(), |test_harness| async move {
			let current = test_state.relay_parent;
			let TestHarness {
				mut virtual_overseer,
			} = test_harness;

			let pov_block = PoV {
				block_data: BlockData(vec![42, 43, 44]),
			};

			let pov_hash = pov_block.hash();

			let candidate = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				..Default::default()
			}.build();

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.chain_ids[0])
			).await;

			overseer_signal(
				&mut virtual_overseer,
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
					activated: smallvec![current.clone()],
					deactivated: smallvec![],
				}),
			).await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdateV1(
					NetworkBridgeEvent::OurViewChange(View(vec![current])),
				),
			).await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::DistributeCollation(candidate.clone(), pov_block.clone()),
			).await;

			// obtain the availability cores.
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::AvailabilityCores(tx)
				)) => {
					assert_eq!(relay_parent, current);
					tx.send(Ok(test_state.availability_cores.clone())).unwrap();
				}
			);

			// Obtain the validator groups
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::ValidatorGroups(tx)
				)) => {
					assert_eq!(relay_parent, current);
					tx.send(Ok(test_state.validator_groups.clone())).unwrap();
				}
			);

			// obtain the validators per relay parent
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Validators(tx),
				)) => {
					assert_eq!(relay_parent, current);
					tx.send(Ok(test_state.validator_public.clone())).unwrap();
				}
			);

			// We now should connect to our validator group.
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridge(
					NetworkBridgeMessage::ConnectToValidators(
						peer_set,
						validators,
						tx,
					)
				) => {
					assert_eq!(peer_set, PeerSet::Collation);
					assert_eq!(validators.len(), 4);
					assert!(validators.contains(&test_state.validator_public[2]));
					assert!(validators.contains(&test_state.validator_public[0]));
					assert!(validators.contains(&test_state.validator_public[4]));
					assert!(validators.contains(&test_state.validator_public[1]));

					tx.send(vec![
						(test_state.validator_public[2].clone(), test_state.validator_peer_id[2].clone()),
						(test_state.validator_public[0].clone(), test_state.validator_peer_id[0].clone()),
						(test_state.validator_public[4].clone(), test_state.validator_peer_id[4].clone()),
						(test_state.validator_public[1].clone(), test_state.validator_peer_id[1].clone()),
					]).unwrap();
				}
			);

			// Validator 2 connects.
			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdateV1(
					NetworkBridgeEvent::PeerConnected(
						test_state.validator_peer_id[2].clone(),
						ObservedRole::Authority,
					)
				),
			).await;

			// We declare to the connected validator that we are a collator.
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridge(
					NetworkBridgeMessage::SendCollationMessage(
						to,
						protocol_v1::CollationProtocol::CollatorProtocol(wire_message),
					)
				) => {
					assert_eq!(to, vec![test_state.validator_peer_id[2].clone()]);
					assert_matches!(
						wire_message,
						protocol_v1::CollatorProtocolMessage::Declare(collator_id) => {
							assert_eq!(collator_id, test_state.our_collator_pair.public());
						}
					);
				}
			);

			// Send info about peer's view.
			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdateV1(
					NetworkBridgeEvent::PeerViewChange(
						test_state.validator_peer_id[2].clone(),
						View(vec![current]),
					)
				)
			).await;

			// The peer is interested in a leaf that we have a collation for;
			// advertise it.
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridge(
					NetworkBridgeMessage::SendCollationMessage(
						to,
						protocol_v1::CollationProtocol::CollatorProtocol(wire_message),
					)
				) => {
					assert_eq!(to, vec![test_state.validator_peer_id[2].clone()]);
					assert_matches!(
						wire_message,
						protocol_v1::CollatorProtocolMessage::AdvertiseCollation(
							relay_parent,
							collating_on,
						) => {
							assert_eq!(relay_parent, current);
							assert_eq!(collating_on, test_state.chain_ids[0]);
						}
					);
				}
			);

			let request_id = 42;

			// Request a collation.
			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdateV1(
					NetworkBridgeEvent::PeerMessage(
						test_state.validator_peer_id[2].clone(),
						protocol_v1::CollatorProtocolMessage::RequestCollation(
							request_id,
							current,
							test_state.chain_ids[0],
						)
					)
				)
			).await;

			// Wait for the reply.
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridge(
					NetworkBridgeMessage::SendCollationMessage(
						to,
						protocol_v1::CollationProtocol::CollatorProtocol(wire_message),
					)
				) => {
					assert_eq!(to, vec![test_state.validator_peer_id[2].clone()]);
					assert_matches!(
						wire_message,
						protocol_v1::CollatorProtocolMessage::Collation(req_id, receipt, pov) => {
							assert_eq!(req_id, request_id);
							assert_eq!(receipt, candidate);
							assert_eq!(pov, pov_block);
						}
					);
				}
			);

			let new_head = Hash::repeat_byte(0xA);

			// Collator's view moves on.
			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdateV1(
					NetworkBridgeEvent::OurViewChange(View(vec![new_head])),
				),
			).await;

			let request_id = 43;

			// Re-request a collation.
			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdateV1(
					NetworkBridgeEvent::PeerMessage(
						test_state.validator_peer_id[2].clone(),
						protocol_v1::CollatorProtocolMessage::RequestCollation(
							request_id,
							current,
							test_state.chain_ids[0],
						)
					)
				)
			).await;

			assert!(overseer_recv_with_timeout(&mut virtual_overseer, TIMEOUT).await.is_none());
		});
	}
}
