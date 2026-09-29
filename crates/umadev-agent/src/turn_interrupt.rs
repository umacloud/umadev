//! Stopping a base turn before its session is used again.
//!
//! [`BaseSession::interrupt`] returns `Ok` only once the aborted turn reached
//! its terminal event. `SessionError::InterruptPending`, any other error, or no
//! answer within the caller's bound means that turn may still end later and
//! deliver its result into whatever turn is sent next on the same session, so
//! the conversation runs one turn out of step. Such a session is not safe to
//! reuse and is closed: its next send then fails honestly instead of reading
//! another turn's end.

use std::time::Duration;

use umadev_runtime::BaseSession;

/// Interrupt the running turn within `bound` and close the session when the
/// interrupt did not settle. `true` when the session is safe to use again.
pub(crate) async fn interrupt_or_close(session: &mut dyn BaseSession, bound: Duration) -> bool {
    let outcome = tokio::time::timeout(bound, session.interrupt()).await;
    if matches!(outcome, Ok(Ok(()))) {
        return true;
    }
    tracing::warn!(
        ?outcome,
        "base turn did not settle after an interrupt; closing its session so that turn \
         cannot end the next one"
    );
    let _ = session.end().await;
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use umadev_runtime::{ApprovalDecision, SessionError, SessionEvent};

    /// How the mock answers `interrupt`.
    #[derive(Clone, Copy)]
    enum Interrupt {
        Settles,
        Pending,
        Fails,
        Hangs,
    }

    struct MockSession {
        interrupt: Interrupt,
        ends: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl BaseSession for MockSession {
        async fn send_turn(&mut self, _directive: String) -> Result<(), SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            None
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            match self.interrupt {
                Interrupt::Settles => Ok(()),
                Interrupt::Pending => Err(SessionError::InterruptPending("still running".into())),
                Interrupt::Fails => Err(SessionError::Send("pipe closed".into())),
                Interrupt::Hangs => {
                    std::future::pending::<()>().await;
                    Ok(())
                }
            }
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            *self.ends.lock().unwrap() += 1;
            Ok(())
        }
    }

    async fn run(interrupt: Interrupt) -> (bool, u32) {
        let ends = Arc::new(Mutex::new(0));
        let mut session = MockSession {
            interrupt,
            ends: Arc::clone(&ends),
        };
        let reusable = interrupt_or_close(&mut session, Duration::from_millis(50)).await;
        let ended = *ends.lock().unwrap();
        (reusable, ended)
    }

    #[tokio::test]
    async fn a_settled_interrupt_keeps_the_session() {
        assert_eq!(run(Interrupt::Settles).await, (true, 0));
    }

    #[tokio::test]
    async fn a_turn_that_may_still_end_closes_its_session() {
        for interrupt in [Interrupt::Pending, Interrupt::Fails, Interrupt::Hangs] {
            assert_eq!(run(interrupt).await, (false, 1));
        }
    }
}
