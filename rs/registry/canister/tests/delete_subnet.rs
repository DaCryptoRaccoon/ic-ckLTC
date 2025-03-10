use candid::Encode;
use cycles_minting_canister::CyclesCanisterInitPayload;
use ic_base_types::{PrincipalId, SubnetId};
use ic_nns_common::registry::encode_or_panic;
use ic_nns_constants::{
    CYCLES_MINTING_CANISTER_ID, GOVERNANCE_CANISTER_ID, LEDGER_CANISTER_ID, NNS_SUBNET_ID,
};
use ic_nns_test_utils::registry::new_node_keys_and_node_id;
use ic_nns_test_utils::{
    itest_helpers::{
        forward_call_via_universal_canister, local_test_on_nns_subnet,
        set_up_cycles_minting_canister, set_up_registry_canister, set_up_universal_canister,
    },
    registry::{invariant_compliant_mutation_as_atomic_req, INITIAL_MUTATION_ID},
};
use ic_protobuf::registry::{
    crypto::v1::PublicKey,
    node::v1::{ConnectionEndpoint, NodeRecord, Protocol},
    subnet::v1::{CatchUpPackageContents, SubnetListRecord, SubnetRecord},
};
use ic_registry_keys::{
    make_catch_up_package_contents_key, make_crypto_threshold_signing_pubkey_key,
    make_subnet_list_record_key, make_subnet_record_key,
};
use ic_registry_subnet_type::SubnetType;
use ic_registry_transport::{insert, pb::v1::RegistryAtomicMutateRequest, update};
use ic_test_utilities::types::ids::user_test_id;
use ic_types::{p2p::build_default_gossip_config, ReplicaVersion};
use registry_canister::mutations::node_management::common::make_add_node_registry_mutations;
use registry_canister::{
    init::RegistryCanisterInitPayloadBuilder, mutations::do_delete_subnet::DeleteSubnetPayload,
};

#[test]
fn test_subnet_is_only_deleted_when_appropriate() {
    local_test_on_nns_subnet(|runtime| async move {
        let (valid_pks_2, node_pid_2) = new_node_keys_and_node_id();
        let (valid_pks_3, node_pid_3) = new_node_keys_and_node_id();
        let node_operator_pid = user_test_id(999);
        let subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(999));
        let application_subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(997));
        let second_system_subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(998));
        let replica_version_id = ReplicaVersion::default().to_string();
        let init_mutation = invariant_compliant_mutation_as_atomic_req(INITIAL_MUTATION_ID);
        let node_2 = {
            let effective_id = 1 + INITIAL_MUTATION_ID;
            let xnet = Some(ConnectionEndpoint {
                ip_addr: format!("128.0.{effective_id}.1"),
                port: 1234,
                protocol: Protocol::Http1 as i32,
            });
            let http = Some(ConnectionEndpoint {
                ip_addr: format!("128.0.{effective_id}.1"),
                port: 4321,
                protocol: Protocol::Http1 as i32,
            });
            NodeRecord {
                node_operator_id: node_operator_pid.get().to_vec(),
                xnet,
                http,
                ..Default::default()
            }
        };
        let node_3 = {
            let effective_id = 2 + INITIAL_MUTATION_ID;
            let xnet = Some(ConnectionEndpoint {
                ip_addr: format!("128.0.{effective_id}.1"),
                port: 1234,
                protocol: Protocol::Http1 as i32,
            });
            let http = Some(ConnectionEndpoint {
                ip_addr: format!("128.0.{effective_id}.1"),
                port: 4321,
                protocol: Protocol::Http1 as i32,
            });
            NodeRecord {
                node_operator_id: node_operator_pid.get().to_vec(),
                xnet,
                http,
                ..Default::default()
            }
        };

        let application_subnet_cup = CatchUpPackageContents::default();
        let application_subnet_pk = PublicKey::default();
        let application_subnet = SubnetRecord {
            membership: vec![node_pid_2.get().to_vec()],
            subnet_type: i32::from(SubnetType::Application),
            replica_version_id: replica_version_id.clone(),
            unit_delay_millis: 600,
            gossip_config: Some(build_default_gossip_config()),
            ..Default::default()
        };
        let second_system_subnet_cup = CatchUpPackageContents::default();
        let second_system_subnet_pk = PublicKey::default();
        let second_system_subnet = SubnetRecord {
            membership: vec![node_pid_3.get().to_vec()],
            subnet_type: i32::from(SubnetType::System),
            replica_version_id,
            unit_delay_millis: 600,
            gossip_config: Some(build_default_gossip_config()),
            ..Default::default()
        };

        let subnet_list = SubnetListRecord {
            subnets: vec![
                subnet_id.get().to_vec(),
                application_subnet_id.get().to_vec(),
                second_system_subnet_id.get().to_vec(),
            ],
        };

        let mut mutations = vec![
            insert(
                make_subnet_record_key(application_subnet_id).as_bytes(),
                encode_or_panic(&application_subnet),
            ),
            insert(
                make_catch_up_package_contents_key(application_subnet_id).as_bytes(),
                encode_or_panic(&application_subnet_cup),
            ),
            insert(
                make_crypto_threshold_signing_pubkey_key(application_subnet_id).as_bytes(),
                encode_or_panic(&application_subnet_pk),
            ),
            insert(
                make_catch_up_package_contents_key(second_system_subnet_id).as_bytes(),
                encode_or_panic(&second_system_subnet_cup),
            ),
            insert(
                make_crypto_threshold_signing_pubkey_key(second_system_subnet_id).as_bytes(),
                encode_or_panic(&second_system_subnet_pk),
            ),
            insert(
                make_subnet_record_key(second_system_subnet_id).as_bytes(),
                encode_or_panic(&second_system_subnet),
            ),
            update(
                make_subnet_list_record_key().as_bytes(),
                encode_or_panic(&subnet_list),
            ),
        ];

        mutations.append(&mut make_add_node_registry_mutations(
            node_pid_2,
            node_2,
            valid_pks_2,
        ));
        mutations.append(&mut make_add_node_registry_mutations(
            node_pid_3,
            node_3,
            valid_pks_3,
        ));

        let registry = set_up_registry_canister(
            &runtime,
            RegistryCanisterInitPayloadBuilder::new()
                .push_init_mutate_request(init_mutation)
                .push_init_mutate_request(RegistryAtomicMutateRequest {
                    mutations,
                    preconditions: vec![],
                })
                .build(),
        )
        .await;

        // Install the universal canister in place of the governance canister
        let fake_governance_canister = set_up_universal_canister(&runtime).await;
        let _ = set_up_universal_canister(&runtime).await;
        let _ = set_up_universal_canister(&runtime).await;
        // // Install the universal canister in place of the cycles minting canister
        // let fake_cmc = set_up_universal_canister(&runtime).await;
        let cmc = set_up_cycles_minting_canister(
            &runtime,
            Some(CyclesCanisterInitPayload {
                ledger_canister_id: Some(LEDGER_CANISTER_ID),
                governance_canister_id: Some(GOVERNANCE_CANISTER_ID),
                exchange_rate_canister: None,
                minting_account_id: Some(GOVERNANCE_CANISTER_ID.get().into()),
                last_purged_notification: Some(1),
            }),
        )
        .await;
        // Since it takes the id reserved for the governance canister, it can
        // impersonate it
        assert_eq!(
            fake_governance_canister.canister_id(),
            GOVERNANCE_CANISTER_ID
        );
        assert_eq!(cmc.canister_id(), CYCLES_MINTING_CANISTER_ID);

        let nonexistent_subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(1));
        let payload = DeleteSubnetPayload {
            subnet_id: Some(nonexistent_subnet_id.get()),
        };

        // Cannot delete nonexistent Subnet
        assert!(
            !forward_call_via_universal_canister(
                &fake_governance_canister,
                &registry,
                "delete_subnet",
                Encode!(&payload).unwrap()
            )
            .await
        );

        let payload = DeleteSubnetPayload {
            subnet_id: Some((*NNS_SUBNET_ID).get()),
        };

        // Cannot delete the NNS Subnet
        assert!(
            !forward_call_via_universal_canister(
                &fake_governance_canister,
                &registry,
                "delete_subnet",
                Encode!(&payload).unwrap()
            )
            .await
        );

        let payload = DeleteSubnetPayload {
            subnet_id: Some(subnet_id.get()),
        };

        // Cannot delete the only system Subnet
        assert!(
            !forward_call_via_universal_canister(
                &fake_governance_canister,
                &registry,
                "delete_subnet",
                Encode!(&payload).unwrap()
            )
            .await
        );

        let payload = DeleteSubnetPayload {
            subnet_id: Some(application_subnet_id.get()),
        };

        // Deleting application Subnets succeeds
        assert!(
            forward_call_via_universal_canister(
                &fake_governance_canister,
                &registry,
                "delete_subnet",
                Encode!(&payload).unwrap()
            )
            .await
        );

        let payload = DeleteSubnetPayload {
            subnet_id: Some(second_system_subnet_id.get()),
        };

        // Deleting second system Subnet succeeds
        assert!(
            forward_call_via_universal_canister(
                &fake_governance_canister,
                &registry,
                "delete_subnet",
                Encode!(&payload).unwrap()
            )
            .await
        );

        Ok(())
    })
}
