//! The chunky update commands on a device that serves `ww_dfu` with chunky's `chunky_ww`: `flash` (delta, trial,
//! confirm) with `status`, `confirm`, `revert` and `reboot`. The `ww-chunky` binary runs them.
#![forbid(unsafe_code)]

mod commands;

pub use commands::{Command, FlashArgs, Into, run_command};
