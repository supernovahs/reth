use std::mem;

use crate::{Address, H256};
use reth_codecs::{main_codec, Compact};
use reth_rlp::{RlpDecodable, RlpDecodableWrapper, RlpEncodable, RlpEncodableWrapper};
use revm_primitives::{B160, U256};
use serde::{Deserialize, Serialize};

/// A list of addresses and storage keys that the transaction plans to access.
/// Accesses outside the list are possible, but become more expensive.
#[main_codec(rlp)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, RlpDecodable, RlpEncodable)]
#[serde(rename_all = "camelCase")]
pub struct AccessListItem {
    /// Account addresses that would be loaded at the start of execution
    pub address: Address,
    /// Keys of storage that would be loaded at the start of execution
    #[cfg_attr(
        any(test, feature = "arbitrary"),
        proptest(
            strategy = "proptest::collection::vec(proptest::arbitrary::any::<H256>(), 0..=20)"
        )
    )]
    pub storage_keys: Vec<H256>,
}

impl AccessListItem {
    /// Calculates a heuristic for the in-memory size of the [AccessListItem].
    #[inline]
    pub fn size(&self) -> usize {
        mem::size_of::<Address>() + self.storage_keys.capacity() * mem::size_of::<H256>()
    }
}

/// AccessList as defined in EIP-2930
#[main_codec(rlp)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, RlpDecodableWrapper, RlpEncodableWrapper)]
pub struct AccessList(
    #[cfg_attr(
        any(test, feature = "arbitrary"),
        proptest(
            strategy = "proptest::collection::vec(proptest::arbitrary::any::<AccessListItem>(), 0..=20)"
        )
    )]
    pub Vec<AccessListItem>,
);

impl AccessList {
    /// Converts the list into a vec, expected by revm
    pub fn flattened(self) -> Vec<(Address, Vec<U256>)> {
        self.flatten().collect()
    }

    /// Converts the ['AccessList'] into a Vec of (B160, `Vec<U256>`), as expected by revm
    pub fn to_ref_slice(&self) -> Vec<(B160, Vec<U256>)> {
        self.0
            .iter()
            .map(|item| {
                // Convert Address to B160 (if needed)
                let address: B160 = item.address;
                // Convert Vec<H256> to Vec<Uint<256, 4>>
                let storage_keys: Vec<U256> =
                    item.storage_keys.iter().map(|h256| U256::from_be_bytes(h256.0)).collect();

                (address, storage_keys)
            })
            .collect()
    }

    /// Returns an iterator over the list's addresses and storage keys.
    pub fn flatten(self) -> impl Iterator<Item = (Address, Vec<U256>)> {
        self.0.into_iter().map(|item| {
            (
                item.address,
                item.storage_keys.into_iter().map(|slot| U256::from_be_bytes(slot.0)).collect(),
            )
        })
    }

    /// Calculates a heuristic for the in-memory size of the [AccessList].
    #[inline]
    pub fn size(&self) -> usize {
        // take into account capacity
        self.0.iter().map(AccessListItem::size).sum::<usize>() +
            self.0.capacity() * mem::size_of::<AccessListItem>()
    }
}

/// Access list with gas used appended.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccessListWithGasUsed {
    /// List with accounts accessed during transaction.
    pub access_list: AccessList,
    /// Estimated gas used with access list.
    pub gas_used: U256,
}
