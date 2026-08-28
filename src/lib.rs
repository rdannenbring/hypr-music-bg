//! Types shared between the daemon and the settings GUI.
//!
//! These live in a library rather than in the binary because the GUI is a
//! separate executable, and two binaries cannot share a module by declaring it
//! twice — that produces two unrelated types with the same name, so a `Status`
//! decoded by one would not be the `Status` the other defined.
//!
//! Only the genuinely shared surface is here. The daemon's internals — art
//! sources, rendering, the tray — stay in the binary, since the GUI has no use
//! for them and linking them into it would drag the whole dependency tree along.

pub mod build_info;
pub mod config;
pub mod config_edit;
pub mod control;
