//! The fake provider test rig — part of this crate's acceptance.
//!
//! [`FakeLlm`] is a scriptable [`ProviderTransport`] that never touches the
//! network: tests enqueue the exact exchanges they want, then assert on the
//! recorded request bodies and the number of calls the client made. Exhausting
//! the script panics with a loud message — an unexpected extra call (say, a
//! retry that should not happen) must fail the test, not quietly succeed.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Duration;

use crate::transport::{ProviderTransport, TransportError, TransportReply};

/// A scriptable in-memory provider.
///
/// Every `send` records the request body and consumes the next scripted
/// outcome. The rig is deliberately dumb: response classification, retry
/// policy, and decoding are the client's job and are exercised through it.
#[derive(Debug, Default)]
pub struct FakeLlm {
    script: RefCell<VecDeque<Result<TransportReply, TransportError>>>,
    sent: RefCell<Vec<String>>,
    sent_keys: RefCell<Vec<String>>,
}

impl FakeLlm {
    /// An empty rig: every call panics until something is enqueued.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a reply with the given status and raw body.
    pub fn enqueue_reply(&self, status: u16, body: impl Into<String>) {
        self.enqueue_reply_after(status, body, None);
    }

    /// Queue a reply that also carries a decoded `Retry-After` delay.
    pub fn enqueue_reply_after(
        &self,
        status: u16,
        body: impl Into<String>,
        retry_after: Option<Duration>,
    ) {
        self.script.borrow_mut().push_back(Ok(TransportReply {
            status,
            body: body.into(),
            retry_after,
        }));
    }

    /// Queue a transport failure.
    pub fn enqueue_failure(&self, output_observed: bool, message: impl Into<String>) {
        self.script.borrow_mut().push_back(Err(TransportError {
            output_observed,
            message: message.into(),
        }));
    }

    /// How many times the client called `send`.
    pub fn call_count(&self) -> usize {
        self.sent.borrow().len()
    }

    /// The request body sent on call `index`, in call order.
    pub fn sent_body(&self, index: usize) -> String {
        self.sent.borrow()[index].clone()
    }

    /// The idempotency key sent on call `index`, in call order.
    ///
    /// A call that bypassed the keyed seam has no key; tests that care about
    /// the key go through `complete`, which always uses that seam.
    ///
    /// # Panics
    ///
    /// When call `index` has no recorded key — no such call happened, or
    /// it reached the rig through `send` and bypassed the keyed seam
    /// (`FakeLlm: call #<index> was not keyed`).
    pub fn sent_idempotency_key(&self, index: usize) -> String {
        self.sent_keys
            .borrow()
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("FakeLlm: call #{index} was not keyed"))
    }
}

impl ProviderTransport for FakeLlm {
    fn send(&self, body: &str) -> Result<TransportReply, TransportError> {
        self.sent.borrow_mut().push(body.to_string());
        self.script
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| panic!("FakeLlm: script exhausted at call #{}", self.call_count()))
    }

    fn send_with_idempotency_key(
        &self,
        idempotency_key: &str,
        body: &str,
    ) -> Result<TransportReply, TransportError> {
        self.sent_keys
            .borrow_mut()
            .push(idempotency_key.to_string());
        self.send(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rig_records_bodies_and_counts_calls_in_order() {
        let fake = FakeLlm::new();
        fake.enqueue_reply(200, "first");
        fake.enqueue_reply(200, "second");

        assert_eq!(fake.send("a").expect("first reply").body, "first");
        assert_eq!(fake.send("b").expect("second reply").body, "second");
        assert_eq!(fake.call_count(), 2);
        assert_eq!(fake.sent_body(0), "a");
        assert_eq!(fake.sent_body(1), "b");
    }

    #[test]
    fn exhausting_the_script_panics_rather_than_inventing_a_reply() {
        let fake = FakeLlm::new();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fake.send("x")));
        assert!(panic.is_err());
    }

    #[test]
    fn the_keyed_seam_records_the_key_in_call_order() {
        let fake = FakeLlm::new();
        fake.enqueue_reply(200, "first");
        fake.enqueue_reply(200, "second");

        fake.send_with_idempotency_key("first-key", "a")
            .expect("first keyed reply");
        fake.send_with_idempotency_key("second-key", "b")
            .expect("second keyed reply");

        assert_eq!(fake.sent_idempotency_key(0), "first-key");
        assert_eq!(fake.sent_idempotency_key(1), "second-key");
    }
}
