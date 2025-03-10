//! # Signature API
//! The signature API contains basic, multi, and threshold signatures.
//!
//! # Threshold Signatures
//! Threshold signatures require the contribution of at least `t` out of `n`
//! participants to generate a valid signature. The key material necessary to
//! compute and verify threshold signatures is generated by running a
//! distributed key generation protocol (DKG). In particular,
//! `DkgAlgorithm::load_transcript` loads the Crypto component with the data
//! required for threshold signing and threshold signature verification.
//!
//! The `ThresholdSigner`'s sign method computes an individual threshold
//! signature share.
//!
//! The `ThresholdSigVerifier`'s methods perform the following operations on
//! signatures:
//! * Verify an individual signature share using `verify_threshold_sig_share`.
//! * Combine individual signature shares into a combined threshold signature
//!   using `combine_threshold_sig_shares`.
//! * Verify a combined threshold signature using
//!   `verify_threshold_sig_combined`.
//!
//! Please refer to the trait documentation for details.

use ic_types::crypto::{
    BasicSigOf, CanisterSigOf, CombinedMultiSigOf, CryptoResult, IndividualMultiSigOf, Signable,
    UserPublicKey,
};
use ic_types::messages::{Delegation, MessageId, WebAuthnEnvelope};
use ic_types::signature::BasicSignatureBatch;
use ic_types::{NodeId, RegistryVersion};
use std::collections::{BTreeMap, BTreeSet};

pub mod threshold_sig;

pub use threshold_sig::{ThresholdSigVerifier, ThresholdSigVerifierByPublicKey, ThresholdSigner};

pub mod canister_threshold_sig;

/// A Crypto Component interface to create basic signatures.
///
/// Although the exact underlying signature scheme is unspecified and
/// potentially subject to change, it is guaranteed to be non-malleable,
/// that is, strongly unforgeable under chosen-message attack.
pub trait BasicSigner<T: Signable> {
    /// Creates a (non-malleable) basic signature.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if the `signer`'s public key cannot
    ///   be found at the given `registry_version`.
    /// * `CryptoError::MalformedPublicKey`: if the `signer`'s public key
    ///   obtained from the registry is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the `signer`'s public key
    ///   obtained from the registry is for an unsupported algorithm.
    /// * `CryptoError::SecretKeyNotFound`: if the `signer`'s secret key cannot
    ///   be found in the secret key store.
    /// * `CryptoError::MalformedSecretKey`: if the secret key is malformed.
    /// * `CryptoError::InvalidArgument`: if the signature algorithm is not
    ///   supported.
    ///
    /// When called within a Tokio runtime the function should be wrapped inside
    /// 'tokio::task::spawn_blocking' when in async function or
    /// 'tokio::task::block_in_place' when in sync function (using 'block_in_place'
    /// should be very rare event). Otherwise the call panics because the
    /// implementation of 'sign_basic' calls 'tokio::runtime::Runtime.block_on'.
    fn sign_basic(
        &self,
        message: &T,
        signer: NodeId,
        registry_version: RegistryVersion,
    ) -> CryptoResult<BasicSigOf<T>>;
}

/// A Crypto Component interface to verify basic signatures.
pub trait BasicSigVerifier<T: Signable> {
    /// Verifies a basic signature.
    ///
    /// Although the exact underlying signature scheme is unspecified and
    /// potentially subject to change, it is guaranteed to be non-malleable,
    /// that is, strongly unforgeable under chosen-message attack.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if the `signer`'s public key cannot
    ///   be found at the given `registry_version`.
    /// * `CryptoError::MalformedSignature`: if the signature is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the signature algorithm is
    ///   not supported, or if the `signer`'s public key obtained from the
    ///   registry is for an unsupported algorithm.
    /// * `CryptoError::MalformedPublicKey`: if the `signer`'s public key
    ///   obtained from the registry is malformed.
    /// * `CryptoError::SignatureVerification`: if the `signature` could not be
    ///   verified.
    fn verify_basic_sig(
        &self,
        signature: &BasicSigOf<T>,
        message: &T,
        signer: NodeId,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;

    /// Combines individual basic signatures.
    ///
    /// The registry version is not needed for the cryptographic scheme we use
    /// currently/initially. Yet it is a parameter so that we can switch to
    /// other schemes without affecting the API.
    ///
    /// Note that the resulting basic signature batch will only be valid if all the
    /// individual signatures are valid, i.e. `verify_basic_sig`
    /// returned `Ok`.
    ///
    /// the individual basic signatures passed as `signatures` must have been
    ///   verified using `verify_basic_sig`.
    ///
    /// # Errors
    /// * `CryptoError::InvalidArgument`: if `signatures` is empty.
    fn combine_basic_sig(
        &self,
        signatures: BTreeMap<NodeId, &BasicSigOf<T>>,
        registry_version: RegistryVersion,
    ) -> CryptoResult<BasicSignatureBatch<T>>;

    /// Verifies a basic signature batch.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if at least one of the signer's public key cannot
    ///   be found at the given `registry_version`.
    /// * `CryptoError::MalformedSignature`: if the signature is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the signature algorithm is
    ///   not supported, or if one of the signer's public key obtained from the
    ///   registry is for an unsupported algorithm.
    /// * `CryptoError::MalformedPublicKey`: if the signer's public key
    ///   obtained from the registry is malformed.
    /// * `CryptoError::SignatureVerification`: if at least one signature in `signature_batch` could not be
    ///   verified for the given message.
    /// * `CryptoError::InvalidArgument`: if `signature` does not contain any signatures.
    fn verify_basic_sig_batch(
        &self,
        signature_batch: &BasicSignatureBatch<T>,
        message: &T,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;
}

/// A Crypto Component interface to verify basic signatures by public key.
pub trait BasicSigVerifierByPublicKey<T: Signable> {
    /// Verifies a basic signature using the given `public_key`.
    ///
    /// # Errors
    /// * `CryptoError::MalformedPublicKey`: if the `public_key` is malformed.
    /// * `CryptoError::MalformedSignature`: if the `signature` is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the signature algorithm is
    ///   not supported, or if the `public_key` is for an unsupported algorithm.
    /// * `CryptoError::SignatureVerification`: if the `signature` could not be
    ///   verified.
    fn verify_basic_sig_by_public_key(
        &self,
        signature: &BasicSigOf<T>,
        signed_bytes: &T,
        public_key: &UserPublicKey,
    ) -> CryptoResult<()>;
}

/// A Crypto Component interface to verify (ICCSA) canister signatures.
pub trait CanisterSigVerifier<T: Signable> {
    /// Verifies an ICCSA canister signature.
    ///
    /// # Errors
    /// * `CryptoError::AlgorithmNotSupported`: if the signature algorithm is
    ///   not supported for canister signatures.
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::RootSubnetPublicKeyNotFound`: if the root subnet id or
    ///   the root subnet threshold signing public key cannot be found in the
    ///   registry at `registry_version`.
    /// * `CryptoError::MalformedPublicKey`: if the root subnet's threshold
    ///   signing public key is malformed.
    /// * `CryptoError::MalformedSignature`: if the `signature` is malformed.
    /// * `CryptoError::SignatureVerification`: if the `signature` could not be
    ///   verified.
    fn verify_canister_sig(
        &self,
        signature: &CanisterSigOf<T>,
        signed_bytes: &T,
        public_key: &UserPublicKey,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;
}

/// A Crypto Component interface to verify ingress messages.
pub trait IngressSigVerifier:
    BasicSigVerifierByPublicKey<WebAuthnEnvelope>
    + BasicSigVerifierByPublicKey<MessageId>
    + BasicSigVerifierByPublicKey<Delegation>
    + CanisterSigVerifier<Delegation>
    + CanisterSigVerifier<MessageId>
{
}

impl<T> IngressSigVerifier for T where
    T: BasicSigVerifierByPublicKey<WebAuthnEnvelope>
        + BasicSigVerifierByPublicKey<MessageId>
        + BasicSigVerifierByPublicKey<Delegation>
        + CanisterSigVerifier<Delegation>
        + CanisterSigVerifier<MessageId>
{
}

/// A Crypto Component interface to create multi-signatures.
pub trait MultiSigner<T: Signable> {
    /// Creates an individual multi-signature.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if the public key cannot be found at
    ///   the given `registry_version`.
    /// * `CryptoError::MalformedPublicKey`: if the public key obtained from the
    ///   registry is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the public key obtained from
    ///   the registry is for an unsupported algorithm.
    /// * `CryptoError::SecretKeyNotFound`: if the signing key cannot be found
    ///   in the secret key store.
    /// * `CryptoError::MalformedSecretKey`: if the secret key is malformed.
    fn sign_multi(
        &self,
        message: &T,
        signer: NodeId,
        registry_version: RegistryVersion,
    ) -> CryptoResult<IndividualMultiSigOf<T>>;
}

/// A Crypto Component interface to verify and combine multi-signatures.
pub trait MultiSigVerifier<T: Signable> {
    /// Verifies an individual multi-signature.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if the public key cannot be found at
    ///   the given `registry_version`.
    /// * `CryptoError::MalformedSignature`: if the mutli-signature is
    ///   malformed.
    /// * `CryptoError::MalformedPublicKey`: if the public key obtained from the
    ///   registry is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if the public key obtained from
    ///   the registry is for an unsupported algorithm.
    /// * `CryptoError::SignatureVerification`: if the individual
    ///   multi-signature could not be verified.
    fn verify_multi_sig_individual(
        &self,
        signature: &IndividualMultiSigOf<T>,
        message: &T,
        signer: NodeId,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;

    /// Combines individual multi-signature shares.
    ///
    /// The registry version is not needed for the cryptographic scheme we use
    /// currently/initially. Yet it is a parameter so that we can switch to
    /// other schemes without affecting the API.
    ///
    /// Note that the resulting combined signature will only be valid if all the
    /// individual signatures are valid, i.e. `verify_multi_sig_individual`
    /// returned `Ok`.
    ///
    /// the individual multi-signatures passed as `signatures` must have been
    ///   verified using `verify_multi_sig_individual`.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if any of the public keys for the
    ///   signatures cannot be found at the given `registry_version`.
    /// * `CryptoError::MalformedSignature`: if any of the mutli-signatures is
    ///   malformed.
    /// * `CryptoError::MalformedPublicKey`: if any of the public keys obtained
    ///   from the registry is malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if any of the public keys
    ///   obtained from the registry or a signature is for an unsupported
    ///   algorithm.
    /// * `CryptoError::InvalidArgument`: if `signatures` is empty.
    fn combine_multi_sig_individuals(
        &self,
        signatures: BTreeMap<NodeId, IndividualMultiSigOf<T>>,
        registry_version: RegistryVersion,
    ) -> CryptoResult<CombinedMultiSigOf<T>>;

    /// Verifies a combined multi-signature.
    ///
    /// # Errors
    /// * `CryptoError::RegistryClient`: if the registry cannot be accessed at
    ///   `registry_version`.
    /// * `CryptoError::PublicKeyNotFound`: if any of the public keys for the
    ///   'signers' cannot be found at the given `registry_version`.
    /// * `CryptoError::MalformedPublicKey`: if any of the public keys obtained
    ///   from the registry is malformed.
    /// * `CryptoError::MalformedSignature`: if the combined `signature` is
    ///   malformed.
    /// * `CryptoError::AlgorithmNotSupported`: if any of the public keys
    ///   obtained from the registry or the combined signature is for an
    ///   unsupported algorithm. obtained from the registry or the combined
    ///   signature is for an unsupported algorithm.
    /// * `CryptoError::SignatureVerification`: if the combined multi-signature
    ///   could not be verified.
    /// * `CryptoError::InvalidArgument`: if `signers` is empty.
    fn verify_multi_sig_combined(
        &self,
        signature: &CombinedMultiSigOf<T>,
        message: &T,
        signers: BTreeSet<NodeId>,
        registry_version: RegistryVersion,
    ) -> CryptoResult<()>;
}
