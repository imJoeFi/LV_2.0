use lv_core::{Catalog, KioskError, PersistentState, ProductId, PromoCode};
use redb::{Database, ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("kiosk_state");
const STATE_KEY: &str = "persistent_state";
const STATE_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
struct StoredState {
    schema_version: u32,
    state: PersistentState,
}

pub struct StateStore {
    database: Database,
    path: PathBuf,
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| StoreError::CreateDirectory {
                path: parent.to_owned(),
                source,
            })?;
        }
        let database = Database::create(path).map_err(|error| StoreError::Database {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        Ok(Self {
            database,
            path: path.to_owned(),
        })
    }

    pub fn load(&self) -> Result<PersistentState, StoreError> {
        let read = self
            .database
            .begin_read()
            .map_err(|error| self.database_error(error))?;
        let table = match read.open_table(STATE) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(PersistentState::default()),
            Err(error) => return Err(self.database_error(error)),
        };
        let Some(value) = table
            .get(STATE_KEY)
            .map_err(|error| self.database_error(error))?
        else {
            return Ok(PersistentState::default());
        };
        let stored: StoredState =
            serde_json::from_slice(value.value()).map_err(StoreError::Deserialize)?;
        if stored.schema_version != STATE_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: stored.schema_version,
                expected: STATE_SCHEMA_VERSION,
            });
        }
        Ok(stored.state)
    }

    pub fn save(&self, state: &PersistentState) -> Result<(), StoreError> {
        let encoded = serde_json::to_vec(&StoredState {
            schema_version: STATE_SCHEMA_VERSION,
            state: state.clone(),
        })
        .map_err(StoreError::Serialize)?;
        let write = self
            .database
            .begin_write()
            .map_err(|error| self.database_error(error))?;
        {
            let mut table = write
                .open_table(STATE)
                .map_err(|error| self.database_error(error))?;
            table
                .insert(STATE_KEY, encoded.as_slice())
                .map_err(|error| self.database_error(error))?;
        }
        write.commit().map_err(|error| self.database_error(error))
    }

    pub fn replace_codes_from_csv(
        state: &PersistentState,
        catalog: &Catalog,
        mut reader: impl Read,
    ) -> Result<PersistentState, StoreError> {
        let mut input = String::new();
        reader.read_to_string(&mut input)?;
        let mut replacement = state.clone();
        replacement.clear_codes();
        for record in csv::Reader::from_reader(input.as_bytes()).deserialize::<CodeRow>() {
            let row = record?;
            let code = PromoCode::parse(row.code)?;
            let product = ProductId::parse(row.product_id)
                .map_err(|error| StoreError::InvalidCatalogReference(error.to_string()))?;
            if catalog.product(&product).is_none() {
                return Err(StoreError::InvalidCatalogReference(format!(
                    "promo CSV refers to unknown product {product}"
                )));
            }
            replacement.grant(code, product, row.quantity)?;
        }
        Ok(replacement)
    }

    fn database_error(&self, error: impl std::fmt::Display) -> StoreError {
        StoreError::Database {
            path: self.path.clone(),
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CodeRow {
    code: String,
    product_id: String,
    quantity: u32,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not create state directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("redb database {path}: {message}")]
    Database { path: PathBuf, message: String },
    #[error("could not encode kiosk state: {0}")]
    Serialize(serde_json::Error),
    #[error("could not decode kiosk state: {0}")]
    Deserialize(serde_json::Error),
    #[error("unsupported kiosk state schema {found}; this build expects schema {expected}")]
    UnsupportedSchemaVersion { found: u32, expected: u32 },
    #[error("could not read promo CSV: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid promo CSV: {0}")]
    Csv(#[from] csv::Error),
    #[error("invalid promo CSV: {0}")]
    Kiosk(#[from] KioskError),
    #[error("invalid promo CSV: {0}")]
    InvalidCatalogReference(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use lv_core::{Catalog, SlotId};
    use std::path::Path;
    use std::str::FromStr;

    const CATALOG: &str = r#"
        [[products]]
        id = "water"
        name = "Sparkling Water"

        [[slots]]
        id = "A1"
        product = "water"
        payment = "promo"
    "#;

    #[test]
    fn state_round_trips_through_redb() {
        let directory = std::env::temp_dir().join(format!("lv-kiosk-{}", uuid::Uuid::new_v4()));
        let path = directory.join("state.redb");
        let store = StateStore::open(&path).unwrap();
        let mut state = PersistentState::default();
        state.set_inventory(SlotId::from_str("A1").unwrap(), 7);
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.inventory(&SlotId::from_str("A1").unwrap()), 7);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_a_state_document_from_another_schema_version() {
        let directory = std::env::temp_dir().join(format!("lv-kiosk-{}", uuid::Uuid::new_v4()));
        let store = StateStore::open(directory.join("state.redb")).unwrap();
        let encoded = serde_json::to_vec(&StoredState {
            schema_version: STATE_SCHEMA_VERSION + 1,
            state: PersistentState::default(),
        })
        .unwrap();
        let write = store.database.begin_write().unwrap();
        {
            let mut table = write.open_table(STATE).unwrap();
            table.insert(STATE_KEY, encoded.as_slice()).unwrap();
        }
        write.commit().unwrap();

        assert!(matches!(
            store.load(),
            Err(StoreError::UnsupportedSchemaVersion { .. })
        ));
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn csv_grants_products_instead_of_slots() {
        let directory = std::env::temp_dir().join(format!("lv-kiosk-{}", uuid::Uuid::new_v4()));
        let store = StateStore::open(directory.join("state.redb")).unwrap();
        let catalog = Catalog::parse(CATALOG, Path::new(".")).unwrap();
        let mut existing = PersistentState::default();
        existing.set_inventory(SlotId::from_str("A1").unwrap(), 9);
        let state = StateStore::replace_codes_from_csv(
            &existing,
            &catalog,
            "code,product_id,quantity\n123456,water,2\n".as_bytes(),
        )
        .unwrap();
        let entitlement = state
            .code(&PromoCode::parse("123456").unwrap())
            .unwrap()
            .entitlement(&ProductId::parse("water").unwrap())
            .unwrap();
        assert_eq!(entitlement.granted(), 2);
        assert_eq!(state.inventory(&SlotId::from_str("A1").unwrap()), 9);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }
}
