//! Collection of methods for block validation.

use reth_interfaces::{consensus::ConsensusError, RethResult};
use reth_primitives::{
    constants::{
        self,
        eip4844::{DATA_GAS_PER_BLOB, MAX_DATA_GAS_PER_BLOCK},
        MINIMUM_GAS_LIMIT,
    },
    eip4844::calculate_excess_blob_gas,
    BlockNumber, ChainSpec, GotExpected, Hardfork, Header, InvalidTransactionError, SealedBlock,
    SealedHeader, Transaction, TransactionSignedEcRecovered, TxEip1559, TxEip2930, TxEip4844,
    TxLegacy,
};
use reth_provider::{AccountReader, HeaderProvider, WithdrawalsProvider};
use std::collections::{hash_map::Entry, HashMap};

/// Validate header standalone
pub fn validate_header_standalone(
    header: &SealedHeader,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    // Gas used needs to be less then gas limit. Gas used is going to be check after execution.
    if header.gas_used > header.gas_limit {
        return Err(ConsensusError::HeaderGasUsedExceedsGasLimit {
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
        })
    }

    // Check if base fee is set.
    if chain_spec.fork(Hardfork::London).active_at_block(header.number) &&
        header.base_fee_per_gas.is_none()
    {
        return Err(ConsensusError::BaseFeeMissing)
    }

    let wd_root_missing = header.withdrawals_root.is_none() && !chain_spec.is_optimism();

    // EIP-4895: Beacon chain push withdrawals as operations
    if chain_spec.fork(Hardfork::Shanghai).active_at_timestamp(header.timestamp) && wd_root_missing
    {
        return Err(ConsensusError::WithdrawalsRootMissing)
    } else if !chain_spec.fork(Hardfork::Shanghai).active_at_timestamp(header.timestamp) &&
        header.withdrawals_root.is_some()
    {
        return Err(ConsensusError::WithdrawalsRootUnexpected)
    }

    // Ensures that EIP-4844 fields are valid once cancun is active.
    if chain_spec.fork(Hardfork::Cancun).active_at_timestamp(header.timestamp) {
        validate_4844_header_standalone(header)?;
    } else if header.blob_gas_used.is_some() {
        return Err(ConsensusError::BlobGasUsedUnexpected)
    } else if header.excess_blob_gas.is_some() {
        return Err(ConsensusError::ExcessBlobGasUnexpected)
    } else if header.parent_beacon_block_root.is_some() {
        return Err(ConsensusError::ParentBeaconBlockRootUnexpected)
    }

    Ok(())
}

/// Validate a transaction in regards to a block header.
///
/// The only parameter from the header that affects the transaction is `base_fee`.
pub fn validate_transaction_regarding_header(
    transaction: &Transaction,
    chain_spec: &ChainSpec,
    at_block_number: BlockNumber,
    at_timestamp: u64,
    base_fee: Option<u64>,
) -> Result<(), ConsensusError> {
    let chain_id = match transaction {
        Transaction::Legacy(TxLegacy { chain_id, .. }) => {
            // EIP-155: Simple replay attack protection: https://eips.ethereum.org/EIPS/eip-155
            if !chain_spec.fork(Hardfork::SpuriousDragon).active_at_block(at_block_number) &&
                chain_id.is_some()
            {
                return Err(InvalidTransactionError::OldLegacyChainId.into())
            }
            *chain_id
        }
        Transaction::Eip2930(TxEip2930 { chain_id, .. }) => {
            // EIP-2930: Optional access lists: https://eips.ethereum.org/EIPS/eip-2930 (New transaction type)
            if !chain_spec.fork(Hardfork::Berlin).active_at_block(at_block_number) {
                return Err(InvalidTransactionError::Eip2930Disabled.into())
            }
            Some(*chain_id)
        }
        Transaction::Eip1559(TxEip1559 {
            chain_id,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            ..
        }) => {
            // EIP-1559: Fee market change for ETH 1.0 chain https://eips.ethereum.org/EIPS/eip-1559
            if !chain_spec.fork(Hardfork::London).active_at_block(at_block_number) {
                return Err(InvalidTransactionError::Eip1559Disabled.into())
            }

            // EIP-1559: add more constraints to the tx validation
            // https://github.com/ethereum/EIPs/pull/3594
            if max_priority_fee_per_gas > max_fee_per_gas {
                return Err(InvalidTransactionError::TipAboveFeeCap.into())
            }

            Some(*chain_id)
        }
        Transaction::Eip4844(TxEip4844 {
            chain_id,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            ..
        }) => {
            // EIP-4844: Shard Blob Transactions https://eips.ethereum.org/EIPS/eip-4844
            if !chain_spec.fork(Hardfork::Cancun).active_at_timestamp(at_timestamp) {
                return Err(InvalidTransactionError::Eip4844Disabled.into())
            }

            // EIP-1559: add more constraints to the tx validation
            // https://github.com/ethereum/EIPs/pull/3594
            if max_priority_fee_per_gas > max_fee_per_gas {
                return Err(InvalidTransactionError::TipAboveFeeCap.into())
            }

            Some(*chain_id)
        }
        #[cfg(feature = "optimism")]
        Transaction::Deposit(_) => None,
    };
    if let Some(chain_id) = chain_id {
        if chain_id != chain_spec.chain().id() {
            return Err(InvalidTransactionError::ChainIdMismatch.into())
        }
    }
    // Check basefee and few checks that are related to that.
    // https://github.com/ethereum/EIPs/pull/3594
    if let Some(base_fee_per_gas) = base_fee {
        if transaction.max_fee_per_gas() < base_fee_per_gas as u128 {
            return Err(InvalidTransactionError::FeeCapTooLow.into())
        }
    }

    Ok(())
}

/// Iterate over all transactions, validate them against each other and against the block.
/// There is no gas check done as [REVM](https://github.com/bluealloy/revm/blob/fd0108381799662098b7ab2c429ea719d6dfbf28/crates/revm/src/evm_impl.rs#L113-L131) already checks that.
pub fn validate_all_transaction_regarding_block_and_nonces<
    'a,
    Provider: HeaderProvider + AccountReader,
>(
    transactions: impl Iterator<Item = &'a TransactionSignedEcRecovered>,
    header: &Header,
    provider: Provider,
    chain_spec: &ChainSpec,
) -> RethResult<()> {
    let mut account_nonces = HashMap::new();

    for transaction in transactions {
        validate_transaction_regarding_header(
            transaction,
            chain_spec,
            header.number,
            header.timestamp,
            header.base_fee_per_gas,
        )?;

        // Get nonce, if there is previous transaction from same sender we need
        // to take that nonce.
        let nonce = match account_nonces.entry(transaction.signer()) {
            Entry::Occupied(mut entry) => {
                let nonce = *entry.get();
                *entry.get_mut() += 1;
                nonce
            }
            Entry::Vacant(entry) => {
                let account = provider.basic_account(transaction.signer())?.unwrap_or_default();
                // Signer account shouldn't have bytecode. Presence of bytecode means this is a
                // smartcontract.
                if account.has_bytecode() {
                    return Err(ConsensusError::from(
                        InvalidTransactionError::SignerAccountHasBytecode,
                    )
                    .into())
                }
                let nonce = account.nonce;
                entry.insert(account.nonce + 1);
                nonce
            }
        };

        // check nonce
        if transaction.nonce() != nonce {
            return Err(ConsensusError::from(InvalidTransactionError::NonceNotConsistent).into())
        }
    }

    Ok(())
}

/// Validate a block without regard for state:
///
/// - Compares the ommer hash in the block header to the block body
/// - Compares the transactions root in the block header to the block body
/// - Pre-execution transaction validation
/// - (Optionally) Compares the receipts root in the block header to the block body
pub fn validate_block_standalone(
    block: &SealedBlock,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    // Check ommers hash
    let ommers_hash = reth_primitives::proofs::calculate_ommers_root(&block.ommers);
    if block.header.ommers_hash != ommers_hash {
        return Err(ConsensusError::BodyOmmersHashDiff(
            GotExpected { got: ommers_hash, expected: block.header.ommers_hash }.into(),
        ))
    }

    // Check transaction root
    if let Err(error) = block.ensure_transaction_root_valid() {
        return Err(ConsensusError::BodyTransactionRootDiff(error.into()));
    }

    // EIP-4895: Beacon chain push withdrawals as operations
    if chain_spec.fork(Hardfork::Shanghai).active_at_timestamp(block.timestamp) {
        let withdrawals =
            block.withdrawals.as_ref().ok_or(ConsensusError::BodyWithdrawalsMissing)?;
        let withdrawals_root = reth_primitives::proofs::calculate_withdrawals_root(withdrawals);
        let header_withdrawals_root =
            block.withdrawals_root.as_ref().ok_or(ConsensusError::WithdrawalsRootMissing)?;
        if withdrawals_root != *header_withdrawals_root {
            return Err(ConsensusError::BodyWithdrawalsRootDiff(
                GotExpected { got: withdrawals_root, expected: *header_withdrawals_root }.into(),
            ))
        }
    }

    // EIP-4844: Shard Blob Transactions
    if chain_spec.is_cancun_active_at_timestamp(block.timestamp) {
        // Check that the blob gas used in the header matches the sum of the blob gas used by each
        // blob tx
        let header_blob_gas_used = block.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;
        let total_blob_gas = block.blob_gas_used();
        if total_blob_gas != header_blob_gas_used {
            return Err(ConsensusError::BlobGasUsedDiff(GotExpected {
                got: header_blob_gas_used,
                expected: total_blob_gas,
            }))
        }
    }

    Ok(())
}

/// Checks the gas limit for consistency between parent and child headers.
///
/// The maximum allowable difference between child and parent gas limits is determined by the
/// parent's gas limit divided by the elasticity multiplier (1024).
///
/// This check is skipped if the Optimism flag is enabled in the chain spec, as gas limits on
/// Optimism can adjust instantly.
#[inline(always)]
fn check_gas_limit(
    parent: &SealedHeader,
    child: &SealedHeader,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    // Determine the parent gas limit, considering elasticity multiplier on the London fork.
    let mut parent_gas_limit = parent.gas_limit;
    if chain_spec.fork(Hardfork::London).transitions_at_block(child.number) {
        parent_gas_limit =
            parent.gas_limit * chain_spec.base_fee_params(child.timestamp).elasticity_multiplier;
    }

    // Check for an increase in gas limit beyond the allowed threshold.
    if child.gas_limit > parent_gas_limit {
        if child.gas_limit - parent_gas_limit >= parent_gas_limit / 1024 {
            return Err(ConsensusError::GasLimitInvalidIncrease {
                parent_gas_limit,
                child_gas_limit: child.gas_limit,
            });
        }
    }
    // Check for a decrease in gas limit beyond the allowed threshold.
    else if parent_gas_limit - child.gas_limit >= parent_gas_limit / 1024 {
        return Err(ConsensusError::GasLimitInvalidDecrease {
            parent_gas_limit,
            child_gas_limit: child.gas_limit,
        });
    }
    // Check if the child gas limit is below the minimum required limit.
    else if child.gas_limit < MINIMUM_GAS_LIMIT {
        return Err(ConsensusError::GasLimitInvalidMinimum { child_gas_limit: child.gas_limit });
    }

    Ok(())
}

/// Validate block in regards to parent
pub fn validate_header_regarding_parent(
    parent: &SealedHeader,
    child: &SealedHeader,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    // Parent number is consistent.
    if parent.number + 1 != child.number {
        return Err(ConsensusError::ParentBlockNumberMismatch {
            parent_block_number: parent.number,
            block_number: child.number,
        })
    }

    if parent.hash != child.parent_hash {
        return Err(ConsensusError::ParentHashMismatch(
            GotExpected { got: child.parent_hash, expected: parent.hash }.into(),
        ))
    }

    // timestamp in past check
    if child.header.is_timestamp_in_past(parent.timestamp) {
        return Err(ConsensusError::TimestampIsInPast {
            parent_timestamp: parent.timestamp,
            timestamp: child.timestamp,
        })
    }

    // TODO Check difficulty increment between parent and child
    // Ace age did increment it by some formula that we need to follow.

    cfg_if::cfg_if! {
        if #[cfg(feature = "optimism")] {
            // On Optimism, the gas limit can adjust instantly, so we skip this check
            // if the optimism feature is enabled in the chain spec.
            if !chain_spec.is_optimism() {
                check_gas_limit(parent, child, chain_spec)?;
            }
        } else {
            check_gas_limit(parent, child, chain_spec)?;
        }
    }

    // EIP-1559 check base fee
    if chain_spec.fork(Hardfork::London).active_at_block(child.number) {
        let base_fee = child.base_fee_per_gas.ok_or(ConsensusError::BaseFeeMissing)?;

        let expected_base_fee =
            if chain_spec.fork(Hardfork::London).transitions_at_block(child.number) {
                constants::EIP1559_INITIAL_BASE_FEE
            } else {
                // This BaseFeeMissing will not happen as previous blocks are checked to have them.
                parent
                    .next_block_base_fee(chain_spec.base_fee_params(child.timestamp))
                    .ok_or(ConsensusError::BaseFeeMissing)?
            };
        if expected_base_fee != base_fee {
            return Err(ConsensusError::BaseFeeDiff(GotExpected {
                expected: expected_base_fee,
                got: base_fee,
            }))
        }
    }

    // ensure that the blob gas fields for this block
    if chain_spec.fork(Hardfork::Cancun).active_at_timestamp(child.timestamp) {
        validate_4844_header_with_parent(parent, child)?;
    }

    Ok(())
}

/// Validate block in regards to chain (parent)
///
/// Checks:
///  If we already know the block.
///  If parent is known
///
/// Returns parent block header
pub fn validate_block_regarding_chain<PROV: HeaderProvider + WithdrawalsProvider>(
    block: &SealedBlock,
    provider: &PROV,
) -> RethResult<SealedHeader> {
    let hash = block.header.hash();

    // Check if block is known.
    if provider.is_known(&hash)? {
        return Err(ConsensusError::BlockKnown { hash, number: block.header.number }.into())
    }

    // Check if parent is known.
    let parent = provider
        .header(&block.parent_hash)?
        .ok_or(ConsensusError::ParentUnknown { hash: block.parent_hash })?;

    // Return parent header.
    Ok(parent.seal(block.parent_hash))
}

/// Validates that the EIP-4844 header fields are correct with respect to the parent block. This
/// ensures that the `blob_gas_used` and `excess_blob_gas` fields exist in the child header, and
/// that the `excess_blob_gas` field matches the expected `excess_blob_gas` calculated from the
/// parent header fields.
pub fn validate_4844_header_with_parent(
    parent: &SealedHeader,
    child: &SealedHeader,
) -> Result<(), ConsensusError> {
    // From [EIP-4844](https://eips.ethereum.org/EIPS/eip-4844#header-extension):
    //
    // > For the first post-fork block, both parent.blob_gas_used and parent.excess_blob_gas
    // > are evaluated as 0.
    //
    // This means in the first post-fork block, calculate_excess_blob_gas will return 0.
    let parent_blob_gas_used = parent.blob_gas_used.unwrap_or(0);
    let parent_excess_blob_gas = parent.excess_blob_gas.unwrap_or(0);

    if child.blob_gas_used.is_none() {
        return Err(ConsensusError::BlobGasUsedMissing)
    }
    let excess_blob_gas = child.excess_blob_gas.ok_or(ConsensusError::ExcessBlobGasMissing)?;

    let expected_excess_blob_gas =
        calculate_excess_blob_gas(parent_excess_blob_gas, parent_blob_gas_used);
    if expected_excess_blob_gas != excess_blob_gas {
        return Err(ConsensusError::ExcessBlobGasDiff {
            diff: GotExpected { got: excess_blob_gas, expected: expected_excess_blob_gas },
            parent_excess_blob_gas,
            parent_blob_gas_used,
        })
    }

    Ok(())
}

/// Validates that the EIP-4844 header fields exist and conform to the spec. This ensures that:
///
///  * `blob_gas_used` exists as a header field
///  * `excess_blob_gas` exists as a header field
///  * `parent_beacon_block_root` exists as a header field
///  * `blob_gas_used` is less than or equal to `MAX_DATA_GAS_PER_BLOCK`
///  * `blob_gas_used` is a multiple of `DATA_GAS_PER_BLOB`
pub fn validate_4844_header_standalone(header: &SealedHeader) -> Result<(), ConsensusError> {
    let blob_gas_used = header.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;

    if header.excess_blob_gas.is_none() {
        return Err(ConsensusError::ExcessBlobGasMissing)
    }

    if header.parent_beacon_block_root.is_none() {
        return Err(ConsensusError::ParentBeaconBlockRootMissing)
    }

    if blob_gas_used > MAX_DATA_GAS_PER_BLOCK {
        return Err(ConsensusError::BlobGasUsedExceedsMaxBlobGasPerBlock {
            blob_gas_used,
            max_blob_gas_per_block: MAX_DATA_GAS_PER_BLOCK,
        })
    }

    if blob_gas_used % DATA_GAS_PER_BLOB != 0 {
        return Err(ConsensusError::BlobGasUsedNotMultipleOfBlobGasPerBlob {
            blob_gas_used,
            blob_gas_per_blob: DATA_GAS_PER_BLOB,
        })
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::mock;
    use reth_interfaces::{
        provider::ProviderResult,
        test_utils::generators::{self, Rng},
    };
    use reth_primitives::{
        constants::eip4844::DATA_GAS_PER_BLOB, hex_literal::hex, proofs, Account, Address,
        BlockBody, BlockHash, BlockHashOrNumber, Bytes, ChainSpecBuilder, Header, Signature,
        TransactionKind, TransactionSigned, Withdrawal, MAINNET, U256,
    };
    use std::ops::RangeBounds;

    mock! {
        WithdrawalsProvider {}

        impl WithdrawalsProvider for WithdrawalsProvider {
            fn latest_withdrawal(&self) -> ProviderResult<Option<Withdrawal>> ;

            fn withdrawals_by_block(
                &self,
                _id: BlockHashOrNumber,
                _timestamp: u64,
            ) -> ProviderResult<Option<Vec<Withdrawal>>> ;
        }
    }

    struct Provider {
        is_known: bool,
        parent: Option<Header>,
        account: Option<Account>,
        withdrawals_provider: MockWithdrawalsProvider,
    }

    impl Provider {
        /// New provider with parent
        fn new(parent: Option<Header>) -> Self {
            Self {
                is_known: false,
                parent,
                account: None,
                withdrawals_provider: MockWithdrawalsProvider::new(),
            }
        }
        /// New provider where is_known is always true
        fn new_known() -> Self {
            Self {
                is_known: true,
                parent: None,
                account: None,
                withdrawals_provider: MockWithdrawalsProvider::new(),
            }
        }
    }

    impl AccountReader for Provider {
        fn basic_account(&self, _address: Address) -> ProviderResult<Option<Account>> {
            Ok(self.account)
        }
    }

    impl HeaderProvider for Provider {
        fn is_known(&self, _block_hash: &BlockHash) -> ProviderResult<bool> {
            Ok(self.is_known)
        }

        fn header(&self, _block_number: &BlockHash) -> ProviderResult<Option<Header>> {
            Ok(self.parent.clone())
        }

        fn header_by_number(&self, _num: u64) -> ProviderResult<Option<Header>> {
            Ok(self.parent.clone())
        }

        fn header_td(&self, _hash: &BlockHash) -> ProviderResult<Option<U256>> {
            Ok(None)
        }

        fn header_td_by_number(&self, _number: BlockNumber) -> ProviderResult<Option<U256>> {
            Ok(None)
        }

        fn headers_range(
            &self,
            _range: impl RangeBounds<BlockNumber>,
        ) -> ProviderResult<Vec<Header>> {
            Ok(vec![])
        }

        fn sealed_header(
            &self,
            _block_number: BlockNumber,
        ) -> ProviderResult<Option<SealedHeader>> {
            Ok(None)
        }

        fn sealed_headers_while(
            &self,
            _range: impl RangeBounds<BlockNumber>,
            _predicate: impl FnMut(&SealedHeader) -> bool,
        ) -> ProviderResult<Vec<SealedHeader>> {
            Ok(vec![])
        }
    }

    impl WithdrawalsProvider for Provider {
        fn withdrawals_by_block(
            &self,
            _id: BlockHashOrNumber,
            _timestamp: u64,
        ) -> ProviderResult<Option<Vec<Withdrawal>>> {
            self.withdrawals_provider.withdrawals_by_block(_id, _timestamp)
        }

        fn latest_withdrawal(&self) -> ProviderResult<Option<Withdrawal>> {
            self.withdrawals_provider.latest_withdrawal()
        }
    }

    fn mock_tx(nonce: u64) -> TransactionSignedEcRecovered {
        let request = Transaction::Eip2930(TxEip2930 {
            chain_id: 1u64,
            nonce,
            gas_price: 0x28f000fff,
            gas_limit: 10,
            to: TransactionKind::Call(Address::default()),
            value: 3_u64.into(),
            input: Bytes::from(vec![1, 2]),
            access_list: Default::default(),
        });

        let signature = Signature { odd_y_parity: true, r: U256::default(), s: U256::default() };

        let tx = TransactionSigned::from_transaction_and_signature(request, signature);
        let signer = Address::ZERO;
        TransactionSignedEcRecovered::from_signed_transaction(tx, signer)
    }

    fn mock_blob_tx(nonce: u64, num_blobs: usize) -> TransactionSigned {
        let mut rng = generators::rng();
        let request = Transaction::Eip4844(TxEip4844 {
            chain_id: 1u64,
            nonce,
            max_fee_per_gas: 0x28f000fff,
            max_priority_fee_per_gas: 0x28f000fff,
            max_fee_per_blob_gas: 0x7,
            gas_limit: 10,
            to: TransactionKind::Call(Address::default()),
            value: 3_u64.into(),
            input: Bytes::from(vec![1, 2]),
            access_list: Default::default(),
            blob_versioned_hashes: std::iter::repeat_with(|| rng.gen()).take(num_blobs).collect(),
        });

        let signature = Signature { odd_y_parity: true, r: U256::default(), s: U256::default() };

        TransactionSigned::from_transaction_and_signature(request, signature)
    }

    /// got test block
    fn mock_block() -> (SealedBlock, Header) {
        // https://etherscan.io/block/15867168 where transaction root and receipts root are cleared
        // empty merkle tree: 0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421

        let header = Header {
            parent_hash: hex!("859fad46e75d9be177c2584843501f2270c7e5231711e90848290d12d7c6dcdd").into(),
            ommers_hash: hex!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347").into(),
            beneficiary: hex!("4675c7e5baafbffbca748158becba61ef3b0a263").into(),
            state_root: hex!("8337403406e368b3e40411138f4868f79f6d835825d55fd0c2f6e17b1a3948e9").into(),
            transactions_root: hex!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").into(),
            receipts_root: hex!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421").into(),
            logs_bloom: hex!("002400000000004000220000800002000000000000000000000000000000100000000000000000100000000000000021020000000800000006000000002100040000000c0004000000000008000008200000000000000000000000008000000001040000020000020000002000000800000002000020000000022010000000000000010002001000000000020200000000000001000200880000004000000900020000000000020000000040000000000000000000000000000080000000000001000002000000000000012000200020000000000000001000000000000020000010321400000000100000000000000000000000000000400000000000000000").into(),
            difficulty: U256::ZERO, // total difficulty: 0xc70d815d562d3cfa955).into(),
            number: 0xf21d20,
            gas_limit: 0x1c9c380,
            gas_used: 0x6e813,
            timestamp: 0x635f9657,
            extra_data: hex!("")[..].into(),
            mix_hash: hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
            nonce: 0x0000000000000000,
            base_fee_per_gas: 0x28f0001df.into(),
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
        };
        // size: 0x9b5

        let mut parent = header.clone();
        parent.gas_used = 17763076;
        parent.gas_limit = 30000000;
        parent.base_fee_per_gas = Some(0x28041f7f5);
        parent.number -= 1;
        parent.timestamp -= 1;

        let ommers = Vec::new();
        let body = Vec::new();

        (SealedBlock { header: header.seal_slow(), body, ommers, withdrawals: None }, parent)
    }

    #[test]
    fn sanity_tx_nonce_check() {
        let (block, _) = mock_block();
        let tx1 = mock_tx(0);
        let tx2 = mock_tx(1);
        let provider = Provider::new_known();

        let txs = vec![tx1, tx2];
        validate_all_transaction_regarding_block_and_nonces(
            txs.iter(),
            &block.header,
            provider,
            &MAINNET,
        )
        .expect("To Pass");
    }

    #[test]
    fn nonce_gap_in_first_transaction() {
        let (block, _) = mock_block();
        let tx1 = mock_tx(1);
        let provider = Provider::new_known();

        let txs = vec![tx1];
        assert_eq!(
            validate_all_transaction_regarding_block_and_nonces(
                txs.iter(),
                &block.header,
                provider,
                &MAINNET,
            ),
            Err(ConsensusError::from(InvalidTransactionError::NonceNotConsistent).into())
        )
    }

    #[test]
    fn nonce_gap_on_second_tx_from_same_signer() {
        let (block, _) = mock_block();
        let tx1 = mock_tx(0);
        let tx2 = mock_tx(3);
        let provider = Provider::new_known();

        let txs = vec![tx1, tx2];
        assert_eq!(
            validate_all_transaction_regarding_block_and_nonces(
                txs.iter(),
                &block.header,
                provider,
                &MAINNET,
            ),
            Err(ConsensusError::from(InvalidTransactionError::NonceNotConsistent).into())
        );
    }

    #[test]
    fn valid_withdrawal_index() {
        let chain_spec = ChainSpecBuilder::mainnet().shanghai_activated().build();

        let create_block_with_withdrawals = |indexes: &[u64]| {
            let withdrawals = indexes
                .iter()
                .map(|idx| Withdrawal { index: *idx, ..Default::default() })
                .collect::<Vec<_>>();
            SealedBlock {
                header: Header {
                    withdrawals_root: Some(proofs::calculate_withdrawals_root(&withdrawals)),
                    ..Default::default()
                }
                .seal_slow(),
                withdrawals: Some(withdrawals),
                ..Default::default()
            }
        };

        // Single withdrawal
        let block = create_block_with_withdrawals(&[1]);
        assert_eq!(validate_block_standalone(&block, &chain_spec), Ok(()));

        // Multiple increasing withdrawals
        let block = create_block_with_withdrawals(&[1, 2, 3]);
        assert_eq!(validate_block_standalone(&block, &chain_spec), Ok(()));
        let block = create_block_with_withdrawals(&[5, 6, 7, 8, 9]);
        assert_eq!(validate_block_standalone(&block, &chain_spec), Ok(()));

        let (_, parent) = mock_block();
        let provider = Provider::new(Some(parent.clone()));
        let block = create_block_with_withdrawals(&[0, 1, 2]);
        let res = validate_block_regarding_chain(&block, &provider);
        assert!(res.is_ok());

        // Withdrawal index should be the last withdrawal index + 1
        let mut provider = Provider::new(Some(parent));
        let block = create_block_with_withdrawals(&[3, 4, 5]);
        provider
            .withdrawals_provider
            .expect_latest_withdrawal()
            .return_const(Ok(Some(Withdrawal { index: 2, ..Default::default() })));
        let res = validate_block_regarding_chain(&block, &provider);
        assert!(res.is_ok());
    }

    #[test]
    fn shanghai_block_zero_withdrawals() {
        // ensures that if shanghai is activated, and we include a block with a withdrawals root,
        // that the header is valid
        let chain_spec = ChainSpecBuilder::mainnet().shanghai_activated().build();

        let header = Header {
            base_fee_per_gas: Some(1337u64),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            ..Default::default()
        }
        .seal_slow();

        assert_eq!(validate_header_standalone(&header, &chain_spec), Ok(()));
    }

    #[test]
    fn cancun_block_incorrect_blob_gas_used() {
        let chain_spec = ChainSpecBuilder::mainnet().cancun_activated().build();

        // create a tx with 10 blobs
        let transaction = mock_blob_tx(1, 10);

        let header = Header {
            base_fee_per_gas: Some(1337u64),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(1),
            transactions_root: proofs::calculate_transaction_root(&[transaction.clone()]),
            ..Default::default()
        }
        .seal_slow();

        let body = BlockBody {
            transactions: vec![transaction],
            ommers: vec![],
            withdrawals: Some(vec![]),
        };

        let block = SealedBlock::new(header, body);

        // 10 blobs times the blob gas per blob.
        let expected_blob_gas_used = 10 * DATA_GAS_PER_BLOB;

        // validate blob, it should fail blob gas used validation
        assert_eq!(
            validate_block_standalone(&block, &chain_spec),
            Err(ConsensusError::BlobGasUsedDiff(GotExpected {
                got: 1,
                expected: expected_blob_gas_used
            }))
        );
    }

    #[test]
    fn test_valid_gas_limit_increase() {
        let parent = SealedHeader {
            header: Header { gas_limit: 1024 * 10, ..Default::default() },
            ..Default::default()
        };
        let child = SealedHeader {
            header: Header { gas_limit: parent.header.gas_limit + 5, ..Default::default() },
            ..Default::default()
        };
        let chain_spec = ChainSpec::default();

        assert_eq!(check_gas_limit(&parent, &child, &chain_spec), Ok(()));
    }

    #[test]
    fn test_gas_limit_below_minimum() {
        let parent = SealedHeader {
            header: Header { gas_limit: MINIMUM_GAS_LIMIT, ..Default::default() },
            ..Default::default()
        };
        let child = SealedHeader {
            header: Header { gas_limit: MINIMUM_GAS_LIMIT - 1, ..Default::default() },
            ..Default::default()
        };
        let chain_spec = ChainSpec::default();

        assert_eq!(
            check_gas_limit(&parent, &child, &chain_spec),
            Err(ConsensusError::GasLimitInvalidMinimum { child_gas_limit: child.gas_limit })
        );
    }

    #[test]
    fn test_invalid_gas_limit_increase_exceeding_limit() {
        let gas_limit = 1024 * 10;
        let parent = SealedHeader {
            header: Header { gas_limit, ..Default::default() },
            ..Default::default()
        };
        let child = SealedHeader {
            header: Header {
                gas_limit: parent.header.gas_limit + parent.header.gas_limit / 1024 + 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let chain_spec = ChainSpec::default();

        assert_eq!(
            check_gas_limit(&parent, &child, &chain_spec),
            Err(ConsensusError::GasLimitInvalidIncrease {
                parent_gas_limit: parent.header.gas_limit,
                child_gas_limit: child.header.gas_limit,
            })
        );
    }

    #[test]
    fn test_valid_gas_limit_decrease_within_limit() {
        let gas_limit = 1024 * 10;
        let parent = SealedHeader {
            header: Header { gas_limit, ..Default::default() },
            ..Default::default()
        };
        let child = SealedHeader {
            header: Header { gas_limit: parent.header.gas_limit - 5, ..Default::default() },
            ..Default::default()
        };
        let chain_spec = ChainSpec::default();

        assert_eq!(check_gas_limit(&parent, &child, &chain_spec), Ok(()));
    }

    #[test]
    fn test_invalid_gas_limit_decrease_exceeding_limit() {
        let gas_limit = 1024 * 10;
        let parent = SealedHeader {
            header: Header { gas_limit, ..Default::default() },
            ..Default::default()
        };
        let child = SealedHeader {
            header: Header {
                gas_limit: parent.header.gas_limit - parent.header.gas_limit / 1024 - 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let chain_spec = ChainSpec::default();

        assert_eq!(
            check_gas_limit(&parent, &child, &chain_spec),
            Err(ConsensusError::GasLimitInvalidDecrease {
                parent_gas_limit: parent.header.gas_limit,
                child_gas_limit: child.header.gas_limit,
            })
        );
    }
}
