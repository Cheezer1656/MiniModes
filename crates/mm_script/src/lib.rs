//! The script runtime: one sandboxed Luau VM per instance.
//!
//! The host (a game-host shard) owns a [`Vm`] and, once per tick, hands it a
//! [`Snapshot`] of player state plus the [`Event`]s since the last step. The
//! step runs on another thread under a CPU slice and returns [`Command`]s for
//! the host to validate and apply. Scripts never touch the ECS world, so a
//! slow or hostile script only delays its own instance. Rules (`game.Rules`,
//! `Region.Rules`) come back as commands too, and the host enforces them
//! instantly through a [`RuleSet`].

mod api;
mod bundle;
mod host;
mod io;
mod rules;
mod vm;

pub use bundle::{Bundle, ENTRY};
pub use io::{Command, Diagnostic, Event, Limits, PlayerId, PlayerSnap, RegionId, Snapshot, StepOutput};
pub use rules::{RuleSet, Rules};
pub use vm::Vm;

#[cfg(test)]
mod tests;
