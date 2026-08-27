use serde::{Deserialize, Serialize};
use std::fmt;

/// An exact amount of Bitcoin expressed in millisatoshis.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Msats(u64);

impl Msats {
    pub const ZERO: Self = Self(0);

    pub const fn from_msats(msats: u64) -> Self {
        Self(msats)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Msats {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} msats", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_the_underlying_millisatoshi_count() {
        let encoded = serde_json::to_string(&Msats::from_msats(110_000)).unwrap();
        assert_eq!(encoded, "110000");
    }
}
