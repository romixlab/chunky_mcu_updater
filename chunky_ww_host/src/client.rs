//! The `ww_dfu` traits as host clients over global trait paths: they reach the traits wherever the device's API
//! has them, whatever that API and its version (API-20).

use wire_weaver_client::{Commander, DynClient};

/// `ww_dfu::UpdateNode` client.
pub struct UpdateNode {
    cmd: Commander,
}

/// `ww_dfu::BootControl` client.
pub struct BootControl {
    cmd: Commander,
}

/// `ww_dfu::Inventory` client.
pub struct Inventory {
    cmd: Commander,
}

/// `ww_dfu::Stage` client.
pub struct Stage {
    cmd: Commander,
}

// the generated methods carry the trait's docs only in the trait
#[allow(missing_docs)]
mod update_node_client {
    use super::*;
    wire_weaver::ww_codegen!(
        ww_dfu :: UpdateNode for UpdateNode,
        client = "trait_client",
        no_alloc = false,
        use_async = true,
    );
}

// the generated methods carry the trait's docs only in the trait
#[allow(missing_docs)]
mod boot_control_client {
    use super::*;
    wire_weaver::ww_codegen!(
        ww_dfu :: BootControl for BootControl,
        client = "trait_client",
        no_alloc = false,
        use_async = true,
    );
}

// the generated methods carry the trait's docs only in the trait
#[allow(missing_docs)]
mod inventory_client {
    use super::*;
    wire_weaver::ww_codegen!(
        ww_dfu :: Inventory for Inventory,
        client = "trait_client",
        no_alloc = false,
        use_async = true,
    );
}

// the generated methods carry the trait's docs only in the trait
#[allow(missing_docs)]
mod stage_client {
    use super::*;
    wire_weaver::ww_codegen!(
        ww_dfu :: Stage for Stage,
        client = "trait_client",
        no_alloc = false,
        use_async = true,
    );
}

/// The four `ww_dfu` traits a chunky node serves, on one connection.
pub struct Dfu {
    /// `UpdateNode`.
    pub node: UpdateNode,
    /// `BootControl`.
    pub boot: BootControl,
    /// `Inventory`.
    pub inventory: Inventory,
    /// `Stage`.
    pub stage: Stage,
}

impl Dfu {
    /// Clients of `device`'s `ww_dfu` traits.
    pub fn new(device: &DynClient) -> Self {
        let attach = |gid, name| device.global_attachment(gid, name).cmd_tx_take();
        Self {
            node: UpdateNode {
                cmd: attach(ww_dfu::UPDATE_NODE_FULL_GID, "UpdateNode"),
            },
            boot: BootControl {
                cmd: attach(ww_dfu::BOOT_CONTROL_FULL_GID, "BootControl"),
            },
            inventory: Inventory {
                cmd: attach(ww_dfu::INVENTORY_FULL_GID, "Inventory"),
            },
            stage: Stage {
                cmd: attach(ww_dfu::STAGE_FULL_GID, "Stage"),
            },
        }
    }
}
