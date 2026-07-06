//! Concrete agent implementations.
//!
//! [`ConversationalAgent`] is the reference agent: it demonstrates the full
//! plan → act → reflect loop against the model router and event bus, and serves
//! as a template for the specialised agents (code, refactor, test, review, git).

mod conversational;
mod react;
mod specialists;

pub use conversational::ConversationalAgent;
pub use react::ReactAgent;
pub use specialists::SpecialistAgent;
