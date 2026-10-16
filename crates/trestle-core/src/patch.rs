//! Three-state patch values (design doc §5.3): a PATCH property can be
//! absent (leave unchanged), `null` (clear a nullable value) or a value
//! (replace). `Option<T>` cannot express that, so patch inputs use
//! [`Patch<T>`] with `#[serde(default, skip_serializing_if = "Patch::is_missing")]`.
//!
//! Whether `null` is acceptable is the contract's decision: a non-nullable
//! patch field rejects `null` with `not_nullable` before this type sees it.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Patch<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> Patch<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Patch::Missing)
    }

    /// The new value to store, if this patch changes the field: `Some(None)`
    /// clears it, `Some(Some(v))` replaces it, `None` leaves it alone.
    pub fn change(self) -> Option<Option<T>> {
        match self {
            Patch::Missing => None,
            Patch::Null => Some(None),
            Patch::Value(value) => Some(Some(value)),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            None => Patch::Null,
            Some(value) => Patch::Value(value),
        })
    }
}

impl<T: Serialize> Serialize for Patch<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // Only reachable without `skip_serializing_if`; absent is the intent.
            Patch::Missing | Patch::Null => serializer.serialize_none(),
            Patch::Value(value) => value.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, PartialEq, Deserialize, Serialize)]
    struct Input {
        #[serde(default, skip_serializing_if = "Patch::is_missing")]
        subtitle: Patch<String>,
    }

    #[test]
    fn distinguishes_missing_null_and_value() {
        let missing: Input = serde_json::from_value(json!({})).unwrap();
        let null: Input = serde_json::from_value(json!({"subtitle": null})).unwrap();
        let value: Input = serde_json::from_value(json!({"subtitle": "x"})).unwrap();
        assert_eq!(missing.subtitle, Patch::Missing);
        assert_eq!(null.subtitle, Patch::Null);
        assert_eq!(value.subtitle, Patch::Value("x".into()));
        assert_eq!(serde_json::to_value(&missing).unwrap(), json!({}));
        assert_eq!(
            serde_json::to_value(&null).unwrap(),
            json!({"subtitle": null})
        );
        assert_eq!(null.subtitle.change(), Some(None));
        assert_eq!(Patch::<String>::Missing.change(), None);
    }
}
