//! Request-side types and the canonical serialization behind `request_hash`.
//!
//! The canonical form of a request is a `serde_json` round-trip: every object's
//! keys come out in lexicographic order (`serde_json::Map` is a `BTreeMap` in
//! this workspace — free-form tool `parameters` schemas included), and arrays
//! keep the order the caller gave. Tool order is part of the request, not an
//! accident of construction: the provider's prompt cache is keyed on the
//! serialization order of the tools JSON, so reordering tools is a different
//! request and must produce a different [`LlmRequest::request_hash`].
//!
//! The exact canonical bytes are what the transport sends to the provider, so
//! the hash mirrors the provider's cache key rather than approximating it.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Domain separator for [`LlmRequest::request_hash`], so the hash cannot
/// collide with revision/chunk hashes that use their own domains.
const REQUEST_HASH_DOMAIN: &[u8] = b"momotaro-llm-request-v1\0";

/// Who authored one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The standing instruction message.
    System,
    /// The human turn.
    User,
    /// A previous model turn.
    Assistant,
}

/// One chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// Who authored it.
    pub role: Role,
    /// The text body.
    pub content: String,
}

/// One tool definition the model may call.
///
/// Serializes in the OpenAI-compatible shape
/// `{"type":"function","function":{"name":…,"description":…,"parameters":…}}`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    /// Tool name as the model sees it.
    pub name: String,
    /// One-line description for the model.
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: serde_json::Value,
}

/// The `function` object inside a serialized [`ToolDef`].
#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

impl Serialize for ToolDef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut outer = serializer.serialize_struct("ToolDef", 2)?;
        outer.serialize_field("type", "function")?;
        outer.serialize_field(
            "function",
            &WireFunction {
                name: &self.name,
                description: &self.description,
                parameters: &self.parameters,
            },
        )?;
        outer.end()
    }
}

/// One stateless provider request: model, messages, tools, max tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LlmRequest {
    /// Model identifier as the provider spells it.
    pub model: String,
    /// Conversation so far, in wire order.
    pub messages: Vec<Message>,
    /// Tool definitions offered to the model, in wire order.
    pub tools: Vec<ToolDef>,
    /// Upper bound on generated tokens.
    pub max_tokens: u32,
}

impl LlmRequest {
    /// The canonical JSON body: exactly what is sent to the provider and
    /// exactly what [`Self::request_hash`] covers.
    ///
    /// # Panics
    ///
    /// Only if an internal invariant breaks: the first `expect` fires when
    /// `serde_json::to_value` cannot encode the request (it always can —
    /// every field is JSON-native: a `serde_json::Value` tool schema,
    /// strings, and numbers), the second when a `serde_json::Value` cannot
    /// serialize (it always can).
    ///
    /// # Examples
    ///
    /// ```
    /// use momotaro_llm::{LlmRequest, Message, Role, ToolDef};
    ///
    /// let request = LlmRequest {
    ///     model: "m".to_string(),
    ///     messages: vec![Message {
    ///         role: Role::User,
    ///         content: "hi".to_string(),
    ///     }],
    ///     tools: vec![ToolDef {
    ///         name: "lookup".to_string(),
    ///         description: "find a note".to_string(),
    ///         parameters: serde_json::json!({ "type": "object", "properties": {} }),
    ///     }],
    ///     max_tokens: 16,
    /// };
    ///
    /// // Struct fields and free-form tool schemas are sorted alike.
    /// assert_eq!(
    ///     request.canonical_json(),
    ///     concat!(
    ///         r#"{"max_tokens":16,"messages":[{"content":"hi","role":"user"}],"#,
    ///         r#""model":"m","tools":[{"function":{"description":"find a note","#,
    ///         r#""name":"lookup","parameters":{"properties":{},"type":"object"}},"#,
    ///         r#""type":"function"}]}"#
    ///     )
    /// );
    /// ```
    pub fn canonical_json(&self) -> String {
        // A `Value` round-trip puts every object key — struct fields and
        // free-form `parameters` schemas alike — into lexicographic order
        // (`serde_json::Map` is a `BTreeMap` in this workspace); only arrays
        // keep the caller's order. Serializing a `Value` to a string cannot
        // fail — it holds only string keys and JSON-native scalars — so the
        // `expect` below is an invariant, not a swallowed error path.
        let value = serde_json::to_value(self).expect("request fields are JSON-native");
        serde_json::to_string(&value).expect("a Value always serializes")
    }

    /// `hex(sha256("momotaro-llm-request-v1\0" + canonical_json))`.
    ///
    /// Same input (up to field-equality) ⇒ byte-identical hash. Changing any
    /// covered input — model, message text or order, tool set or tool order,
    /// max_tokens — changes the hash. The `v1` suffix is the canonical-form
    /// version: a future form change bumps the domain, which makes old and
    /// new hashes different strings that never collide — a cache keyed on the
    /// hash can hold both eras safely.
    ///
    /// # Examples
    ///
    /// ```
    /// use momotaro_llm::{LlmRequest, Message, Role, ToolDef};
    ///
    /// let request = LlmRequest {
    ///     model: "m".to_string(),
    ///     messages: vec![Message {
    ///         role: Role::User,
    ///         content: "hi".to_string(),
    ///     }],
    ///     tools: vec![
    ///         ToolDef {
    ///             name: "first".to_string(),
    ///             description: "one".to_string(),
    ///             parameters: serde_json::json!({ "type": "object" }),
    ///         },
    ///         ToolDef {
    ///             name: "second".to_string(),
    ///             description: "two".to_string(),
    ///             parameters: serde_json::json!({ "type": "object" }),
    ///         },
    ///     ],
    ///     max_tokens: 64,
    /// };
    ///
    /// assert_eq!(request.request_hash().len(), 64, "hex of a sha256 digest");
    ///
    /// // Tool order is part of the request: the provider's prompt cache is
    /// // keyed on the serialization order of this array.
    /// let mut reordered = request.clone();
    /// reordered.tools.reverse();
    /// assert_ne!(reordered.request_hash(), request.request_hash());
    /// ```
    pub fn request_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(REQUEST_HASH_DOMAIN);
        hasher.update(self.canonical_json().as_bytes());
        hex::encode(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_request() -> LlmRequest {
        LlmRequest {
            model: "test-model".to_string(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: "sys".to_string(),
                },
                Message {
                    role: Role::User,
                    content: "hi".to_string(),
                },
            ],
            tools: vec![
                ToolDef {
                    name: "alpha".to_string(),
                    description: "first".to_string(),
                    parameters: json!({"type": "object", "properties": {"a": {"type": "string"}}}),
                },
                ToolDef {
                    name: "beta".to_string(),
                    description: "second".to_string(),
                    parameters: json!({"type": "object"}),
                },
            ],
            max_tokens: 64,
        }
    }

    #[test]
    fn building_the_same_request_twice_gives_byte_identical_json_and_hash() {
        let first = sample_request();
        let second = sample_request();
        assert_eq!(first.canonical_json(), second.canonical_json());
        assert_eq!(first.request_hash(), second.request_hash());
    }

    #[test]
    fn the_canonical_bytes_are_exactly_this_and_the_hash_is_their_sha256() {
        // Hand-written expectation: the canonical form is a contract, so its
        // bytes are pinned here rather than derived from the code under test.
        let expected = r#"{"max_tokens":64,"messages":[{"content":"sys","role":"system"},{"content":"hi","role":"user"}],"model":"test-model","tools":[{"function":{"description":"first","name":"alpha","parameters":{"properties":{"a":{"type":"string"}},"type":"object"}},"type":"function"},{"function":{"description":"second","name":"beta","parameters":{"type":"object"}},"type":"function"}]}"#;

        assert_eq!(sample_request().canonical_json(), expected);

        // Independent recomputation of the hash over those exact bytes.
        let mut hasher = Sha256::new();
        hasher.update(b"momotaro-llm-request-v1\0");
        hasher.update(expected.as_bytes());
        assert_eq!(
            sample_request().request_hash(),
            hex::encode(hasher.finalize())
        );
    }

    #[test]
    fn reordering_the_tools_changes_the_hash() {
        // Order is identity: the tools
        // list keeps the caller's order and is never sorted. A canonicalizer
        // that sorted the list would make this test red, and flipping this
        // test is only legal together with a contract-wording change approved
        // by the user.
        let mut request = sample_request();
        request.tools.swap(0, 1);
        assert_ne!(request.request_hash(), sample_request().request_hash());
    }

    #[test]
    fn changing_any_request_field_changes_the_hash() {
        let base = sample_request().request_hash();

        let mut with_other_model = sample_request();
        with_other_model.model = "other-model".to_string();
        assert_ne!(with_other_model.request_hash(), base);

        let mut with_other_message = sample_request();
        with_other_message.messages[1].content = "bye".to_string();
        assert_ne!(with_other_message.request_hash(), base);

        let mut with_other_first_message = sample_request();
        with_other_first_message.messages[0].content = "hey".to_string();
        assert_ne!(with_other_first_message.request_hash(), base);

        let mut with_swapped_messages = sample_request();
        with_swapped_messages.messages.swap(0, 1);
        assert_ne!(with_swapped_messages.request_hash(), base);

        let mut with_other_role = sample_request();
        with_other_role.messages[0].role = Role::User;
        assert_ne!(with_other_role.request_hash(), base);

        let mut with_other_tool = sample_request();
        with_other_tool.tools[0].name = "gamma".to_string();
        assert_ne!(with_other_tool.request_hash(), base);

        let mut with_other_description = sample_request();
        with_other_description.tools[0].description = "other".to_string();
        assert_ne!(with_other_description.request_hash(), base);

        let mut with_other_parameters = sample_request();
        with_other_parameters.tools[0].parameters = json!({"type": "number"});
        assert_ne!(with_other_parameters.request_hash(), base);

        let mut with_other_max_tokens = sample_request();
        with_other_max_tokens.max_tokens = 65;
        assert_ne!(with_other_max_tokens.request_hash(), base);
    }

    #[test]
    fn tool_parameters_serialize_with_sorted_keys_regardless_of_insertion_order() {
        // `serde_json::Map` is a BTreeMap here; if anyone enables
        // `preserve_order`, this test goes red and the hash stops being stable.
        let mut request = sample_request();
        request.tools[0].parameters =
            json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let reordered = {
            let mut r = sample_request();
            r.tools[0].parameters =
                json!({"properties": {"a": {"type": "string"}}, "type": "object"});
            r
        };
        assert_eq!(request.canonical_json(), reordered.canonical_json());
    }

    #[test]
    fn the_role_wire_spellings_are_pinned_to_hand_written_literals() {
        // Hand-written expectations: the wire spellings are contract, so they
        // are pinned here rather than derived from the serialization code.
        // `serde(rename_all)` is the single source of the spelling.
        for (role, expected) in [
            (Role::System, "system"),
            (Role::User, "user"),
            (Role::Assistant, "assistant"),
        ] {
            let wire = serde_json::to_value(role).expect("role serializes");
            assert_eq!(wire, json!(expected));
        }
    }
}
