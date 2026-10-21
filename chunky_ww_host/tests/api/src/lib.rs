//! The API root of the simulated chunky device in `chunky_ww_host`'s end-to-end tests: what a firmware on
//! `chunky_ww` serves at the least.

use wire_weaver::prelude::*;

/// A device that updates its app A/B over `ww_dfu`, one step per call.
#[ww_api_root]
pub trait ChunkyApp {
    ww_impl!(node: ww_dfu::UpdateNode);
    ww_impl!(boot: ww_dfu::BootControl);
    ww_impl!(inventory: ww_dfu::Inventory);
    ww_impl!(stage: ww_dfu::Stage);
}
