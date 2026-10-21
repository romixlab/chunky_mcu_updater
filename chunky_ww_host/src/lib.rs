//! chunky on WireWeaver, host side: updates a device that serves `ww_dfu` with `chunky_ww` (scenario `2C-APP-STEP`,
//! A/B applied live, one step per call).
//!
//! [`flash`] does a whole update: [`Node::open`] reads `UpdateNode::node` and finds the layout, [`Node::stage`]
//! reads the manifest over `Inventory::chunks`, plans with `chunky_server`, sends the steps with up to
//! `StageLimits::in_flight` outstanding and has the node verify and stage the image; [`reboot_into_trial`] reboots
//! into the trial and reconnects ([`Reconnect`]: by serial number on USB); [`wait_confirmed`] waits for the
//! device's own confirm, or its revert. Each step is public for tools that want them one at a time; [`Event`]s
//! feed a progress bar.
//!
//! What the host does when the node disagrees:
//! - a reply lost (a timeout): `Stage::progress` says where the node is, the host goes on from there; a step sent
//!   twice is harmless (the node checks it by its hash);
//! - `OutOfOrder(step)`: the host goes on from that step;
//! - `HashMismatch`: the chunk arrived damaged, all of it again;
//! - `Custom(CUSTOM_NOT_BLANK)`, `SourceMismatch`, `Ecc`: DFU or ACTIVE is not what the manifest said; the host
//!   aborts, reads the manifest again and plans again (bounded by [`Options::max_replans`]).
//!
//! Waiting times come from the layout ([`Timeouts::for_geometry`]): an ERASE step's timeout covers the part's
//! slowest erase of one block ([`max_erase_time`], seconds for an STM32H7 sector, during which a single bank part
//! does not answer), the reboot covers the bootloader's swap.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod push;
pub mod timing;
pub mod update;

pub use client::Dfu;
pub use timing::{Timeouts, max_erase_time};
pub use update::{
    Event, Node, Options, Reconnect, Report, SealFor, UpdateError, check_image, flash, preset_geometry,
    reboot_into_trial, scheme, seal_for_node, wait_confirmed,
};
pub use ww_dfu;
