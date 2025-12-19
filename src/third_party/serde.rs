//! Serde serialization support for [`crate::Brc`]

use crate::Brc;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

impl<T: Serialize> Serialize for Brc<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        T::serialize(&**self, serializer)
    }
}
/// Deserialize a [`Brc`].
///
/// Does not perform any deduplication.
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Brc<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Brc::new)
    }
}
