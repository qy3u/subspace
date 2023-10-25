use crate::fraud_proof::{
    ExtrinsicDigest, InvalidBundlesFraudProof, InvalidExtrinsicsRootProof,
    InvalidStateTransitionProof, ProofDataPerInvalidBundleType, ValidBundleProof,
    VerificationError,
};
use crate::fraud_proof_runtime_interface::get_fraud_proof_verification_info;
use crate::{
    fraud_proof_runtime_interface, FraudProofVerificationInfoRequest,
    FraudProofVerificationInfoResponse, SetCodeExtrinsic,
};
use codec::{Decode, Encode};
use hash_db::Hasher;
use sp_core::storage::StorageKey;
use sp_core::H256;
use sp_domains::extrinsics::{deduplicate_and_shuffle_extrinsics, extrinsics_shuffling_seed};
use sp_domains::proof_provider_and_verifier::StorageProofVerifier;
use sp_domains::valued_trie::valued_ordered_trie_root;
use sp_domains::{
    BundleValidity, ExecutionReceipt, HeaderHashFor, HeaderHashingFor, HeaderNumberFor,
    InboxedBundle, InvalidBundleType,
};
use sp_runtime::generic::Digest;
use sp_runtime::traits::{BlakeTwo256, Block as BlockT, Hash, Header as HeaderT, NumberFor};
use sp_std::vec::Vec;
use sp_trie::{LayoutV1, StorageProof};
use subspace_core_primitives::Randomness;
use trie_db::node::Value;

/// Verifies invalid domain extrinsic root fraud proof.
pub fn verify_invalid_domain_extrinsics_root_fraud_proof<CBlock, Balance, Hashing, DomainHeader>(
    bad_receipt: ExecutionReceipt<
        NumberFor<CBlock>,
        CBlock::Hash,
        HeaderNumberFor<DomainHeader>,
        HeaderHashFor<DomainHeader>,
        Balance,
    >,
    fraud_proof: &InvalidExtrinsicsRootProof<HeaderHashFor<DomainHeader>>,
) -> Result<(), VerificationError>
where
    CBlock: BlockT,
    Hashing: Hasher<Out = CBlock::Hash>,
    DomainHeader: HeaderT,
    DomainHeader::Hash: Into<H256> + PartialEq + Copy,
{
    let InvalidExtrinsicsRootProof {
        valid_bundle_digests,
        domain_id,
        ..
    } = fraud_proof;

    let consensus_block_hash = bad_receipt.consensus_block_hash;
    let block_randomness = get_fraud_proof_verification_info(
        H256::from_slice(consensus_block_hash.as_ref()),
        FraudProofVerificationInfoRequest::BlockRandomness,
    )
    .and_then(|resp| resp.into_block_randomness())
    .ok_or(VerificationError::FailedToGetBlockRandomness)?;

    let domain_timestamp_extrinsic = get_fraud_proof_verification_info(
        H256::from_slice(consensus_block_hash.as_ref()),
        FraudProofVerificationInfoRequest::DomainTimestampExtrinsic(*domain_id),
    )
    .and_then(|resp| resp.into_domain_timestamp_extrinsic())
    .ok_or(VerificationError::FailedToDeriveDomainTimestampExtrinsic)?;

    let maybe_domain_set_code_extrinsic = get_fraud_proof_verification_info(
        H256::from_slice(consensus_block_hash.as_ref()),
        FraudProofVerificationInfoRequest::DomainSetCodeExtrinsic(*domain_id),
    )
    .map(|resp| resp.into_domain_set_code_extrinsic())
    .ok_or(VerificationError::FailedToDeriveDomainSetCodeExtrinsic)?;

    let mut bundle_extrinsics_digests = Vec::new();
    for (bad_receipt_valid_bundle_digest, bundle_digest) in bad_receipt
        .valid_bundle_digests()
        .into_iter()
        .zip(valid_bundle_digests)
    {
        let bundle_digest_hash =
            HeaderHashingFor::<DomainHeader>::hash_of(&bundle_digest.bundle_digest);
        if bundle_digest_hash != bad_receipt_valid_bundle_digest {
            return Err(VerificationError::InvalidBundleDigest);
        }

        bundle_extrinsics_digests.extend(bundle_digest.bundle_digest.clone());
    }

    let shuffling_seed =
        H256::from_slice(extrinsics_shuffling_seed::<Hashing>(block_randomness).as_ref());

    let mut ordered_extrinsics = deduplicate_and_shuffle_extrinsics(
        bundle_extrinsics_digests,
        Randomness::from(shuffling_seed.to_fixed_bytes()),
    );

    if let SetCodeExtrinsic::EncodedExtrinsic(domain_set_code_extrinsic) =
        maybe_domain_set_code_extrinsic
    {
        let domain_set_code_extrinsic = ExtrinsicDigest::new::<
            LayoutV1<HeaderHashingFor<DomainHeader>>,
        >(domain_set_code_extrinsic);
        ordered_extrinsics.push_front(domain_set_code_extrinsic);
    }

    let timestamp_extrinsic = ExtrinsicDigest::new::<LayoutV1<HeaderHashingFor<DomainHeader>>>(
        domain_timestamp_extrinsic,
    );
    ordered_extrinsics.push_front(timestamp_extrinsic);

    let ordered_trie_node_values = ordered_extrinsics
        .iter()
        .map(|ext_digest| match ext_digest {
            ExtrinsicDigest::Data(data) => Value::Inline(data),
            ExtrinsicDigest::Hash(hash) => Value::Node(hash.0.as_slice()),
        })
        .collect();

    let extrinsics_root = valued_ordered_trie_root::<LayoutV1<HeaderHashingFor<DomainHeader>>>(
        ordered_trie_node_values,
    );
    if bad_receipt.domain_block_extrinsic_root == extrinsics_root {
        return Err(VerificationError::InvalidProof);
    }

    Ok(())
}

/// Verifies valid bundle fraud proof.
pub fn verify_valid_bundle_fraud_proof<CBlock, DomainNumber, DomainHash, Balance>(
    bad_receipt: ExecutionReceipt<
        NumberFor<CBlock>,
        CBlock::Hash,
        DomainNumber,
        DomainHash,
        Balance,
    >,
    fraud_proof: &ValidBundleProof<DomainHash>,
) -> Result<(), VerificationError>
where
    CBlock: BlockT,
    CBlock::Hash: Into<H256>,
    DomainHash: Copy + Into<H256>,
{
    let ValidBundleProof {
        domain_id,
        bundle_index,
        ..
    } = fraud_proof;

    let bundle_body = fraud_proof_runtime_interface::get_fraud_proof_verification_info(
        bad_receipt.consensus_block_hash.into(),
        FraudProofVerificationInfoRequest::DomainBundleBody {
            domain_id: *domain_id,
            bundle_index: *bundle_index,
        },
    )
    .and_then(FraudProofVerificationInfoResponse::into_bundle_body)
    .ok_or(VerificationError::FailedToGetDomainBundleBody)?;

    let valid_bundle_digest = fraud_proof_runtime_interface::derive_bundle_digest(
        bad_receipt.consensus_block_hash.into(),
        *domain_id,
        bundle_body,
    )
    .ok_or(VerificationError::FailedToDeriveBundleDigest)?;

    let bad_valid_bundle_digest = bad_receipt
        .valid_bundle_digest_at(*bundle_index as usize)
        .ok_or(VerificationError::TargetValidBundleNotFound)?;

    if bad_valid_bundle_digest.into() == valid_bundle_digest {
        Err(VerificationError::InvalidProof)
    } else {
        Ok(())
    }
}

/// Verifies invalid state transition fraud proof.
pub fn verify_invalid_state_transition_fraud_proof<CBlock, DomainHeader, Balance>(
    bad_receipt: ExecutionReceipt<
        NumberFor<CBlock>,
        <CBlock as BlockT>::Hash,
        DomainHeader::Number,
        DomainHeader::Hash,
        Balance,
    >,
    bad_receipt_parent: ExecutionReceipt<
        NumberFor<CBlock>,
        <CBlock as BlockT>::Hash,
        DomainHeader::Number,
        DomainHeader::Hash,
        Balance,
    >,
    fraud_proof: &InvalidStateTransitionProof<HeaderHashFor<DomainHeader>>,
) -> Result<(), VerificationError>
where
    CBlock: BlockT,
    CBlock::Hash: Into<H256>,
    DomainHeader: HeaderT,
    DomainHeader::Hash: Into<H256> + From<H256>,
{
    let InvalidStateTransitionProof {
        domain_id,
        proof,
        execution_phase,
        ..
    } = fraud_proof;

    let domain_runtime_code = fraud_proof_runtime_interface::get_fraud_proof_verification_info(
        bad_receipt.consensus_block_hash.into(),
        FraudProofVerificationInfoRequest::DomainRuntimeCode(*domain_id),
    )
    .and_then(FraudProofVerificationInfoResponse::into_domain_runtime_code)
    .ok_or(VerificationError::FailedToGetDomainRuntimeCode)?;

    let (pre_state_root, post_state_root) = execution_phase
        .pre_post_state_root::<CBlock, DomainHeader, Balance>(&bad_receipt, &bad_receipt_parent)?;

    let call_data = execution_phase
        .call_data::<CBlock, DomainHeader, Balance>(&bad_receipt, &bad_receipt_parent)?;

    let execution_result = fraud_proof_runtime_interface::execution_proof_check(
        pre_state_root,
        proof.encode(),
        execution_phase.verifying_method(),
        call_data.as_ref(),
        domain_runtime_code,
    )
    .ok_or(VerificationError::BadExecutionProof)?;

    let valid_post_state_root = execution_phase
        .decode_execution_result::<DomainHeader>(execution_result)?
        .into();

    if valid_post_state_root != post_state_root {
        Ok(())
    } else {
        Err(VerificationError::InvalidProof)
    }
}

/// Verifies invalid domain block hash fraud proof.
pub fn verify_invalid_domain_block_hash_fraud_proof<CBlock, Balance, DomainHeader>(
    bad_receipt: ExecutionReceipt<
        NumberFor<CBlock>,
        CBlock::Hash,
        DomainHeader::Number,
        DomainHeader::Hash,
        Balance,
    >,
    digest_storage_proof: StorageProof,
    parent_domain_block_hash: DomainHeader::Hash,
) -> Result<(), VerificationError>
where
    CBlock: BlockT,
    Balance: PartialEq + Decode,
    DomainHeader: HeaderT,
{
    let state_root = bad_receipt.final_state_root;
    let digest_storage_key = StorageKey(crate::fraud_proof::system_digest_final_key());

    let digest = StorageProofVerifier::<DomainHeader::Hashing>::get_decoded_value::<Digest>(
        &state_root,
        digest_storage_proof,
        digest_storage_key,
    )
    .map_err(|_| VerificationError::InvalidStorageProof)?;

    let derived_domain_block_hash = sp_domains::derive_domain_block_hash::<DomainHeader>(
        bad_receipt.domain_block_number,
        bad_receipt.domain_block_extrinsic_root,
        state_root,
        parent_domain_block_hash,
        digest,
    );

    if bad_receipt.domain_block_hash == derived_domain_block_hash {
        return Err(VerificationError::InvalidProof);
    }

    Ok(())
}

/// Verifies invalid total rewards fraud proof.
pub fn verify_invalid_total_rewards_fraud_proof<
    CBlock,
    DomainNumber,
    DomainHash,
    Balance,
    DomainHashing,
>(
    bad_receipt: ExecutionReceipt<
        NumberFor<CBlock>,
        CBlock::Hash,
        DomainNumber,
        DomainHash,
        Balance,
    >,
    storage_proof: &StorageProof,
) -> Result<(), VerificationError>
where
    CBlock: BlockT,
    Balance: PartialEq + Decode,
    DomainHashing: Hasher<Out = DomainHash>,
{
    let storage_key = StorageKey(crate::fraud_proof::operator_block_rewards_final_key());
    let storage_proof = storage_proof.clone();

    let total_rewards = StorageProofVerifier::<DomainHashing>::get_decoded_value::<Balance>(
        &bad_receipt.final_state_root,
        storage_proof,
        storage_key,
    )
    .map_err(|_| VerificationError::InvalidStorageProof)?;

    // if the rewards matches, then this is an invalid fraud proof since rewards must be different.
    if bad_receipt.total_rewards == total_rewards {
        return Err(VerificationError::InvalidProof);
    }

    Ok(())
}

fn convert_bundle_validity_to_generic<DomainHash>(
    bundle_validity: BundleValidity<DomainHash>,
) -> BundleValidity<H256>
where
    DomainHash: Into<H256> + Clone,
{
    match bundle_validity {
        BundleValidity::Invalid(invalid_bundle_type) => {
            BundleValidity::Invalid(invalid_bundle_type.clone())
        }
        BundleValidity::Valid(hash) => BundleValidity::Valid(hash.clone().into()),
    }
}

/// This function checks if this fraud proof is expected against the inboxed bundle entry it is targeting.
/// If the entry is expected then it will be returned
/// In any other cases VerificationError will be returned
fn check_expected_bundle_entry<CBlock, DomainNumber, DomainHash, Balance, ReceiptHash>(
    bad_receipt: &ExecutionReceipt<
        NumberFor<CBlock>,
        CBlock::Hash,
        DomainNumber,
        DomainHash,
        Balance,
    >,
    invalid_bundle_fraud_proof: &InvalidBundlesFraudProof<ReceiptHash>,
) -> Result<InboxedBundle<DomainHash>, VerificationError>
where
    CBlock: BlockT,
    DomainHash: Into<H256> + PartialEq + Clone,
{
    let targeted_invalid_bundle_entry = bad_receipt
        .inboxed_bundles
        .get(invalid_bundle_fraud_proof.bundle_index as usize)
        .ok_or(VerificationError::BundleNotFound)?;

    let invalid_type_of_proof = match invalid_bundle_fraud_proof.proof_data {
        ProofDataPerInvalidBundleType::OutOfRangeTx => {
            InvalidBundleType::OutOfRangeTx(invalid_bundle_fraud_proof.mismatched_extrinsic_index)
        }
    };

    let is_expected = if !invalid_bundle_fraud_proof.is_true_invalid_fraud_proof {
        // `FalseInvalid`
        // The proof trying to prove `bad_receipt_bundle`'s `invalid_bundle_type` is wrong,
        // so the proof should contains the same `invalid_bundle_type`
        targeted_invalid_bundle_entry.bundle
            == BundleValidity::Invalid(invalid_type_of_proof.clone())
    } else {
        // `TrueInvalid`
        match &targeted_invalid_bundle_entry.bundle {
            // The proof trying to prove the bundle is invalid due to `invalid_type_of_proof` while `bad_receipt_bundle`
            // think it is valid
            BundleValidity::Valid(_) => true,
            BundleValidity::Invalid(invalid_type) => {
                // The proof trying to prove there is an invalid extrinsic that the `bad_receipt_bundle` think is valid,
                // so the proof should point to an extrinsic that in front of the `bad_receipt_bundle`'s
                invalid_type_of_proof.extrinsic_index() < invalid_type.extrinsic_index() ||
                    // The proof trying to prove the invalid extrinsic can not pass a check that the `bad_receipt_bundle` think it can,
                    // so the proof should point to the same extrinsic and a check that perform before the `bad_receipt_bundle`'s
                    (invalid_type_of_proof.extrinsic_index() == invalid_type.extrinsic_index()
                        && invalid_type_of_proof.checking_order() < invalid_type.checking_order())
            }
        }
    };

    if !is_expected {
        return Err(VerificationError::UnexpectedTargetedBundleEntry {
            bundle_index: invalid_bundle_fraud_proof.bundle_index,
            fraud_proof_invalid_type_of_proof: invalid_type_of_proof,
            targeted_entry_bundle: convert_bundle_validity_to_generic(
                targeted_invalid_bundle_entry.bundle.clone(),
            ),
        });
    }

    Ok(targeted_invalid_bundle_entry.clone())
}

pub fn verify_invalid_bundles_fraud_proof<CBlock, DomainHeader, Balance>(
    bad_receipt: ExecutionReceipt<
        NumberFor<CBlock>,
        CBlock::Hash,
        HeaderNumberFor<DomainHeader>,
        HeaderHashFor<DomainHeader>,
        Balance,
    >,
    invalid_bundles_fraud_proof: &InvalidBundlesFraudProof<HeaderHashFor<DomainHeader>>,
) -> Result<(), VerificationError>
where
    CBlock: BlockT,
    CBlock::Hash: Into<H256>,
    DomainHeader: HeaderT,
    DomainHeader::Hash: Into<H256> + From<H256> + PartialEq + Clone,
{
    let invalid_bundle_entry = check_expected_bundle_entry::<
        CBlock,
        <DomainHeader as HeaderT>::Number,
        <DomainHeader as HeaderT>::Hash,
        Balance,
        <DomainHeader as HeaderT>::Hash,
    >(&bad_receipt, invalid_bundles_fraud_proof)?;

    let extrinsic = StorageProofVerifier::<BlakeTwo256>::get_decoded_value(
        &invalid_bundle_entry.extrinsics_root.into(),
        StorageProof::new(
            invalid_bundles_fraud_proof
                .extrinsic_inclusion_proof
                .clone()
                .drain(..),
        ),
        StorageKey(
            invalid_bundles_fraud_proof
                .mismatched_extrinsic_index
                .to_be_bytes()
                .to_vec(),
        ),
    )
    .map_err(|_e| VerificationError::InvalidProof)?;

    match invalid_bundles_fraud_proof.proof_data {
        ProofDataPerInvalidBundleType::OutOfRangeTx => {
            let is_tx_in_range = fraud_proof_runtime_interface::get_fraud_proof_verification_info(
                H256::from_slice(bad_receipt.consensus_block_hash.as_ref()),
                FraudProofVerificationInfoRequest::TxRangeCheck {
                    domain_id: invalid_bundles_fraud_proof.domain_id,
                    bundle_index: invalid_bundles_fraud_proof.bundle_index,
                    opaque_extrinsic: extrinsic,
                },
            )
            // TODO: In future if we start getting errors from host function, change below errors to include the underlying error
            .ok_or(VerificationError::FailedToGetResponseFromTxRangeHostFn)?
            .into_tx_range_check()
            .ok_or(VerificationError::ReceivedInvalidInfoFromTxRangeHostFn)?;

            // If it is true invalid fraud proof then tx must not be in range and
            // if it is false invalid fraud proof then tx must be in range for fraud
            // proof to be considered valid.
            if is_tx_in_range == invalid_bundles_fraud_proof.is_true_invalid_fraud_proof {
                return Err(VerificationError::InvalidProof);
            }
            Ok(())
        }
    }
}
