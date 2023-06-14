//! Wrapper around revms state.

use reth_db::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    models::{AccountBeforeTx, BlockNumberAddress},
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_interfaces::db::DatabaseError;
use reth_primitives::{BlockNumber, Bytecode, StorageEntry, H256, U256};
use reth_revm_primitives::{
    db::states::{StateChangeset as RevmChange, StateReverts as RevmReverts},
    into_reth_acc,
};

/// Revert of the state.
#[derive(Default)]
pub struct StateReverts(pub RevmReverts);

impl From<RevmReverts> for StateReverts {
    fn from(revm: RevmReverts) -> Self {
        Self(revm)
    }
}

impl StateReverts {
    /// Write reverts to database.
    pub fn write_to_db<'a, TX: DbTxMut<'a> + DbTx<'a>>(
        self,
        tx: &TX,
        first_block: BlockNumber,
    ) -> Result<(), DatabaseError> {
        // Write storage changes
        tracing::trace!(target: "provider::reverts", "Writing storage changes");
        let mut storages_cursor = tx.cursor_dup_write::<tables::PlainStorageState>()?;
        let mut storage_changeset_cursor = tx.cursor_dup_write::<tables::StorageChangeSet>()?;
        for (block_number, storage_changes) in self.0.storage.into_iter().enumerate() {
            let block_number = first_block + block_number as BlockNumber;

            tracing::trace!(target: "provider::reverts", block_number=block_number,"Writing block change");
            for (address, wipe_storage, storage) in storage_changes.into_iter() {
                let storage_id = BlockNumberAddress((block_number, address));
                tracing::trace!(target: "provider::reverts","Writting revert for {:?}", address);
                // If we are writing the primary storage wipe transition, the pre-existing plain
                // storage state has to be taken from the database and written to storage history.
                // See [StorageWipe::Primary] for more details.
                let mut wiped_storage: Vec<(U256, U256)> = Vec::new();
                if wipe_storage {
                    tracing::trace!(target: "provider::reverts", "wipe storage storage changes");
                    if let Some((_, entry)) = storages_cursor.seek_exact(address)? {
                        wiped_storage.push((entry.key.into(), entry.value));
                        while let Some(entry) = storages_cursor.next_dup_val()? {
                            wiped_storage.push((entry.key.into(), entry.value))
                        }
                    }
                }
                tracing::trace!(target: "provider::reverts", "storage changes: {:?}",storage);
                // if empty just write storage reverts.
                if wiped_storage.is_empty() {
                    for (slot, old_value) in storage {
                        storage_changeset_cursor.append_dup(
                            storage_id,
                            StorageEntry { key: H256(slot.to_be_bytes()), value: old_value },
                        )?;
                    }
                } else {
                    // if there is some of wiped storage, they are both sorted, intersect both of
                    // them in in conflict use change from revert (discard values from wiped storage).
                    let mut wiped_iter = wiped_storage.into_iter();
                    let mut revert_iter = storage.into_iter();
                    // items to apply. both iterators are sorted.
                    let mut wiped_item = wiped_iter.next();
                    let mut revert_item = revert_iter.next();
                    loop {
                        let apply = match (wiped_item, revert_item) {
                            (None, None) => break,
                            (Some(w), None) => {
                                wiped_item = wiped_iter.next();
                                w.clone()
                            }
                            (None, Some(r)) => {
                                revert_item = revert_iter.next();
                                r.clone()
                            }
                            (Some(w), Some(r)) => {
                                if w.0 < r.0 {
                                    wiped_item = wiped_iter.next();
                                    // next key is from revert storage
                                    w.clone()
                                } else if w.0 > r.0 {
                                    revert_item = revert_iter.next();
                                    // next key is from wiped storage
                                    r.clone()
                                } else {
                                    wiped_item = wiped_iter.next();
                                    revert_item = revert_iter.next();
                                    // priority goes for storage if key is same.
                                    r.clone()
                                }
                            }
                        };

                        storage_changeset_cursor.append_dup(
                            storage_id,
                            StorageEntry { key: H256(apply.0.to_be_bytes()), value: apply.1 },
                        )?;
                    }
                }
            }
        }

        // Write account changes
        tracing::trace!(target: "provider::reverts", "Writing account changes");
        let mut account_changeset_cursor = tx.cursor_dup_write::<tables::AccountChangeSet>()?;
        for (block_number, account_block_reverts) in self.0.accounts.into_iter().enumerate() {
            let block_number = first_block + block_number as BlockNumber;
            for (address, info) in account_block_reverts {
                account_changeset_cursor.append_dup(
                    block_number,
                    AccountBeforeTx { address, info: info.map(|a| into_reth_acc(a)) },
                )?;
            }
        }

        Ok(())
    }
}

/// A change to the state of the world.
#[derive(Default)]
pub struct StateChange(pub RevmChange);

impl From<RevmChange> for StateChange {
    fn from(revm: RevmChange) -> Self {
        Self(revm)
    }
}

impl StateChange {
    /// Write the post state to the database.
    pub fn write_to_db<'a, TX: DbTxMut<'a> + DbTx<'a>>(self, tx: &TX) -> Result<(), DatabaseError> {
        // Write new storage state
        tracing::trace!(target: "provider::post_state", len = self.0.storage.len(), "Writing new storage state");
        let mut storages_cursor = tx.cursor_dup_write::<tables::PlainStorageState>()?;
        for (address, (_wipped, storage)) in self.0.storage.into_iter() {
            // Wipping of storage is done when appling the reverts.
            // // If the storage was wiped at least once, remove all previous entries from the
            // // database.
            // if wipped {
            //     tracing::trace!(target: "provider::post_state", ?address, "Wiping storage from plain state");
            //     if storages_cursor.seek_exact(address)?.is_some() {
            //         storages_cursor.delete_current_duplicates()?;
            //     }
            // }

            for (key, value) in storage.into_iter() {
                tracing::trace!(target: "provider::post_state", ?address, ?key, "Updating plain state storage");
                let key: H256 = key.into();
                if let Some(entry) = storages_cursor.seek_by_key_subkey(address, key)? {
                    if entry.key == key {
                        storages_cursor.delete_current()?;
                    }
                }

                if value != U256::ZERO {
                    storages_cursor.upsert(address, StorageEntry { key, value })?;
                }
            }
        }

        // Write new account state
        tracing::trace!(target: "provider::post_state", len = self.0.accounts.len(), "Writing new account state");
        let mut accounts_cursor = tx.cursor_write::<tables::PlainAccountState>()?;
        for (address, account) in self.0.accounts.into_iter() {
            if let Some(account) = account {
                tracing::trace!(target: "provider::post_state", ?address, "Updating plain state account");
                accounts_cursor.upsert(address, into_reth_acc(account))?;
            } else if accounts_cursor.seek_exact(address)?.is_some() {
                tracing::trace!(target: "provider::post_state", ?address, "Deleting plain state account");
                accounts_cursor.delete_current()?;
            }
        }

        // Write bytecode
        tracing::trace!(target: "provider::post_state", len = self.0.contracts.len(), "Writing bytecodes");
        let mut bytecodes_cursor = tx.cursor_write::<tables::Bytecodes>()?;
        for (hash, bytecode) in self.0.contracts.into_iter() {
            bytecodes_cursor.upsert(hash, Bytecode(bytecode))?;
        }
        Ok(())
    }
}
