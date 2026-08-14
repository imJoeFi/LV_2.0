use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProductId(String);

impl ProductId {
    pub fn parse(value: impl Into<String>) -> Result<Self, CatalogError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(CatalogError::InvalidProductId(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProductId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SlotId(String);

impl SlotId {
    pub fn from_ap113(row: u8, column: u8) -> Result<Self, CatalogError> {
        if row >= 26 || column == 0 {
            return Err(CatalogError::InvalidSlot(format!("{row:02x}:{column:02x}")));
        }
        Ok(Self(format!("{}{}", char::from(b'A' + row), column)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SlotId {
    type Err = CatalogError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        let valid = bytes.len() >= 2
            && bytes[0].is_ascii_uppercase()
            && bytes[1..].iter().all(u8::is_ascii_digit)
            && bytes[1] != b'0';
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(CatalogError::InvalidSlot(value.to_owned()))
        }
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Product {
    id: ProductId,
    name: String,
    image: Option<PathBuf>,
}

impl Product {
    pub fn id(&self) -> &ProductId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn image(&self) -> Option<&Path> {
        self.image.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentPolicy {
    Promo,
    Lightning { price_cents: u32 },
}

impl PaymentPolicy {
    pub const fn price_cents(self) -> Option<u32> {
        match self {
            Self::Promo => None,
            Self::Lightning { price_cents } => Some(price_cents),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    id: SlotId,
    product_id: ProductId,
    payment: PaymentPolicy,
    enabled: bool,
}

impl Slot {
    pub fn id(&self) -> &SlotId {
        &self.id
    }

    pub fn product_id(&self) -> &ProductId {
        &self.product_id
    }

    pub const fn payment(&self) -> PaymentPolicy {
        self.payment
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

#[derive(Debug, Clone)]
pub struct Catalog {
    products: BTreeMap<ProductId, Product>,
    slots: BTreeMap<SlotId, Slot>,
}

impl Catalog {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CatalogError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|source| CatalogError::Read {
            path: path.to_owned(),
            source,
        })?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse(&source, base)
    }

    pub fn parse(source: &str, base: &Path) -> Result<Self, CatalogError> {
        let file: CatalogFile = toml::from_str(source)?;
        let mut products = BTreeMap::new();
        for config in file.products {
            let id = ProductId::parse(config.id)?;
            if config.name.trim().is_empty() {
                return Err(CatalogError::EmptyProductName(id));
            }
            let image = config.image.map(|path| base.join(path));
            let product = Product {
                id: id.clone(),
                name: config.name,
                image,
            };
            if products.insert(id.clone(), product).is_some() {
                return Err(CatalogError::DuplicateProduct(id));
            }
        }

        let mut slots = BTreeMap::new();
        for config in file.slots {
            let id = SlotId::from_str(&config.id)?;
            let product_id = ProductId::parse(config.product)?;
            if !products.contains_key(&product_id) {
                return Err(CatalogError::UnknownProduct {
                    slot: id,
                    product: product_id,
                });
            }
            let payment = match (config.payment, config.price_cents) {
                (PaymentConfig::Promo, None) => PaymentPolicy::Promo,
                (PaymentConfig::Promo, Some(_)) => {
                    return Err(CatalogError::PromoPrice(id));
                }
                (PaymentConfig::Lightning, Some(price_cents))
                    if price_cents > 0 && price_cents.is_multiple_of(10) =>
                {
                    PaymentPolicy::Lightning { price_cents }
                }
                (PaymentConfig::Lightning, Some(price_cents)) => {
                    return Err(CatalogError::InvalidLightningPrice {
                        slot: id,
                        price_cents,
                    });
                }
                (PaymentConfig::Lightning, None) => {
                    return Err(CatalogError::MissingLightningPrice(id));
                }
            };
            let slot = Slot {
                id: id.clone(),
                product_id,
                payment,
                enabled: config.enabled,
            };
            if slots.insert(id.clone(), slot).is_some() {
                return Err(CatalogError::DuplicateSlot(id));
            }
        }

        if products.is_empty() || slots.is_empty() {
            return Err(CatalogError::EmptyCatalog);
        }
        Ok(Self { products, slots })
    }

    pub fn product(&self, id: &ProductId) -> Option<&Product> {
        self.products.get(id)
    }

    pub fn slot(&self, id: &SlotId) -> Option<&Slot> {
        self.slots.get(id)
    }

    pub fn products(&self) -> impl Iterator<Item = &Product> {
        self.products.values()
    }

    pub fn slots(&self) -> impl Iterator<Item = &Slot> {
        self.slots.values()
    }

    pub fn slots_for_product<'a>(
        &'a self,
        product_id: &'a ProductId,
    ) -> impl Iterator<Item = &'a Slot> + 'a {
        self.slots
            .values()
            .filter(move |slot| slot.product_id() == product_id)
    }
}

#[derive(Debug, Deserialize)]
struct CatalogFile {
    products: Vec<ProductConfig>,
    slots: Vec<SlotConfig>,
}

#[derive(Debug, Deserialize)]
struct ProductConfig {
    id: String,
    name: String,
    image: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct SlotConfig {
    id: String,
    product: String,
    payment: PaymentConfig,
    price_cents: Option<u32>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

const fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PaymentConfig {
    Promo,
    Lightning,
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("could not read catalog {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid catalog TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("catalog must contain at least one product and one slot")]
    EmptyCatalog,
    #[error("invalid product id {0:?}; use lowercase letters, digits, '-' or '_'")]
    InvalidProductId(String),
    #[error("product {0} has an empty display name")]
    EmptyProductName(ProductId),
    #[error("duplicate product {0}")]
    DuplicateProduct(ProductId),
    #[error("invalid slot {0:?}; expected a selection such as A1")]
    InvalidSlot(String),
    #[error("duplicate slot {0}")]
    DuplicateSlot(SlotId),
    #[error("slot {slot} refers to unknown product {product}")]
    UnknownProduct { slot: SlotId, product: ProductId },
    #[error("promo slot {0} must not define a dollar price")]
    PromoPrice(SlotId),
    #[error("Lightning slot {0} must define price_cents")]
    MissingLightningPrice(SlotId),
    #[error("Lightning slot {slot} has invalid price {price_cents}; use a positive multiple of 10 cents")]
    InvalidLightningPrice { slot: SlotId, price_cents: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"
        [[products]]
        id = "water"
        name = "Sparkling Water"

        [[products]]
        id = "snack"
        name = "Trail Mix"

        [[slots]]
        id = "A1"
        product = "water"
        payment = "promo"

        [[slots]]
        id = "A2"
        product = "water"
        payment = "promo"

        [[slots]]
        id = "B1"
        product = "snack"
        payment = "lightning"
        price_cents = 250
    "#;

    #[test]
    fn parses_product_level_catalog_with_multiple_matching_slots() {
        let catalog = Catalog::parse(CATALOG, Path::new("assets")).unwrap();
        let product = ProductId::parse("water").unwrap();
        let slots = catalog
            .slots_for_product(&product)
            .map(|slot| slot.id().as_str())
            .collect::<Vec<_>>();
        assert_eq!(slots, ["A1", "A2"]);
    }

    #[test]
    fn rejects_lightning_prices_that_mdb_cannot_represent() {
        let source = CATALOG.replace("price_cents = 250", "price_cents = 255");
        assert!(matches!(
            Catalog::parse(&source, Path::new(".")),
            Err(CatalogError::InvalidLightningPrice { .. })
        ));
    }

    #[test]
    fn maps_ap113_item_bytes_to_selection() {
        assert_eq!(SlotId::from_ap113(0, 1).unwrap().as_str(), "A1");
        assert_eq!(SlotId::from_ap113(1, 2).unwrap().as_str(), "B2");
    }
}
