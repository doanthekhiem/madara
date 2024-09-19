use mp_class::{ClassInfo, CompiledSierra, ConvertedClass};
use rayon::{iter::ParallelIterator, slice::ParallelSlice};
use rocksdb::WriteOptions;
use starknet_types_core::felt::Felt;

use crate::{
    db_block_id::{DbBlockId, DbBlockIdResolvable},
    Column, DatabaseExt, MadaraBackend, MadaraStorageError, WriteBatchWithTransaction, DB_UPDATES_BATCH_SIZE,
};

const LAST_KEY: &[u8] = &[0xFF; 64];

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ClassInfoWithBlockNumber {
    class_info: ClassInfo,
    block_id: DbBlockId,
}

impl MadaraBackend {
    fn class_db_get_encoded_kv<V: serde::de::DeserializeOwned>(
        &self,
        is_pending: bool,
        key: &Felt,
        pending_col: Column,
        nonpending_col: Column,
    ) -> Result<Option<V>, MadaraStorageError> {
        // todo: smallint here to avoid alloc
        log::debug!("get encoded {key:#x}");
        let key_encoded = bincode::serialize(key)?;

        // Get from pending db, then normal db if not found.
        if is_pending {
            let col = self.db.get_column(pending_col);
            if let Some(res) = self.db.get_pinned_cf(&col, &key_encoded)? {
                return Ok(Some(bincode::deserialize(&res)?)); // found in pending
            }
        }
        log::debug!("get encoded: not in pending");

        let col = self.db.get_column(nonpending_col);
        let Some(val) = self.db.get_pinned_cf(&col, &key_encoded)? else { return Ok(None) };
        let val = bincode::deserialize(&val)?;

        Ok(Some(val))
    }

    pub fn get_class_info(
        &self,
        id: &impl DbBlockIdResolvable,
        class_hash: &Felt,
    ) -> Result<Option<ClassInfo>, MadaraStorageError> {
        let Some(requested_id) = id.resolve_db_block_id(self)? else { return Ok(None) };

        log::debug!("class info {requested_id:?} {class_hash:#x}");

        let Some(info) = self.class_db_get_encoded_kv::<ClassInfoWithBlockNumber>(
            requested_id.is_pending(),
            class_hash,
            Column::PendingClassInfo,
            Column::ClassInfo,
        )?
        else {
            return Ok(None);
        };

        log::debug!("class info got {:?}", info.block_id);

        let valid = match (requested_id, info.block_id) {
            (DbBlockId::Pending, _) => true,
            (DbBlockId::BlockN(block_n), DbBlockId::BlockN(real_block_n)) => real_block_n <= block_n,
            _ => false,
        };
        if !valid {
            return Ok(None);
        }
        log::debug!("valid");

        Ok(Some(info.class_info))
    }

    pub fn contains_class(&self, class_hash: &Felt) -> Result<bool, MadaraStorageError> {
        let col = self.db.get_column(Column::ClassInfo);
        let key_encoded = bincode::serialize(class_hash)?;
        Ok(self.db.get_pinned_cf(&col, &key_encoded)?.is_some())
    }

    pub fn get_sierra_compiled(
        &self,
        id: &impl DbBlockIdResolvable,
        class_hash: &Felt,
    ) -> Result<Option<CompiledSierra>, MadaraStorageError> {
        let Some(requested_id) = id.resolve_db_block_id(self)? else { return Ok(None) };

        log::debug!("sierra compiled {requested_id:?} {class_hash:#x}");

        let Some(compiled) = self.class_db_get_encoded_kv::<CompiledSierra>(
            requested_id.is_pending(),
            class_hash,
            Column::PendingClassCompiled,
            Column::ClassCompiled,
        )?
        else {
            return Ok(None);
        };

        Ok(Some(compiled))
    }

    /// NB: This functions needs to run on the rayon thread pool
    pub(crate) fn store_classes(
        &self,
        block_id: DbBlockId,
        converted_classes: &[ConvertedClass],
        col_info: Column,
        col_compiled: Column,
    ) -> Result<(), MadaraStorageError> {
        let mut writeopts = WriteOptions::new();
        writeopts.disable_wal(true);

        converted_classes.par_chunks(DB_UPDATES_BATCH_SIZE).try_for_each_init(
            || self.db.get_column(col_info),
            |col, chunk| {
                let mut batch = WriteBatchWithTransaction::default();
                for converted_class in chunk {
                    let class_hash = converted_class.class_hash();
                    let key_bin = bincode::serialize(&class_hash)?;
                    // this is a patch because some legacy classes are declared multiple times
                    if !self.contains_class(&class_hash)? {
                        // TODO: find a way to avoid this allocation
                        batch.put_cf(
                            col,
                            &key_bin,
                            bincode::serialize(&ClassInfoWithBlockNumber {
                                class_info: converted_class.info(),
                                block_id,
                            })?,
                        );
                    }
                }
                self.db.write_opt(batch, &writeopts)?;
                Ok::<_, MadaraStorageError>(())
            },
        )?;

        converted_classes
            .iter()
            .filter_map(|converted_class| match converted_class {
                ConvertedClass::Sierra(sierra) => Some((sierra.info.compiled_class_hash, sierra.compiled.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
            .par_chunks(DB_UPDATES_BATCH_SIZE)
            .try_for_each_init(
                || self.db.get_column(col_compiled),
                |col, chunk| {
                    let mut batch = WriteBatchWithTransaction::default();
                    for (key, value) in chunk {
                        log::trace!("Class compiled store key={key:#x}");
                        let key_bin = bincode::serialize(key)?;
                        // TODO: find a way to avoid this allocation
                        batch.put_cf(col, &key_bin, bincode::serialize(&value)?);
                    }
                    self.db.write_opt(batch, &writeopts)?;
                    Ok::<_, MadaraStorageError>(())
                },
            )?;

        Ok(())
    }

    /// NB: This functions needs to run on the rayon thread pool
    pub(crate) fn class_db_store_block(
        &self,
        block_number: u64,
        converted_classes: &[ConvertedClass],
    ) -> Result<(), MadaraStorageError> {
        self.store_classes(DbBlockId::BlockN(block_number), converted_classes, Column::ClassInfo, Column::ClassCompiled)
    }

    /// NB: This functions needs to run on the rayon thread pool
    pub(crate) fn class_db_store_pending(
        &self,
        converted_classes: &[ConvertedClass],
    ) -> Result<(), MadaraStorageError> {
        self.store_classes(
            DbBlockId::Pending,
            converted_classes,
            Column::PendingClassInfo,
            Column::PendingClassCompiled,
        )
    }

    pub(crate) fn class_db_clear_pending(&self) -> Result<(), MadaraStorageError> {
        let mut writeopts = WriteOptions::new();
        writeopts.disable_wal(true);

        self.db.delete_range_cf_opt(&self.db.get_column(Column::PendingClassInfo), &[] as _, LAST_KEY, &writeopts)?;
        self.db.delete_range_cf_opt(
            &self.db.get_column(Column::PendingClassCompiled),
            &[] as _,
            LAST_KEY,
            &writeopts,
        )?;

        Ok(())
    }
}
