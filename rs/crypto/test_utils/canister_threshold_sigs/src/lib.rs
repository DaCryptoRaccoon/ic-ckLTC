//! Utilities for testing IDkg and canister threshold signature operations.

use crate::node::{Node, Nodes};
use ic_crypto_internal_csp::Csp;
use ic_crypto_internal_threshold_sig_ecdsa::test_utils::corrupt_dealing;
use ic_crypto_internal_threshold_sig_ecdsa::{IDkgDealingInternal, NodeIndex, Seed};
use ic_crypto_temp_crypto::{TempCryptoComponent, TempCryptoComponentGeneric};
use ic_crypto_test_utils_reproducible_rng::ReproducibleRng;
use ic_interfaces::crypto::{
    BasicSigner, KeyManager, ThresholdEcdsaSigVerifier, ThresholdEcdsaSigner,
};
use ic_registry_client_fake::FakeRegistryClient;
use ic_registry_keys::make_crypto_node_key;
use ic_registry_proto_data_provider::ProtoRegistryDataProvider;
use ic_types::crypto::canister_threshold_sig::idkg::{
    IDkgComplaint, IDkgDealers, IDkgDealing, IDkgMaskedTranscriptOrigin, IDkgReceivers,
    IDkgTranscript, IDkgTranscriptId, IDkgTranscriptOperation, IDkgTranscriptParams,
    IDkgTranscriptType, IDkgUnmaskedTranscriptOrigin, SignedIDkgDealing,
};
use ic_types::crypto::canister_threshold_sig::{
    ExtendedDerivationPath, PreSignatureQuadruple, ThresholdEcdsaSigShare,
};
use ic_types::crypto::canister_threshold_sig::{
    ThresholdEcdsaCombinedSignature, ThresholdEcdsaSigInputs,
};
use ic_types::crypto::{AlgorithmId, KeyPurpose, Signed};
use ic_types::crypto::{BasicSig, BasicSigOf};
use ic_types::signature::{BasicSignature, BasicSignatureBatch};
use ic_types::{Height, NodeId, PrincipalId, Randomness, RegistryVersion, SubnetId};
use rand::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub mod dummy_values;

pub fn create_params_for_dealers<R: RngCore + CryptoRng>(
    dealer_set: &BTreeSet<NodeId>,
    operation: IDkgTranscriptOperation,
    rng: &mut R,
) -> IDkgTranscriptParams {
    IDkgTranscriptParams::new(
        random_transcript_id(rng),
        dealer_set.clone(),
        dealer_set.clone(),
        RegistryVersion::from(0),
        AlgorithmId::ThresholdEcdsaSecp256k1,
        operation,
    )
    .expect("Should be able to create IDKG params")
}

pub fn mock_unmasked_transcript_type<R: RngCore + CryptoRng>(rng: &mut R) -> IDkgTranscriptType {
    IDkgTranscriptType::Unmasked(IDkgUnmaskedTranscriptOrigin::ReshareMasked(
        random_transcript_id(rng),
    ))
}

pub fn mock_masked_transcript_type() -> IDkgTranscriptType {
    IDkgTranscriptType::Masked(IDkgMaskedTranscriptOrigin::Random)
}

pub fn mock_transcript<R: RngCore + CryptoRng>(
    receivers: Option<BTreeSet<NodeId>>,
    transcript_type: IDkgTranscriptType,
    rng: &mut R,
) -> IDkgTranscript {
    let receivers = match receivers {
        Some(receivers) => receivers,
        None => {
            let mut receivers = BTreeSet::new();
            for i in 1..10 {
                receivers.insert(node_id(i));
            }
            receivers
        }
    };

    IDkgTranscript {
        transcript_id: random_transcript_id(rng),
        receivers: IDkgReceivers::new(receivers).unwrap(),
        registry_version: RegistryVersion::from(314),
        verified_dealings: BTreeMap::new(),
        transcript_type,
        algorithm_id: AlgorithmId::ThresholdEcdsaSecp256k1,
        internal_transcript_raw: vec![],
    }
}

pub fn swap_two_dealings_in_transcript(
    params: &IDkgTranscriptParams,
    transcript: IDkgTranscript,
    env: &CanisterThresholdSigTestEnvironment,
    dealer_a: &Node,
    dealer_b: &Node,
) -> IDkgTranscript {
    assert_ne!(dealer_a, dealer_b);

    let a_idx = transcript.index_for_dealer_id(dealer_a.id()).unwrap();
    let b_idx = transcript.index_for_dealer_id(dealer_b.id()).unwrap();

    let dealing_a = transcript
        .verified_dealings
        .get(&a_idx)
        .expect("Dealing exists")
        .clone();

    let dealing_b = transcript
        .verified_dealings
        .get(&b_idx)
        .expect("Dealing exists")
        .clone();

    let dealing_ba = dealing_b
        .content
        .into_builder()
        .with_dealer_id(dealer_a.id())
        .build_with_signature(params, dealer_a, dealer_a.id());

    let dealing_ab = dealing_a
        .content
        .into_builder()
        .with_dealer_id(dealer_b.id())
        .build_with_signature(params, dealer_b, dealer_b.id());

    let dealing_ab_signed = env
        .nodes
        .support_dealing_from_all_receivers(dealing_ab, params);

    let dealing_ba_signed = env
        .nodes
        .support_dealing_from_all_receivers(dealing_ba, params);

    let mut transcript = transcript;

    assert!(transcript
        .verified_dealings
        .insert(a_idx, dealing_ba_signed)
        .is_some());
    assert!(transcript
        .verified_dealings
        .insert(b_idx, dealing_ab_signed)
        .is_some());

    transcript
}

pub fn generate_key_transcript<R: RngCore + CryptoRng>(
    env: &CanisterThresholdSigTestEnvironment,
    dealers: &IDkgDealers,
    receivers: &IDkgReceivers,
    algorithm_id: AlgorithmId,
    rng: &mut R,
) -> IDkgTranscript {
    let masked_key_params = env.params_for_random_sharing(dealers, receivers, algorithm_id, rng);

    let masked_key_transcript = env
        .nodes
        .run_idkg_and_create_and_verify_transcript(&masked_key_params, rng);

    let unmasked_key_params = build_params_from_previous(
        masked_key_params,
        IDkgTranscriptOperation::ReshareOfMasked(masked_key_transcript),
        rng,
    );

    env.nodes
        .run_idkg_and_create_and_verify_transcript(&unmasked_key_params, rng)
}

pub fn generate_presig_quadruple<R: RngCore + CryptoRng>(
    env: &CanisterThresholdSigTestEnvironment,
    dealers: &IDkgDealers,
    receivers: &IDkgReceivers,
    algorithm_id: AlgorithmId,
    key_transcript: &IDkgTranscript,
    rng: &mut R,
) -> PreSignatureQuadruple {
    let lambda_params = env.params_for_random_sharing(dealers, receivers, algorithm_id, rng);
    let lambda_transcript = env
        .nodes
        .run_idkg_and_create_and_verify_transcript(&lambda_params, rng);

    let kappa_transcript = {
        let masked_kappa_params =
            env.params_for_random_sharing(dealers, receivers, algorithm_id, rng);

        let masked_kappa_transcript = env
            .nodes
            .run_idkg_and_create_and_verify_transcript(&masked_kappa_params, rng);

        let unmasked_kappa_params = build_params_from_previous(
            masked_kappa_params,
            IDkgTranscriptOperation::ReshareOfMasked(masked_kappa_transcript),
            rng,
        );

        env.nodes
            .run_idkg_and_create_and_verify_transcript(&unmasked_kappa_params, rng)
    };

    let kappa_times_lambda_transcript = {
        let kappa_times_lambda_params = build_params_from_previous(
            lambda_params.clone(),
            IDkgTranscriptOperation::UnmaskedTimesMasked(
                kappa_transcript.clone(),
                lambda_transcript.clone(),
            ),
            rng,
        );

        env.nodes
            .run_idkg_and_create_and_verify_transcript(&kappa_times_lambda_params, rng)
    };

    let key_times_lambda_transcript = {
        let key_times_lambda_params = build_params_from_previous(
            lambda_params,
            IDkgTranscriptOperation::UnmaskedTimesMasked(
                key_transcript.clone(),
                lambda_transcript.clone(),
            ),
            rng,
        );

        env.nodes
            .run_idkg_and_create_and_verify_transcript(&key_times_lambda_params, rng)
    };

    PreSignatureQuadruple::new(
        kappa_transcript,
        lambda_transcript,
        kappa_times_lambda_transcript,
        key_times_lambda_transcript,
    )
    .unwrap_or_else(|error| panic!("failed to create pre-signature quadruple: {:?}", error))
}

/// Creates a new `IDkgTranscriptParams` with all information copied from a
/// previous one, except the operation (as given) and the Id
/// (randomly-generated, to avoid collisions).
pub fn build_params_from_previous<R: RngCore + CryptoRng>(
    previous_params: IDkgTranscriptParams,
    operation_type: IDkgTranscriptOperation,
    rng: &mut R,
) -> IDkgTranscriptParams {
    IDkgTranscriptParams::new(
        random_transcript_id(rng),
        previous_params.dealers().get().clone(),
        previous_params.receivers().get().clone(),
        previous_params.registry_version(),
        previous_params.algorithm_id(),
        operation_type,
    )
    .expect("failed to create resharing/multiplication IDkgTranscriptParams")
}

pub mod node {
    use crate::{IDkgParticipants, IDkgParticipantsRandom};
    use ic_crypto_internal_csp::Csp;
    use ic_crypto_temp_crypto::{TempCryptoComponent, TempCryptoComponentGeneric};
    use ic_crypto_test_utils_reproducible_rng::ReproducibleRng;
    use ic_interfaces::crypto::{
        BasicSigVerifier, BasicSigner, CurrentNodePublicKeysError, IDkgProtocol, KeyManager,
        ThresholdEcdsaSigVerifier, ThresholdEcdsaSigner,
    };
    use ic_logger::ReplicaLogger;
    use ic_protobuf::log::log_entry::v1::LogEntry;
    use ic_registry_client_fake::FakeRegistryClient;
    use ic_test_utilities_in_memory_logger::InMemoryReplicaLogger;
    use ic_types::consensus::get_faults_tolerated;
    use ic_types::crypto::canister_threshold_sig::error::{
        IDkgCreateDealingError, IDkgCreateTranscriptError, IDkgLoadTranscriptError,
        IDkgOpenTranscriptError, IDkgRetainKeysError, IDkgVerifyComplaintError,
        IDkgVerifyDealingPrivateError, IDkgVerifyDealingPublicError,
        IDkgVerifyInitialDealingsError, IDkgVerifyOpeningError, IDkgVerifyTranscriptError,
        ThresholdEcdsaCombineSigSharesError, ThresholdEcdsaSignShareError,
        ThresholdEcdsaVerifyCombinedSignatureError, ThresholdEcdsaVerifySigShareError,
    };
    use ic_types::crypto::canister_threshold_sig::idkg::{
        BatchSignedIDkgDealing, BatchSignedIDkgDealings, IDkgComplaint, IDkgDealers, IDkgOpening,
        IDkgReceivers, IDkgTranscript, IDkgTranscriptOperation, IDkgTranscriptParams,
        InitialIDkgDealings, SignedIDkgDealing,
    };
    use ic_types::crypto::canister_threshold_sig::{
        ThresholdEcdsaCombinedSignature, ThresholdEcdsaSigInputs, ThresholdEcdsaSigShare,
    };
    use ic_types::crypto::{BasicSigOf, CryptoResult, CurrentNodePublicKeys, Signable};
    use ic_types::signature::BasicSignatureBatch;
    use ic_types::{NodeId, RegistryVersion};
    use rand::seq::IteratorRandom;
    use rand::{CryptoRng, Rng, RngCore};
    use std::cmp::Ordering;
    use std::collections::btree_set::{IntoIter, Iter};
    use std::collections::{BTreeMap, BTreeSet, HashSet};
    use std::fmt::{Debug, Formatter};
    use std::sync::Arc;

    /// Node involved in IDKG protocol as a receiver or as a dealer or both.
    /// A node is uniquely identified by its `id`.
    pub struct Node {
        id: NodeId,
        crypto_component: Arc<TempCryptoComponentGeneric<Csp, ReproducibleRng>>,
        logger: InMemoryReplicaLogger,
    }

    impl Node {
        pub fn new<R: Rng + CryptoRng>(
            node_id: NodeId,
            registry: Arc<FakeRegistryClient>,
            rng: &mut R,
        ) -> Self {
            let logger = InMemoryReplicaLogger::new();
            Node {
                id: node_id,
                crypto_component: Arc::new(
                    create_crypto_component_with_sign_mega_and_multisign_keys_in_registry(
                        node_id,
                        registry,
                        ReplicaLogger::from(&logger),
                        rng,
                    ),
                ),
                logger,
            }
        }

        pub fn id(&self) -> NodeId {
            self.id
        }

        pub fn crypto(&self) -> Arc<TempCryptoComponentGeneric<Csp, ReproducibleRng>> {
            Arc::clone(&self.crypto_component)
        }

        pub fn create_dealing_or_panic(&self, params: &IDkgTranscriptParams) -> SignedIDkgDealing {
            self.create_dealing(params).unwrap_or_else(|error| {
                panic!("failed to create IDkg dealing for {:?}: {:?}", self, error)
            })
        }

        pub fn load_transcript_or_panic(&self, transcript: &IDkgTranscript) {
            self.crypto_component
                .load_transcript(transcript)
                .unwrap_or_else(|error| {
                    panic!("failed to load transcript for {:?}: {:?}", self, error)
                });
        }

        pub fn load_input_transcripts(&self, inputs: &ThresholdEcdsaSigInputs) {
            self.load_transcript_or_panic(inputs.presig_quadruple().kappa_unmasked());
            self.load_transcript_or_panic(inputs.presig_quadruple().lambda_masked());
            self.load_transcript_or_panic(inputs.presig_quadruple().kappa_times_lambda());
            self.load_transcript_or_panic(inputs.presig_quadruple().key_times_lambda());
            self.load_transcript_or_panic(inputs.key_transcript());
        }

        pub fn create_transcript_or_panic(
            &self,
            params: &IDkgTranscriptParams,
            dealings: &BatchSignedIDkgDealings,
        ) -> IDkgTranscript {
            self.create_transcript(params, dealings)
                .unwrap_or_else(|error| {
                    panic!("failed to create transcript for {:?}: {:?}", self, error)
                })
        }

        pub fn current_node_public_keys(
            &self,
        ) -> Result<CurrentNodePublicKeys, CurrentNodePublicKeysError> {
            self.crypto_component.current_node_public_keys()
        }

        pub fn drain_logs(self) -> Vec<LogEntry> {
            self.logger.drain_logs()
        }
    }

    impl<T: Signable> BasicSigner<T> for Node {
        fn sign_basic(
            &self,
            message: &T,
            signer: NodeId,
            registry_version: RegistryVersion,
        ) -> CryptoResult<BasicSigOf<T>> {
            self.crypto_component
                .sign_basic(message, signer, registry_version)
        }
    }

    impl IDkgProtocol for Node {
        fn create_dealing(
            &self,
            params: &IDkgTranscriptParams,
        ) -> Result<SignedIDkgDealing, IDkgCreateDealingError> {
            self.crypto_component.create_dealing(params)
        }

        fn verify_dealing_public(
            &self,
            params: &IDkgTranscriptParams,
            signed_dealing: &SignedIDkgDealing,
        ) -> Result<(), IDkgVerifyDealingPublicError> {
            self.crypto_component
                .verify_dealing_public(params, signed_dealing)
        }

        fn verify_dealing_private(
            &self,
            params: &IDkgTranscriptParams,
            signed_dealing: &SignedIDkgDealing,
        ) -> Result<(), IDkgVerifyDealingPrivateError> {
            self.crypto_component
                .verify_dealing_private(params, signed_dealing)
        }

        fn verify_initial_dealings(
            &self,
            params: &IDkgTranscriptParams,
            initial_dealings: &InitialIDkgDealings,
        ) -> Result<(), IDkgVerifyInitialDealingsError> {
            self.crypto_component
                .verify_initial_dealings(params, initial_dealings)
        }

        fn create_transcript(
            &self,
            params: &IDkgTranscriptParams,
            dealings: &BatchSignedIDkgDealings,
        ) -> Result<IDkgTranscript, IDkgCreateTranscriptError> {
            self.crypto_component.create_transcript(params, dealings)
        }

        fn verify_transcript(
            &self,
            params: &IDkgTranscriptParams,
            transcript: &IDkgTranscript,
        ) -> Result<(), IDkgVerifyTranscriptError> {
            self.crypto_component.verify_transcript(params, transcript)
        }

        fn load_transcript(
            &self,
            transcript: &IDkgTranscript,
        ) -> Result<Vec<IDkgComplaint>, IDkgLoadTranscriptError> {
            self.crypto_component.load_transcript(transcript)
        }

        fn verify_complaint(
            &self,
            transcript: &IDkgTranscript,
            complainer_id: NodeId,
            complaint: &IDkgComplaint,
        ) -> Result<(), IDkgVerifyComplaintError> {
            self.crypto_component
                .verify_complaint(transcript, complainer_id, complaint)
        }

        fn open_transcript(
            &self,
            transcript: &IDkgTranscript,
            complainer_id: NodeId,
            complaint: &IDkgComplaint,
        ) -> Result<IDkgOpening, IDkgOpenTranscriptError> {
            self.crypto_component
                .open_transcript(transcript, complainer_id, complaint)
        }

        fn verify_opening(
            &self,
            transcript: &IDkgTranscript,
            opener: NodeId,
            opening: &IDkgOpening,
            complaint: &IDkgComplaint,
        ) -> Result<(), IDkgVerifyOpeningError> {
            self.crypto_component
                .verify_opening(transcript, opener, opening, complaint)
        }

        fn load_transcript_with_openings(
            &self,
            transcript: &IDkgTranscript,
            openings: &BTreeMap<IDkgComplaint, BTreeMap<NodeId, IDkgOpening>>,
        ) -> Result<(), IDkgLoadTranscriptError> {
            self.crypto_component
                .load_transcript_with_openings(transcript, openings)
        }

        fn retain_active_transcripts(
            &self,
            active_transcripts: &HashSet<IDkgTranscript>,
        ) -> Result<(), IDkgRetainKeysError> {
            self.crypto_component
                .retain_active_transcripts(active_transcripts)
        }
    }

    impl ThresholdEcdsaSigner for Node {
        fn sign_share(
            &self,
            inputs: &ThresholdEcdsaSigInputs,
        ) -> Result<ThresholdEcdsaSigShare, ThresholdEcdsaSignShareError> {
            self.crypto_component.sign_share(inputs)
        }
    }

    impl ThresholdEcdsaSigVerifier for Node {
        fn verify_sig_share(
            &self,
            signer: NodeId,
            inputs: &ThresholdEcdsaSigInputs,
            share: &ThresholdEcdsaSigShare,
        ) -> Result<(), ThresholdEcdsaVerifySigShareError> {
            self.crypto_component
                .verify_sig_share(signer, inputs, share)
        }

        fn combine_sig_shares(
            &self,
            inputs: &ThresholdEcdsaSigInputs,
            shares: &BTreeMap<NodeId, ThresholdEcdsaSigShare>,
        ) -> Result<ThresholdEcdsaCombinedSignature, ThresholdEcdsaCombineSigSharesError> {
            self.crypto_component.combine_sig_shares(inputs, shares)
        }

        fn verify_combined_sig(
            &self,
            inputs: &ThresholdEcdsaSigInputs,
            signature: &ThresholdEcdsaCombinedSignature,
        ) -> Result<(), ThresholdEcdsaVerifyCombinedSignatureError> {
            self.crypto_component.verify_combined_sig(inputs, signature)
        }
    }

    impl<T: Signable> BasicSigVerifier<T> for Node {
        fn verify_basic_sig(
            &self,
            signature: &BasicSigOf<T>,
            message: &T,
            signer: NodeId,
            registry_version: RegistryVersion,
        ) -> CryptoResult<()> {
            self.crypto_component
                .verify_basic_sig(signature, message, signer, registry_version)
        }

        fn combine_basic_sig(
            &self,
            signatures: BTreeMap<NodeId, &BasicSigOf<T>>,
            registry_version: RegistryVersion,
        ) -> CryptoResult<BasicSignatureBatch<T>> {
            self.crypto_component
                .combine_basic_sig(signatures, registry_version)
        }

        fn verify_basic_sig_batch(
            &self,
            signature_batch: &BasicSignatureBatch<T>,
            message: &T,
            registry_version: RegistryVersion,
        ) -> CryptoResult<()> {
            self.crypto_component
                .verify_basic_sig_batch(signature_batch, message, registry_version)
        }
    }

    fn create_crypto_component_with_sign_mega_and_multisign_keys_in_registry<R: Rng + CryptoRng>(
        node_id: NodeId,
        registry: Arc<FakeRegistryClient>,
        logger: ReplicaLogger,
        rng: &mut R,
    ) -> TempCryptoComponentGeneric<Csp, ReproducibleRng> {
        TempCryptoComponent::builder()
            .with_registry(Arc::clone(&registry) as Arc<_>)
            .with_node_id(node_id)
            .with_keys(ic_crypto_temp_crypto::NodeKeysToGenerate {
                generate_node_signing_keys: true,
                generate_committee_signing_keys: true,
                generate_dkg_dealing_encryption_keys: false,
                generate_idkg_dealing_encryption_keys: true,
                generate_tls_keys_and_certificate: false,
            })
            .with_logger(logger)
            .with_rng(ReproducibleRng::from_rng(rng))
            .build()
    }

    impl Debug for Node {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Node").field("id", &self.id).finish()
        }
    }

    impl PartialEq for Node {
        fn eq(&self, other: &Self) -> bool {
            self.id.eq(&other.id)
        }
    }

    impl Eq for Node {}

    impl PartialOrd for Node {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            self.id.partial_cmp(&other.id)
        }
    }

    impl Ord for Node {
        fn cmp(&self, other: &Self) -> Ordering {
            self.id.cmp(&other.id)
        }
    }

    #[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Nodes {
        nodes: BTreeSet<Node>,
    }

    impl Nodes {
        pub fn new() -> Self {
            Nodes {
                nodes: BTreeSet::new(),
            }
        }

        pub fn insert(&mut self, node: Node) -> bool {
            self.nodes.insert(node)
        }

        pub fn remove(&mut self, node: &Node) -> bool {
            self.nodes.remove(node)
        }

        pub fn len(&self) -> usize {
            self.nodes.len()
        }

        pub fn is_empty(&self) -> bool {
            self.nodes.is_empty()
        }

        pub fn partition<P>(self, predicate: P) -> (Nodes, Nodes)
        where
            P: Fn((&usize, &Node)) -> bool,
        {
            let mut nodes_true = Nodes::new();
            let mut nodes_false = Nodes::new();
            self.nodes
                .into_iter()
                .enumerate()
                .for_each(|(index, node)| {
                    match predicate((&index, &node)) {
                        true => nodes_true.insert(node),
                        false => nodes_false.insert(node),
                    };
                });
            (nodes_true, nodes_false)
        }

        pub fn into_receivers(
            self,
            idkg_receivers: &IDkgReceivers,
        ) -> impl Iterator<Item = Node> + '_ {
            self.nodes
                .into_iter()
                .filter(|node| idkg_receivers.get().contains(&node.id))
        }

        pub fn receivers<'a, T: AsRef<IDkgReceivers> + 'a>(
            &'a self,
            idkg_receivers: T,
        ) -> impl Iterator<Item = &Node> + 'a {
            self.iter()
                .filter(move |node| idkg_receivers.as_ref().get().contains(&node.id))
        }

        pub fn dealers<'a, T: AsRef<IDkgDealers> + 'a>(
            &'a self,
            idkg_dealers: T,
        ) -> impl Iterator<Item = &Node> + 'a {
            self.iter()
                .filter(move |node| idkg_dealers.as_ref().get().contains(&node.id))
        }

        pub fn random_subset_with_min_size<'a, R: RngCore + CryptoRng>(
            &'a self,
            minimum_size: usize,
            rng: &'a mut R,
        ) -> impl Iterator<Item = &Node> + 'a {
            assert!(
                minimum_size <= self.len(),
                "Requested a random subset with at least {} elements but there are only {} elements",
                minimum_size,
                self.len()
            );
            let subset_size = rng.gen_range(minimum_size..=self.len());
            self.iter().choose_multiple(rng, subset_size).into_iter()
        }

        pub fn into_random_receiver<R: Rng>(
            self,
            idkg_receivers: &IDkgReceivers,
            rng: &mut R,
        ) -> Node {
            self.into_receivers(idkg_receivers)
                .choose(rng)
                .expect("empty receivers")
        }

        pub fn random_receiver<'a, R: Rng>(
            &'a self,
            idkg_receivers: &'a IDkgReceivers,
            rng: &mut R,
        ) -> &Node {
            self.receivers(idkg_receivers)
                .choose(rng)
                .expect("empty receivers")
        }

        pub fn random_receiver_excluding<'a, R: Rng>(
            &'a self,
            exclusion: &Node,
            idkg_receivers: &'a IDkgReceivers,
            rng: &mut R,
        ) -> &Node {
            self.receivers(idkg_receivers)
                .filter(|node| *node != exclusion)
                .choose(rng)
                .expect("empty receivers")
        }

        pub fn random_dealer<'a, R: Rng>(
            &'a self,
            params: &'a IDkgTranscriptParams,
            rng: &mut R,
        ) -> &Node {
            self.dealers(params).choose(rng).expect("empty dealers")
        }

        pub fn random_node<R: Rng>(&self, rng: &mut R) -> &Node {
            self.iter().choose(rng).expect("at least one node")
        }

        pub fn support_dealing_from_all_receivers(
            &self,
            signed_dealing: SignedIDkgDealing,
            params: &IDkgTranscriptParams,
        ) -> BatchSignedIDkgDealing {
            let signature = {
                let mut signatures_map = BTreeMap::new();
                for signer in self.receivers(&params) {
                    let signature = signer
                        .sign_basic(&signed_dealing, signer.id(), params.registry_version())
                        .expect("failed to generate basic-signature");
                    signatures_map.insert(signer.id(), signature);
                }

                BasicSignatureBatch { signatures_map }
            };
            BatchSignedIDkgDealing {
                content: signed_dealing,
                signature,
            }
        }

        pub fn create_dealings(
            &self,
            params: &IDkgTranscriptParams,
        ) -> BTreeMap<NodeId, SignedIDkgDealing> {
            self.dealers(params)
                .map(|dealer| (dealer.id(), dealer.create_dealing_or_panic(params)))
                .collect()
        }

        pub fn support_dealings_from_all_receivers(
            &self,
            signed_dealings: BTreeMap<NodeId, SignedIDkgDealing>,
            params: &IDkgTranscriptParams,
        ) -> BatchSignedIDkgDealings {
            signed_dealings
                .into_values()
                .map(|signed_dealing| {
                    self.support_dealing_from_all_receivers(signed_dealing, params)
                })
                .collect()
        }

        pub fn create_batch_signed_dealings(
            &self,
            params: &IDkgTranscriptParams,
        ) -> BatchSignedIDkgDealings {
            let signed_dealings = self.create_dealings(params);
            self.support_dealings_from_all_receivers(signed_dealings, params)
        }

        pub fn create_and_verify_signed_dealings(
            &self,
            params: &IDkgTranscriptParams,
        ) -> BTreeMap<NodeId, SignedIDkgDealing> {
            self.dealers(params)
                .map(|dealer| {
                    let dealing = self.create_and_verify_signed_dealing(params, dealer);
                    (dealer.id(), dealing)
                })
                .collect()
        }

        pub fn create_and_verify_signed_dealing(
            &self,
            params: &IDkgTranscriptParams,
            dealer: &Node,
        ) -> SignedIDkgDealing {
            let signed_dealing = dealer.create_dealing_or_panic(params);

            // Verify the dealing is publicly valid
            dealer
                .verify_dealing_public(params, &signed_dealing)
                .expect("unexpectedly invalid dealing");

            // Verify the dealing is privately valid for all receivers
            for receiver in self.receivers(&params) {
                receiver
                    .verify_dealing_private(params, &signed_dealing)
                    .expect("unexpectedly invalid dealing (private verification)");
            }

            signed_dealing
        }

        pub fn load_previous_transcripts_and_create_signed_dealing(
            &self,
            params: &IDkgTranscriptParams,
            loader: &Node,
        ) -> SignedIDkgDealing {
            match params.operation_type() {
                IDkgTranscriptOperation::Random => (),
                IDkgTranscriptOperation::ReshareOfMasked(transcript)
                | IDkgTranscriptOperation::ReshareOfUnmasked(transcript) => {
                    loader.load_transcript_or_panic(transcript);
                }
                IDkgTranscriptOperation::UnmaskedTimesMasked(transcript_1, transcript_2) => {
                    loader.load_transcript_or_panic(transcript_1);
                    loader.load_transcript_or_panic(transcript_2);
                }
            }

            self.create_and_verify_signed_dealing(params, loader)
        }

        pub fn load_previous_transcripts_and_create_signed_dealings(
            &self,
            params: &IDkgTranscriptParams,
        ) -> BTreeMap<NodeId, SignedIDkgDealing> {
            self.dealers(params)
                .map(|dealer| {
                    let signed_dealing =
                        self.load_previous_transcripts_and_create_signed_dealing(params, dealer);
                    (dealer.id(), signed_dealing)
                })
                .collect()
        }

        /// Load previous transcripts on each node (if resharing or multiplying),
        /// create all dealings, multi-sign them, and build a transcript from those
        /// multi-signed dealings.
        pub fn run_idkg_and_create_and_verify_transcript<R: RngCore + CryptoRng>(
            &self,
            params: &IDkgTranscriptParams,
            rng: &mut R,
        ) -> IDkgTranscript {
            let dealings = self.load_previous_transcripts_and_create_signed_dealings(params);
            let multisigned_dealings = self.support_dealings_from_all_receivers(dealings, params);
            let transcript_creator = self.dealers(params).next().unwrap();
            let transcript =
                transcript_creator.create_transcript_or_panic(params, &multisigned_dealings);
            assert!(self
                .random_receiver(params.receivers(), rng)
                .verify_transcript(params, &transcript)
                .is_ok());
            transcript
        }

        pub fn ids<B: FromIterator<NodeId>>(&self) -> B {
            self.nodes.iter().map(Node::id).collect()
        }

        pub fn iter(&self) -> Iter<'_, Node> {
            self.nodes.iter()
        }
    }

    impl IDkgParticipantsRandom for Nodes {
        fn choose_dealers_and_receivers<R: RngCore + CryptoRng>(
            &self,
            strategy: &IDkgParticipants,
            rng: &mut R,
        ) -> (IDkgDealers, IDkgReceivers) {
            let into_dealers_and_receivers = |dealer_ids, receiver_ids| {
                (
                    IDkgDealers::new(dealer_ids).expect("valid dealers"),
                    IDkgReceivers::new(receiver_ids).expect("valid receivers"),
                )
            };
            match strategy {
                IDkgParticipants::Random => self.choose_dealers_and_receivers(
                    &IDkgParticipants::RandomWithAtLeast {
                        min_num_dealers: 1,
                        min_num_receivers: 1,
                    },
                    rng,
                ),
                IDkgParticipants::RandomWithAtLeast {
                    min_num_dealers,
                    min_num_receivers,
                } => {
                    assert!(
                        *min_num_dealers > 0,
                        "minimum number of dealers must be positive"
                    );
                    assert!(
                        *min_num_receivers > 0,
                        "minimum number of receivers must be positive"
                    );
                    let dealers_ids = self
                        .random_subset_with_min_size(*min_num_dealers, rng)
                        .map(Node::id)
                        .collect();
                    let receivers_ids = self
                        .random_subset_with_min_size(*min_num_receivers, rng)
                        .map(Node::id)
                        .collect();
                    into_dealers_and_receivers(dealers_ids, receivers_ids)
                }
                IDkgParticipants::RandomForThresholdSignature => {
                    let is_threshold_satisfied = |num_dealers, num_receivers| {
                        let faulty_dealers = get_faults_tolerated(num_dealers);
                        let faulty_receivers = get_faults_tolerated(num_receivers);
                        // see IDkgTranscriptParams::collection_threshold
                        let max_collection_threshold = [
                            faulty_dealers + 1,             //IDkgTranscriptOperation::Random
                            faulty_receivers + 1, //IDkgTranscriptOperation::ReshareOfMasked or ReshareOfUnmasked
                            2 * (faulty_receivers + 1) - 1, // IDkgTranscriptOperation::UnmaskedTimesMasked
                        ];
                        let threshold = faulty_dealers
                            + max_collection_threshold.iter().max().expect("non-empty");
                        num_dealers >= threshold
                    };

                    let min_num_receivers = self.len();
                    let min_num_dealers = (1..=min_num_receivers)
                        .into_iter()
                        .find(|&num_dealers| is_threshold_satisfied(num_dealers, min_num_receivers))
                        .unwrap_or_else(||  panic!("no valid number of dealers found for {min_num_receivers} receivers and given constraint"));

                    self.choose_dealers_and_receivers(
                        &IDkgParticipants::RandomWithAtLeast {
                            min_num_dealers,
                            min_num_receivers,
                        },
                        rng,
                    )
                }
                IDkgParticipants::AllNodesAsDealersAndReceivers => {
                    into_dealers_and_receivers(self.ids(), self.ids())
                }
            }
        }
    }

    impl IntoIterator for Nodes {
        type Item = Node;
        type IntoIter = IntoIter<Node>;

        fn into_iter(self) -> Self::IntoIter {
            self.nodes.into_iter()
        }
    }

    impl<'a> IntoIterator for &'a Nodes {
        type Item = &'a Node;
        type IntoIter = Iter<'a, Node>;

        fn into_iter(self) -> Self::IntoIter {
            self.nodes.iter()
        }
    }

    impl FromIterator<Node> for Nodes {
        fn from_iter<T: IntoIterator<Item = Node>>(iter: T) -> Self {
            let mut nodes = Nodes::new();
            for node in iter {
                nodes.insert(node);
            }
            nodes
        }
    }

    impl Extend<Node> for Nodes {
        fn extend<T: IntoIterator<Item = Node>>(&mut self, iter: T) {
            self.nodes.extend(iter)
        }
    }
}

/// A trait to choose the dealers and receivers for an IDKG protocol run.
pub trait IDkgParticipantsRandom {
    fn choose_dealers_and_receivers<R: RngCore + CryptoRng>(
        &self,
        strategy: &IDkgParticipants,
        rng: &mut R,
    ) -> (IDkgDealers, IDkgReceivers);
}

/// Assembles various strategies for choosing IDkg participants.
pub enum IDkgParticipants {
    /// Choose dealers and receivers randomly:
    /// - Choose a random subset with at least one node to be dealers.
    /// - Choose a random subset with at least one node to be receivers.
    /// Both dealers and receivers are chosen independently of each other and it could be the case
    /// that some nodes are neither dealers nor receivers.
    /// This is equivalent to `RandomWithAtLeast{min_num_dealers: 1, min_num_receivers: 1}`.
    Random,

    /// Choose dealers and receivers randomly:
    /// - Choose a random subset with at least `min_num_dealers` nodes to be dealers.
    /// - Choose a random subset with at least `min_num_receivers` nodes to be receivers.
    /// Both dealers and receivers are chosen independently of each other and it could be the case
    /// that some nodes are neither dealers nor receivers.
    ///
    /// # Panics
    /// - If `min_num_dealers` is zero.
    /// - If `min_num_receivers` is zero.
    RandomWithAtLeast {
        min_num_dealers: usize,
        min_num_receivers: usize,
    },

    /// Choose dealers and receivers randomly such that they can be used for threshold signature,
    /// meaning that the *same* dealers and receivers can be involved in all required `IDkgTranscriptOperation`s.
    ///
    /// This implies the following restrictions on how dealers and receivers can be chosen
    /// (see `IDkgTranscriptParams::new` for details):
    /// - dealers must be a subset of receivers
    /// - there must be sufficiently many dealers and receivers such that the collection threshold is satisfied.
    ///
    /// To simplify the implementation, receivers are chosen to be *all nodes*, while dealers are chosen
    /// to be a random subset of a certain minimum size, such that the collection threshold for the
    /// various needed `IDkgTranscriptOperation`s is always satisfied.
    RandomForThresholdSignature,

    /// Choose all nodes as both dealers and receivers.
    /// No random choice is involved.
    AllNodesAsDealersAndReceivers,
}

pub struct CanisterThresholdSigTestEnvironment {
    pub nodes: Nodes,
    pub registry_data: Arc<ProtoRegistryDataProvider>,
    pub registry: Arc<FakeRegistryClient>,
    pub newest_registry_version: RegistryVersion,
}

impl CanisterThresholdSigTestEnvironment {
    /// Creates a new test environment with the given number of nodes.
    pub fn new<R: RngCore + CryptoRng>(num_of_nodes: usize, rng: &mut R) -> Self {
        let registry_data = Arc::new(ProtoRegistryDataProvider::new());
        let registry = Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));
        let registry_version = random_registry_version(rng);

        let mut env = Self {
            nodes: Nodes::new(),
            registry_data,
            registry: Arc::clone(&registry),
            newest_registry_version: registry_version,
        };

        for node_id in n_random_node_ids(num_of_nodes, rng) {
            let node = Node::new(node_id, Arc::clone(&registry), rng);
            env.add_node(node);
        }
        env.registry.update_to_latest_version();

        env
    }

    /// Returns an `IDkgTranscriptParams` appropriate for creating a random
    /// sharing in this environment.
    pub fn params_for_random_sharing<R: RngCore + CryptoRng>(
        &self,
        dealers: &IDkgDealers,
        receivers: &IDkgReceivers,
        algorithm_id: AlgorithmId,
        rng: &mut R,
    ) -> IDkgTranscriptParams {
        IDkgTranscriptParams::new(
            random_transcript_id(rng),
            dealers.get().clone(),
            receivers.get().clone(),
            self.newest_registry_version,
            algorithm_id,
            IDkgTranscriptOperation::Random,
        )
        .expect("failed to create random IDkgTranscriptParams")
    }

    pub fn choose_dealers_and_receivers<R: RngCore + CryptoRng>(
        &self,
        strategy: &IDkgParticipants,
        rng: &mut R,
    ) -> (IDkgDealers, IDkgReceivers) {
        self.nodes.choose_dealers_and_receivers(strategy, rng)
    }

    fn add_node(&mut self, node: Node) {
        let node_id = node.id();
        let node_keys = node
            .crypto()
            .current_node_public_keys()
            .expect("Failed to retrieve node public keys");
        assert!(self.nodes.insert(node), "failed adding node {:?}", node_id);
        self.registry_data
            .add(
                &make_crypto_node_key(node_id, KeyPurpose::NodeSigning),
                self.newest_registry_version,
                node_keys.node_signing_public_key,
            )
            .expect("can add node signing public key to registry");
        self.registry_data
            .add(
                &make_crypto_node_key(node_id, KeyPurpose::CommitteeSigning),
                self.newest_registry_version,
                node_keys.committee_signing_public_key,
            )
            .expect("can add committee public key to registry");

        self.registry_data
            .add(
                &make_crypto_node_key(node_id, KeyPurpose::IDkgMEGaEncryption),
                self.newest_registry_version,
                node_keys.idkg_dealing_encryption_public_key,
            )
            .expect("can add MEGa public key to registry");
    }
}

pub fn random_receiver_for_inputs<R: RngCore + CryptoRng>(
    inputs: &ThresholdEcdsaSigInputs,
    rng: &mut R,
) -> NodeId {
    *inputs
        .receivers()
        .get()
        .iter()
        .choose(rng)
        .expect("receivers is empty")
}

/// Returns a randomly-generate `NodeId` that is *not* in `exclusions`.
pub fn random_node_id_excluding<R: RngCore + CryptoRng>(
    exclusions: &BTreeSet<NodeId>,
    rng: &mut R,
) -> NodeId {
    *random_node_ids_excluding(exclusions, 1, rng)
        .iter()
        .next()
        .expect("we know this is non-empty")
}

/// Returns `n` randomly-generate `NodeId`s that are *not* in `exclusions`.
pub fn random_node_ids_excluding<R: RngCore + CryptoRng>(
    exclusions: &BTreeSet<NodeId>,
    n: usize,
    rng: &mut R,
) -> BTreeSet<NodeId> {
    let mut node_ids = BTreeSet::new();
    while node_ids.len() < n {
        let candidate = node_id(rng.gen());
        if !exclusions.contains(&candidate) {
            node_ids.insert(candidate);
        }
    }
    assert!(node_ids.is_disjoint(exclusions));
    node_ids
}

pub fn node_id(id: u64) -> NodeId {
    NodeId::from(PrincipalId::new_node_test_id(id))
}

pub fn set_of_nodes(ids: &[u64]) -> BTreeSet<NodeId> {
    let mut nodes = BTreeSet::new();
    for id in ids.iter() {
        nodes.insert(node_id(*id));
    }
    nodes
}

fn random_registry_version<R: RngCore + CryptoRng>(rng: &mut R) -> RegistryVersion {
    RegistryVersion::new(rng.gen_range(1..u32::MAX) as u64)
}

fn random_transcript_id<R: RngCore + CryptoRng>(rng: &mut R) -> IDkgTranscriptId {
    let id = rng.gen::<u64>();
    let subnet = SubnetId::from(PrincipalId::new_subnet_test_id(rng.gen::<u64>()));
    let height = Height::from(rng.gen::<u64>());

    IDkgTranscriptId::new(subnet, id, height)
}

fn n_random_node_ids<R: RngCore + CryptoRng>(n: usize, rng: &mut R) -> BTreeSet<NodeId> {
    let mut node_ids = BTreeSet::new();
    while node_ids.len() < n {
        node_ids.insert(NodeId::from(PrincipalId::new_node_test_id(rng.gen())));
    }
    node_ids
}

pub fn random_receiver_id<R: RngCore + CryptoRng>(
    params: &IDkgTranscriptParams,
    rng: &mut R,
) -> NodeId {
    *random_receiver_id_excluding_set(params.receivers(), &BTreeSet::new(), rng)
        .expect("receivers is empty")
}

pub fn random_receiver_id_excluding<R: RngCore + CryptoRng>(
    receivers: &IDkgReceivers,
    exclusion: NodeId,
    rng: &mut R,
) -> NodeId {
    let mut excluded_receivers = BTreeSet::new();
    excluded_receivers.insert(exclusion);
    *random_receiver_id_excluding_set(receivers, &excluded_receivers, rng)
        .expect("the only possible receiver is excluded")
}

pub fn random_receiver_id_excluding_set<'a, R: CryptoRng + RngCore>(
    receivers: &'a IDkgReceivers,
    excluded_receivers: &'a BTreeSet<NodeId>,
    rng: &mut R,
) -> Option<&'a NodeId> {
    let acceptable_receivers: Vec<_> = receivers.get().difference(excluded_receivers).collect();
    if acceptable_receivers.is_empty() {
        return None;
    }
    Some(acceptable_receivers[rng.gen_range(0..acceptable_receivers.len())])
}

pub fn random_dealer_id<R: RngCore + CryptoRng>(
    params: &IDkgTranscriptParams,
    rng: &mut R,
) -> NodeId {
    *params
        .dealers()
        .get()
        .iter()
        .choose(rng)
        .expect("dealers is empty")
}

pub fn random_dealer_id_excluding<R: RngCore + CryptoRng>(
    transcript: &IDkgTranscript,
    exclusion: NodeId,
    rng: &mut R,
) -> NodeId {
    let excluded_index = transcript
        .index_for_dealer_id(exclusion)
        .expect("excluded node not a dealer");
    let dealer_indexes = transcript
        .verified_dealings
        .keys()
        .cloned()
        .filter(|x| x != &excluded_index)
        .collect::<Vec<u32>>();

    let node_index = dealer_indexes.choose(rng).expect("dealing is empty");
    transcript
        .dealer_id_for_index(*node_index)
        .expect("dealer index not in transcript")
}

pub fn n_random_dealer_indexes<R: RngCore + CryptoRng>(
    transcript: &IDkgTranscript,
    n: usize,
    rng: &mut R,
) -> Vec<NodeIndex> {
    assert!(transcript.verified_dealings.len() >= n);

    transcript
        .verified_dealings
        .keys()
        .cloned()
        .choose_multiple(rng, n)
}

pub fn random_crypto_component_not_in_receivers<R: RngCore + CryptoRng>(
    env: &CanisterThresholdSigTestEnvironment,
    receivers: &IDkgReceivers,
    rng: &mut R,
) -> TempCryptoComponentGeneric<Csp, ReproducibleRng> {
    let node_id = random_node_id_excluding(receivers.get(), rng);
    TempCryptoComponent::builder()
        .with_registry(Arc::clone(&env.registry) as Arc<_>)
        .with_node_id(node_id)
        .with_rng(ReproducibleRng::from_rng(rng))
        .build()
}

/// Corrupts the dealing for a single randomly picked receiver.
/// node_id is the self Node Id. The shares for the receivers specified
/// in excluded_receivers won't be corrupted.
/// The transcript params used to create the dealing is passed in with the
/// dealing to be corrupted.
pub fn corrupt_signed_idkg_dealing<R: CryptoRng + RngCore, T: BasicSigner<IDkgDealing>>(
    idkg_dealing: SignedIDkgDealing,
    transcript_params: &IDkgTranscriptParams,
    basic_signer: &T,
    signer_id: NodeId,
    excluded_receivers: &BTreeSet<NodeId>,
    rng: &mut R,
) -> Result<SignedIDkgDealing, CorruptSignedIDkgDealingError> {
    let receiver =
        random_receiver_id_excluding_set(transcript_params.receivers(), excluded_receivers, rng)
            .ok_or(CorruptSignedIDkgDealingError::NoReceivers)?;
    let node_index = transcript_params.receivers().position(*receiver).unwrap();

    Ok(idkg_dealing
        .into_builder()
        .corrupt_internal_dealing_raw_by_changing_ciphertexts(&[node_index], rng)
        .build_with_signature(transcript_params, basic_signer, signer_id))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CorruptSignedIDkgDealingError {
    SerializationError(String),
    FailedToCorruptDealing(String),
    NoReceivers,
}

pub fn generate_tecdsa_protocol_inputs<R: RngCore + CryptoRng>(
    env: &CanisterThresholdSigTestEnvironment,
    dealers: &IDkgDealers,
    receivers: &IDkgReceivers,
    key_transcript: &IDkgTranscript,
    message_hash: &[u8],
    nonce: Randomness,
    derivation_path: &ExtendedDerivationPath,
    algorithm_id: AlgorithmId,
    rng: &mut R,
) -> ThresholdEcdsaSigInputs {
    let quadruple =
        generate_presig_quadruple(env, dealers, receivers, algorithm_id, key_transcript, rng);

    ThresholdEcdsaSigInputs::new(
        derivation_path,
        message_hash,
        nonce,
        quadruple,
        key_transcript.clone(),
    )
    .expect("failed to create signature inputs")
}

pub fn run_tecdsa_protocol<R: RngCore + CryptoRng + Sync + Send>(
    env: &CanisterThresholdSigTestEnvironment,
    sig_inputs: &ThresholdEcdsaSigInputs,
    rng: &mut R,
) -> ThresholdEcdsaCombinedSignature {
    let sig_shares = sig_share_from_each_receiver(env, sig_inputs);
    // Verify that each signature share can be verified
    let verifier_id = random_node_id_excluding(sig_inputs.receivers().get(), rng);
    let verifier_crypto_component = TempCryptoComponent::builder()
        .with_registry(Arc::clone(&env.registry) as Arc<_>)
        .with_node_id(verifier_id)
        .with_rng(ReproducibleRng::from_rng(rng))
        .build();
    for (signer_id, sig_share) in sig_shares.iter() {
        assert!(verifier_crypto_component
            .verify_sig_share(*signer_id, sig_inputs, sig_share)
            .is_ok());
    }

    let combiner_crypto_component = TempCryptoComponent::builder()
        .with_registry(Arc::clone(&env.registry) as Arc<_>)
        .with_node_id(verifier_id)
        .with_rng(ReproducibleRng::from_rng(rng))
        .build();
    combiner_crypto_component
        .combine_sig_shares(sig_inputs, &sig_shares)
        .expect("Failed to generate signature")
}

pub fn sig_share_from_each_receiver(
    env: &CanisterThresholdSigTestEnvironment,
    inputs: &ThresholdEcdsaSigInputs,
) -> BTreeMap<NodeId, ThresholdEcdsaSigShare> {
    let sig_shares: BTreeMap<_, _> = env
        .nodes
        .receivers(&inputs)
        .map(|receiver| {
            receiver.load_input_transcripts(inputs);
            let sig_share = receiver
                .sign_share(inputs)
                .expect("failed to create sig share");
            (receiver.id(), sig_share)
        })
        .collect();
    sig_shares
}

/// Corrupts valid instances of a given type containing some binary data
/// (e.g., signatures) for testing purposes.
///
/// Some types that we want to corrupt are immutable and that's the reason why this trait does
/// not mutate its parameter but rather produces a new instance of the given type.
pub trait CorruptBytes {
    /// The type to corrupt.
    type Type;

    /// Produces a new instance where a bit was flipped.
    /// Which bit was flipped is an implementation's detail.
    /// The produced instance is *guaranteed* to be different from the original one.
    ///
    /// # Panics
    /// If a bit cannot be flipped (e.g., when the binary data contained by the instance is empty)
    /// ```should_panic
    /// # use ic_crypto_test_utils_canister_threshold_sigs::CorruptBytes;
    /// # use ic_types::crypto::BasicSig;
    /// BasicSig(vec![]).clone_with_bit_flipped();
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// # use ic_crypto_test_utils_canister_threshold_sigs::CorruptBytes;
    /// # use ic_types::crypto::BasicSig;
    /// let original_signature=BasicSig(vec![0b00000000]);
    /// let corrupted_signature=original_signature.clone_with_bit_flipped();
    ///
    /// assert_ne!(original_signature, corrupted_signature);
    /// assert_eq!(corrupted_signature, BasicSig(vec![0b00000001]))
    /// ```
    ///
    fn clone_with_bit_flipped(&self) -> Self::Type;
}

impl<T> CorruptBytes for BasicSignature<T> {
    type Type = Self;

    fn clone_with_bit_flipped(&self) -> Self {
        let corrupted_signature = self.signature.clone_with_bit_flipped();
        BasicSignature {
            signature: corrupted_signature,
            signer: self.signer,
        }
    }
}

impl<T> CorruptBytes for BasicSigOf<T> {
    type Type = Self;

    fn clone_with_bit_flipped(&self) -> Self {
        let corrupted_signature = self.get_ref().clone_with_bit_flipped();
        BasicSigOf::new(corrupted_signature)
    }
}

impl CorruptBytes for BasicSig {
    type Type = Self;

    fn clone_with_bit_flipped(&self) -> Self {
        BasicSig(self.0.clone_with_bit_flipped())
    }
}

impl CorruptBytes for Vec<u8> {
    type Type = Self;

    fn clone_with_bit_flipped(&self) -> Self {
        assert!(!self.is_empty(), "cannot flip bit in empty vector");
        let mut bytes = self.clone();
        *bytes.last_mut().expect("cannot be empty") ^= 1;
        bytes
    }
}

impl<const N: usize> CorruptBytes for [u8; N] {
    type Type = Self;

    fn clone_with_bit_flipped(&self) -> Self::Type {
        let mut bytes = *self;
        bytes[N - 1] ^= 1;
        bytes
    }
}

impl CorruptBytes for ThresholdEcdsaSigShare {
    type Type = Self;

    fn clone_with_bit_flipped(&self) -> Self::Type {
        ThresholdEcdsaSigShare {
            sig_share_raw: self.sig_share_raw.clone_with_bit_flipped(),
        }
    }
}

impl CorruptBytes for ThresholdEcdsaCombinedSignature {
    type Type = Self;

    fn clone_with_bit_flipped(&self) -> Self::Type {
        ThresholdEcdsaCombinedSignature {
            signature: self.signature.clone_with_bit_flipped(),
        }
    }
}

impl CorruptBytes for Randomness {
    type Type = Self;

    fn clone_with_bit_flipped(&self) -> Self::Type {
        Randomness::from(self.get().clone_with_bit_flipped())
    }
}

/// Corrupts a collection of elements containing each some binary data.
///
/// A single element in the collection can be corrupted or all of them.
pub trait CorruptBytesCollection {
    /// Mutates a collection of elements, each containing some binary data,
    /// by flipping a bit in the data held by a *single* element.
    /// Which element in the collection was modified or which bit in the modified element
    /// was flipped is an implementation's detail.
    ///
    /// # Panics
    /// If the collection is empty
    /// ```should_panic
    /// # use std::collections::BTreeMap;
    /// # use ic_crypto_test_utils_canister_threshold_sigs::CorruptBytesCollection;
    /// # use ic_types::signature::BasicSignatureBatch;
    /// let mut signatures: BasicSignatureBatch<String> = BasicSignatureBatch {signatures_map: BTreeMap::new()};
    /// signatures.flip_a_bit_in_one();
    /// ```
    ///
    /// # Examples
    /// ```
    /// # use ic_crypto_test_utils_canister_threshold_sigs::CorruptBytesCollection;
    /// # use ic_types::{NodeId, PrincipalId};
    /// # use ic_types::crypto::{BasicSig, BasicSigOf};
    /// # use ic_types::signature::BasicSignatureBatch;
    /// let  node_id1 = NodeId::from(PrincipalId::new_node_test_id(1));
    /// let  node_id2 = NodeId::from(PrincipalId::new_node_test_id(2));
    /// let mut signatures: BasicSignatureBatch<String> = BasicSignatureBatch {
    ///     signatures_map: vec![
    ///         (node_id1, BasicSigOf::new(BasicSig(vec![0b00000000]))),
    ///         (node_id2, BasicSigOf::new(BasicSig(vec![0b11111111]))),
    ///     ]
    ///     .drain(..)
    ///     .collect(),
    /// };
    ///
    /// signatures.flip_a_bit_in_one();
    ///
    /// assert_eq!(signatures.signatures_map[&node_id1].get_ref(), &BasicSig(vec![0b00000001]));
    /// assert_eq!(signatures.signatures_map[&node_id2].get_ref(), &BasicSig(vec![0b11111111]));
    /// ```
    fn flip_a_bit_in_one(&mut self);

    /// Mutates a collection of elements, each containing some binary data,
    /// by flipping a bit in the data held by *every* element.
    /// Which bit was flipped is an implementation's detail.
    ///
    /// # Panics
    /// If the collection is empty
    /// ```should_panic
    /// # use std::collections::BTreeMap;
    /// # use ic_crypto_test_utils_canister_threshold_sigs::CorruptBytesCollection;
    /// # use ic_types::signature::BasicSignatureBatch;
    /// let mut signatures: BasicSignatureBatch<String> = BasicSignatureBatch {signatures_map: BTreeMap::new()};
    /// signatures.flip_a_bit_in_all();
    /// ```
    ///
    /// # Examples
    /// ```
    /// # use ic_crypto_test_utils_canister_threshold_sigs::CorruptBytesCollection;
    /// # use ic_types::{NodeId, PrincipalId};
    /// # use ic_types::crypto::{BasicSig, BasicSigOf};
    /// # use ic_types::signature::BasicSignatureBatch;
    /// let  node_id1 = NodeId::from(PrincipalId::new_node_test_id(1));
    /// let  node_id2 = NodeId::from(PrincipalId::new_node_test_id(2));
    /// let mut signatures: BasicSignatureBatch<String> = BasicSignatureBatch {
    ///     signatures_map: vec![
    ///         (node_id1, BasicSigOf::new(BasicSig(vec![0b00000000]))),
    ///         (node_id2, BasicSigOf::new(BasicSig(vec![0b11111111]))),
    ///     ]
    ///     .drain(..)
    ///     .collect(),
    /// };
    ///
    /// signatures.flip_a_bit_in_all();
    ///
    /// assert_eq!(signatures.signatures_map[&node_id1].get_ref(), &BasicSig(vec![0b00000001]));
    /// assert_eq!(signatures.signatures_map[&node_id2].get_ref(), &BasicSig(vec![0b11111110]));
    /// ```
    fn flip_a_bit_in_all(&mut self);
}

impl<T> CorruptBytesCollection for BasicSignatureBatch<T> {
    fn flip_a_bit_in_one(&mut self) {
        let signature = self
            .signatures_map
            .values_mut()
            .next()
            .expect("cannot flip a bit of a signature in an empty collection");
        *signature = signature.clone_with_bit_flipped();
    }

    fn flip_a_bit_in_all(&mut self) {
        assert!(
            !self.signatures_map.is_empty(),
            "cannot flip a bit of a signature in an empty collection"
        );
        self.signatures_map
            .values_mut()
            .for_each(|signature| *signature = signature.clone_with_bit_flipped());
    }
}

impl<T> CorruptBytesCollection for Signed<T, BasicSignatureBatch<T>> {
    fn flip_a_bit_in_one(&mut self) {
        self.signature.flip_a_bit_in_one();
    }

    fn flip_a_bit_in_all(&mut self) {
        self.signature.flip_a_bit_in_all();
    }
}

pub trait IntoBuilder {
    type BuilderType;
    fn into_builder(self) -> Self::BuilderType;
}

pub struct IDkgComplaintBuilder {
    transcript_id: IDkgTranscriptId,
    dealer_id: NodeId,
    internal_complaint_raw: Vec<u8>,
}

impl IDkgComplaintBuilder {
    pub fn build(self) -> IDkgComplaint {
        IDkgComplaint {
            transcript_id: self.transcript_id,
            dealer_id: self.dealer_id,
            internal_complaint_raw: self.internal_complaint_raw,
        }
    }

    pub fn with_transcript_id(mut self, new_transcript_id: IDkgTranscriptId) -> Self {
        self.transcript_id = new_transcript_id;
        self
    }

    pub fn with_dealer_id(mut self, new_dealer_id: NodeId) -> Self {
        self.dealer_id = new_dealer_id;
        self
    }
}

impl IntoBuilder for IDkgComplaint {
    type BuilderType = IDkgComplaintBuilder;

    fn into_builder(self) -> Self::BuilderType {
        IDkgComplaintBuilder {
            transcript_id: self.transcript_id,
            dealer_id: self.dealer_id,
            internal_complaint_raw: self.internal_complaint_raw,
        }
    }
}

pub struct SignedIDkgDealingBuilder {
    content: IDkgDealing,
    signature: BasicSignature<IDkgDealing>,
}

impl SignedIDkgDealingBuilder {
    pub fn build(self) -> SignedIDkgDealing {
        Signed {
            content: self.content,
            signature: self.signature,
        }
    }
    pub fn corrupt_transcript_id(mut self) -> Self {
        self.content = IDkgDealing {
            transcript_id: self.content.transcript_id.increment(),
            ..self.content
        };
        self
    }

    pub fn build_with_signature<T: BasicSigner<IDkgDealing>>(
        mut self,
        params: &IDkgTranscriptParams,
        basic_signer: &T,
        signer_id: NodeId,
    ) -> SignedIDkgDealing {
        self.signature = BasicSignature {
            signature: basic_signer
                .sign_basic(&self.content, signer_id, params.registry_version())
                .expect("Failed to sign a dealing"),
            signer: signer_id,
        };
        self.build()
    }

    pub fn corrupt_signature(mut self) -> Self {
        self.signature = self.signature.clone_with_bit_flipped();
        self
    }

    pub fn with_dealer_id(mut self, dealer_id: NodeId) -> Self {
        self.signature = BasicSignature {
            signer: dealer_id,
            ..self.signature
        };
        self
    }

    pub fn corrupt_internal_dealing_raw_by_flipping_bit(mut self) -> Self {
        self.content = IDkgDealing {
            internal_dealing_raw: self.content.internal_dealing_raw.clone_with_bit_flipped(),
            ..self.content
        };
        self
    }

    pub fn corrupt_internal_dealing_raw_by_changing_ciphertexts<R: CryptoRng + RngCore>(
        mut self,
        corruption_targets: &[NodeIndex],
        rng: &mut R,
    ) -> Self {
        let internal_dealing = IDkgDealingInternal::deserialize(&self.content.internal_dealing_raw)
            .expect("error deserializing iDKG dealing internal");
        let corrupted_dealing =
            corrupt_dealing(&internal_dealing, corruption_targets, Seed::from_rng(rng))
                .expect("error corrupting dealing");
        self.content = IDkgDealing {
            internal_dealing_raw: corrupted_dealing
                .serialize()
                .expect("error serializing corrupted dealing"),
            ..self.content
        };
        self
    }
}

impl IntoBuilder for SignedIDkgDealing {
    type BuilderType = SignedIDkgDealingBuilder;

    fn into_builder(self) -> Self::BuilderType {
        SignedIDkgDealingBuilder {
            content: self.content,
            signature: self.signature,
        }
    }
}

pub struct ThresholdEcdsaSigInputsBuilder {
    derivation_path: ExtendedDerivationPath,
    hashed_message: Vec<u8>,
    nonce: Randomness,
    presig_quadruple: PreSignatureQuadruple,
    key_transcript: IDkgTranscript,
}

impl ThresholdEcdsaSigInputsBuilder {
    pub fn build(self) -> ThresholdEcdsaSigInputs {
        ThresholdEcdsaSigInputs::new(
            &self.derivation_path,
            &self.hashed_message,
            self.nonce,
            self.presig_quadruple,
            self.key_transcript,
        )
        .expect("invalid threshold ECDSA sig inputs")
    }

    pub fn corrupt_hashed_message(mut self) -> Self {
        self.hashed_message = self.hashed_message.clone_with_bit_flipped();
        self
    }

    pub fn corrupt_nonce(mut self) -> Self {
        self.nonce = self.nonce.clone_with_bit_flipped();
        self
    }
}

impl IntoBuilder for ThresholdEcdsaSigInputs {
    type BuilderType = ThresholdEcdsaSigInputsBuilder;

    fn into_builder(self) -> Self::BuilderType {
        ThresholdEcdsaSigInputsBuilder {
            derivation_path: self.derivation_path().clone(),
            hashed_message: Vec::from(self.hashed_message()),
            nonce: *self.nonce(),
            presig_quadruple: self.presig_quadruple().clone(),
            key_transcript: self.key_transcript().clone(),
        }
    }
}
