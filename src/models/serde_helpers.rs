//! Shared serde helpers for request models.

/// Deserialize an optional JSON field so an ABSENT key and an explicit `null` are
/// distinguishable: absent → `None` (keep current), present `null` → `Some(None)`
/// (clear/reset), present value → `Some(Some(v))` (set). A plain `Option<T>`
/// can't do this — serde collapses a present `null` into `None`, identical to an
/// absent key — so every "set to null to clear" field silently ignored `null`.
///
/// Use on any PATCH/PUT field whose contract includes clearing:
///
/// ```ignore
/// #[serde(default, deserialize_with = "crate::models::double_option")]
/// pub stripe_customer_id: Option<Option<String>>,
/// ```
///
/// Note `validator`'s derive cannot see through `Option<Option<T>>`, so fields
/// using this must be length/format-checked by hand in the handler.
pub fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Probe {
        #[serde(default, deserialize_with = "double_option")]
        field: Option<Option<String>>,
    }

    #[test]
    fn distinguishes_absent_null_and_value() {
        let absent: Probe = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.field, None, "absent key must mean 'keep current'");

        let cleared: Probe = serde_json::from_str(r#"{"field":null}"#).unwrap();
        assert_eq!(cleared.field, Some(None), "explicit null must mean 'clear'");

        let set: Probe = serde_json::from_str(r#"{"field":"x"}"#).unwrap();
        assert_eq!(set.field, Some(Some("x".to_string())));
    }
}
