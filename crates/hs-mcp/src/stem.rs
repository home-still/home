//! A document stem as it arrives from an MCP client.
//!
//! Every tool, prompt and resource that takes a stem (or a doc id, which is a
//! stem) builds storage keys from it. A stem like `..` or `a/b` would walk
//! out of the prefix the key is supposed to live under, so the check happens
//! once, at the boundary, in this type: a `Stem` that exists has passed
//! [`hs_common::validate_stem`].
//!
//! Tool parameters declare `Stem` instead of `String`. rmcp deserializes the
//! arguments before the handler runs, so a bad stem is answered with a
//! JSON-RPC *invalid params* error and the handler never executes. Resource
//! URIs are not deserialized; they go through [`Stem::parse`] and
//! [`Stem::invalid_params`].

use std::fmt;
use std::ops::Deref;

use rmcp::model::ErrorData;
use rmcp::schemars;

/// A stem that passed [`hs_common::validate_stem`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stem(String);

/// A rejected stem. Carries a (truncated) copy of the offending text so the
/// client can see which argument was wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidStemArg {
    shown: String,
    reason: hs_common::InvalidStem,
}

impl fmt::Display for InvalidStemArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid stem {:?}: {}", self.shown, self.reason)
    }
}

impl std::error::Error for InvalidStemArg {}

impl Stem {
    pub fn parse(raw: &str) -> Result<Self, InvalidStemArg> {
        match hs_common::validate_stem(raw) {
            Ok(()) => Ok(Self(raw.to_string())),
            Err(reason) => Err(InvalidStemArg {
                shown: raw.chars().take(64).collect(),
                reason,
            }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The JSON-RPC error for a stem taken from a resource URI.
    pub fn invalid_params(e: InvalidStemArg) -> ErrorData {
        ErrorData::invalid_params(e.to_string(), None)
    }
}

impl TryFrom<String> for Stem {
    type Error = InvalidStemArg;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl<'de> serde::Deserialize<'de> for Stem {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// On the wire and in the tool schema a stem is just a string.
impl schemars::JsonSchema for Stem {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Stem".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <String as schemars::JsonSchema>::json_schema(generator)
    }
}

impl serde::Serialize for Stem {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl Deref for Stem {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Stem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_unusual_but_legal_stems_pass() {
        for ok in [
            "10.1002_aur.2162",
            "Müller",
            "Año",
            "a b",
            ".hidden",
            "it's",
        ] {
            assert_eq!(Stem::parse(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn stems_that_can_leave_their_prefix_are_rejected() {
        for bad in [
            "",
            ".",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "/abs",
            "a\0b",
        ] {
            assert!(Stem::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn deserializing_a_bad_stem_fails_with_the_reason() {
        let err = serde_json::from_value::<Stem>(serde_json::json!("../x")).unwrap_err();
        assert!(err.to_string().contains("path separator"), "{err}");
        let ok: Stem = serde_json::from_value(serde_json::json!("fine")).unwrap();
        assert_eq!(&*ok, "fine");
    }

    #[test]
    fn the_schema_of_a_stem_is_a_plain_string() {
        let schema = serde_json::to_value(schemars::schema_for!(Stem)).unwrap();
        assert_eq!(schema["type"], "string", "{schema}");
    }

    #[test]
    fn the_offending_text_is_truncated_in_the_message() {
        let huge = format!("{}/x", "a".repeat(10_000));
        let msg = Stem::parse(&huge).unwrap_err().to_string();
        assert!(msg.len() < 200, "{}", msg.len());
    }
}
