//! App-independent engine protocol translation. Process ownership migrates separately.
pub mod claude_translate;
mod codex_elicitation;
mod codex_grants;
pub mod codex_history;
pub mod codex_interactions;
pub mod codex_process;
pub mod codex_rpc_translate;

pub mod codex_input;
