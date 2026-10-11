//! The citation under test: a context-item reference plus the five-tuple.

/// One citation as the answer asserts it.
///
/// The five-tuple is the identity of a citation: `source_key +
/// revision_hash + chunk_id + chunk_hash + locator` is what makes a citation
/// verifiable at all. Two fields sit outside the tuple:
///
/// * `context_item_id` names the `run_context_items` row the model was shown
///   (the citation ID must exist in `run_context_items`). The model sees
///   these as `[f0001]`-style markers, so this is an opaque string: this
///   crate neither generates nor parses it.
/// * `quoted_text` is the span the answer claims to be quoting, required
///   to be locatable in the original chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// The context item this citation points at.
    pub context_item_id: String,
    /// Canonical source identity.
    pub source_key: String,
    /// Revision the citation binds to.
    pub revision_hash: String,
    /// Chunk the citation points at.
    pub chunk_id: String,
    /// Content hash the citation claims for that chunk.
    pub chunk_hash: String,
    /// Locator exactly as the store recorded it.
    pub locator_json: String,
    /// The text the answer says it is quoting.
    pub quoted_text: String,
}
