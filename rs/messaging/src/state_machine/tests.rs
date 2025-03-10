use super::*;
use crate::{
    routing::demux::MockDemux, routing::stream_builder::MockStreamBuilder,
    state_machine::StateMachineImpl,
};
use ic_ic00_types::EcdsaKeyId;
use ic_interfaces::execution_environment::Scheduler;
use ic_interfaces_state_manager::StateManager;
use ic_metrics::MetricsRegistry;
use ic_registry_subnet_features::SubnetFeatures;
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{ReplicatedState, SubnetTopology};
use ic_test_utilities::{
    state_manager::FakeStateManager, types::batch::BatchBuilder, types::ids::subnet_test_id,
    types::messages::SignedIngressBuilder,
};
use ic_test_utilities_execution_environment::test_registry_settings;
use ic_test_utilities_logger::with_test_replica_logger;
use ic_types::messages::SignedIngress;
use ic_types::{batch::BatchMessages, crypto::canister_threshold_sig::MasterEcdsaPublicKey};
use ic_types::{Height, PrincipalId, SubnetId};
use mockall::{mock, predicate::*, Sequence};
use std::collections::{BTreeMap, BTreeSet};

mock! {
    pub Scheduler {}
    trait Scheduler {
        type State = ReplicatedState;
        fn execute_round(
            &self,
            state: ic_replicated_state::ReplicatedState,
            randomness: ic_types::Randomness,
            ecdsa_subnet_public_keys: BTreeMap<EcdsaKeyId, MasterEcdsaPublicKey>,
            current_round: ExecutionRound,
            current_round_type: ExecutionRoundType,
            registry_settings: &RegistryExecutionSettings,
        ) -> ReplicatedState;
    }
}

struct StateMachineTestFixture {
    scheduler: Box<dyn Scheduler<State = ReplicatedState>>,
    demux: Box<dyn Demux>,
    stream_builder: Box<dyn StreamBuilder>,
    initial_state: ReplicatedState,
    network_topology: NetworkTopology,
    metrics: Arc<MessageRoutingMetrics>,
}

/// Returns a test fixture for state machine tests with Mocks for Demux,
/// Scheduler, and StreamBuilder. The Mocks will ensure that a panic
/// occurs if they are called in the wrong order.
fn test_fixture(provided_batch: &Batch) -> StateMachineTestFixture {
    // Initial state provided by the state manager.
    let initial_height = Height::from(provided_batch.batch_number.get() - 1);
    let state_manager = FakeStateManager::new();
    let (_height, initial_state) = state_manager.take_tip();
    let metrics_registry = MetricsRegistry::new();
    let metrics = Arc::new(MessageRoutingMetrics::new(&metrics_registry));

    let round = ExecutionRound::from(initial_height.get() + 1);
    let round_type = if provided_batch.requires_full_state_hash {
        ExecutionRoundType::CheckpointRound
    } else {
        ExecutionRoundType::OrdinaryRound
    };

    let mut seq = Sequence::new();

    let mut demux = Box::new(MockDemux::new());
    demux
        .expect_process_payload()
        .times(1)
        .in_sequence(&mut seq)
        .with(always(), eq(provided_batch.messages.clone()))
        .returning(|state, _| state);

    let mut scheduler = Box::new(MockScheduler::new());
    scheduler
        .expect_execute_round()
        .times(1)
        .in_sequence(&mut seq)
        .with(
            always(),
            eq(provided_batch.randomness),
            eq(provided_batch.ecdsa_subnet_public_keys.clone()),
            eq(round),
            eq(round_type),
            eq(test_registry_settings()),
        )
        .returning(|state, _, _, _, _, _| state);

    let mut stream_builder = Box::new(MockStreamBuilder::new());
    stream_builder
        .expect_build_streams()
        .times(1)
        .in_sequence(&mut seq)
        .with(always())
        .returning(|state| state);

    let mut subnets = BTreeMap::new();
    subnets.insert(
        subnet_test_id(0),
        SubnetTopology {
            public_key: vec![0, 1, 2, 3],
            nodes: BTreeSet::new(),
            subnet_type: SubnetType::Application,
            subnet_features: SubnetFeatures::default(),
            ecdsa_keys_held: BTreeSet::new(),
        },
    );

    let network_topology = NetworkTopology {
        subnets,
        routing_table: Default::default(),
        nns_subnet_id: SubnetId::from(PrincipalId::new_subnet_test_id(0)),
        canister_migrations: Default::default(),
        ..Default::default()
    };

    StateMachineTestFixture {
        scheduler,
        demux,
        stream_builder,
        initial_state,
        network_topology,
        metrics,
    }
}

// Utility to build an Ingress message.
fn signed_ingress() -> SignedIngress {
    SignedIngressBuilder::new()
        .sign_for_randomly_generated_sender()
        .build()
}

#[test]
fn state_machine_populates_network_topology() {
    let provided_batch = BatchBuilder::new().batch_number(Height::new(1)).build();
    let fixture = test_fixture(&provided_batch);

    with_test_replica_logger(|log| {
        let _ = &fixture;
        let state_machine = Box::new(StateMachineImpl::new(
            fixture.scheduler,
            fixture.demux,
            fixture.stream_builder,
            log,
            fixture.metrics,
        ));

        assert_ne!(
            fixture.initial_state.metadata.network_topology,
            fixture.network_topology.clone()
        );

        let state = state_machine.execute_round(
            fixture.initial_state,
            fixture.network_topology.clone(),
            provided_batch,
            Default::default(),
            &test_registry_settings(),
        );

        assert_eq!(state.metadata.network_topology, fixture.network_topology);
    });
}

// Tests the processing of a batch. Ensures that the Demux, Scheduler, and
// StreamBuilder are invoked in order and that all of them are called.
fn test_delivered_batch(provided_batch: Batch) {
    let fixture = test_fixture(&provided_batch);

    with_test_replica_logger(|log| {
        let _ = &fixture;
        let state_machine = Box::new(StateMachineImpl::new(
            fixture.scheduler,
            fixture.demux,
            fixture.stream_builder,
            log,
            fixture.metrics,
        ));

        let _state_after = state_machine.execute_round(
            fixture.initial_state,
            NetworkTopology::default(),
            provided_batch,
            Default::default(),
            &test_registry_settings(),
        );
    });
}

// Parameterized test engine for changing the number of ingress messages
// included in the provided batch.
fn param_batch_test(batch_num: Height, in_count: u64) {
    let mut ingress_messages = Vec::<SignedIngress>::new();
    for _ in 0..in_count {
        let in_msg = signed_ingress();
        ingress_messages.push(in_msg);
    }

    let batch_builder = BatchBuilder::new();
    let provided_batch = batch_builder
        .messages(BatchMessages {
            signed_ingress_msgs: ingress_messages,
            ..BatchMessages::default()
        })
        .batch_number(batch_num)
        .build();

    test_delivered_batch(provided_batch);
}

#[test]
fn test_delivered_batch_interface() {
    for i in 0..2 {
        param_batch_test(Height::from(27), i);
    }
}
