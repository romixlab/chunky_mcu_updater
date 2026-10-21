//! A whole update of a chunky node: what the node is, what it holds, the plan, the steps, the image check, the
//! reboot into the trial and the device's own confirm.

use std::future::Future;
use std::time::Duration;

use chunky::ChunkHash;
use chunky::chips::PRESETS;
use chunky::descriptor::{DESCRIPTOR_LEN, ImageDescriptor, Scheme};
use chunky::geometry::FlashGeometry;
use chunky::manifest::{ChunkState as ManifestChunk, manifest_header, slot_digest};
use chunky::plan::PLAN_HEADER_LEN;
use chunky_server::image::read_descriptor_unverified;
use chunky_server::{
    ImageBuildError, ImageMeta, Manifest, MemStore, PlanStats, PlanningError, Policy, Release, Signer, build_image,
    plan,
};
use wire_weaver::prelude::MaxLen;
use wire_weaver_client::{CallError, DynClient, Error as ClientError};
use ww_dfu::{
    BootState, BootTarget, ChunkState, Error, LastResult, MAX_CHUNKS_PAGE, NodeInfoOwned, Profile, Seal, Slot,
    StageLimits, Target,
};

use crate::client::Dfu;
use crate::push::{self, Counts, Stopped, calls};
use crate::timing::Timeouts;

/// Why an update did not end with the new image running.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// The link failed (not a timeout the host retries).
    #[error("link: {0}")]
    Link(#[from] ClientError),
    /// The device does not implement a `ww_dfu` trait the update needs.
    #[error("the device does not implement {0}")]
    Unimplemented(String),
    /// The device refused a call for good.
    #[error("the device refused {call}: {error:?}")]
    Refused {
        /// What was called.
        call: &'static str,
        /// What the device answered.
        error: Error,
    },
    /// The device does not receive updates in its app: its updater is something else (a boot ROM).
    #[error(
        "the device's app does not receive updates (Profile::{profile:?}, no Stage): reboot it into its updater (a boot ROM: `ww stm32dfu flash`, `ww bootsel flash`)"
    )]
    NoStage {
        /// How the device switches images.
        profile: Profile,
    },
    /// No chip preset has the device's flash geometry: pass the geometry.
    #[error("no chip preset has geometry {0:#010x}: give the device's FlashGeometry")]
    UnknownGeometry(u32),
    /// The image is not for this device.
    #[error("the image is not for this device: {0}")]
    WrongImage(String),
    /// The image does not fit.
    #[error("{0}")]
    Build(#[from] ImageBuildError),
    /// No plan could be made.
    #[error("planning: {0}")]
    Planning(#[from] PlanningError),
    /// The plan was made again too often: the device keeps disagreeing with its own manifest.
    #[error("planned {0} times, the device still refuses a step")]
    Replans(u32),
    /// Too many replies were lost.
    #[error("{0} replies lost in a row")]
    Lost(u32),
    /// The device did not come back, or came back as something else.
    #[error("after the reboot: {0}")]
    Reconnect(String),
    /// The device did not reach the state the host waited for.
    #[error("waited {waited:?} for {what}, the device is {state:?}")]
    Timeout {
        /// What the host waited for.
        what: &'static str,
        /// For how long.
        waited: Duration,
        /// The state at the end.
        state: BootState,
    },
}

impl UpdateError {
    fn call(call: &'static str) -> impl Fn(CallError<Error>) -> UpdateError {
        move |e| match e {
            CallError::Device(error) => UpdateError::Refused { call, error },
            CallError::Unimplemented { resource } => UpdateError::Unimplemented(resource.to_string()),
            CallError::Client(e) => UpdateError::Link(e),
        }
    }
}

/// What happened, for a progress bar and a log. Nothing in here is needed to drive the update.
#[derive(Clone, Debug)]
pub enum Event<'a> {
    /// The node, as `UpdateNode::node` reports it.
    Node(&'a NodeInfoOwned),
    /// Chunk states read, of both slots.
    Inventory {
        /// Read so far.
        done: u32,
        /// Of.
        total: u32,
    },
    /// ACTIVE's manifest was known by its digest: only DFU was read.
    KnownActive,
    /// A plan was made.
    Planned {
        /// What it costs.
        stats: &'a PlanStats,
        /// Its steps.
        steps: u32,
        /// How many times it was made (1 the first time).
        attempt: u32,
    },
    /// The node had an unfinished session for this plan.
    Resumed {
        /// The step it goes on from.
        from: u32,
    },
    /// Steps sent, by the bytes of the plan they move.
    Sent {
        /// Bytes acknowledged.
        done: u64,
        /// Of.
        total: u64,
    },
    /// The node needs a new plan: it answered this.
    Replanning(Error),
    /// The node hashes the image and checks its seal.
    Verifying,
    /// The image is staged.
    Staged,
    /// The device was asked to reboot into the trial.
    Rebooting,
    /// The device is back, running this.
    Back(&'a NodeInfoOwned),
    /// The host waits for the device's own confirm.
    WaitingForConfirm,
}

/// How the host updates.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The device's layout, when no chip preset has its geometry id.
    pub geometry: Option<FlashGeometry>,
    /// The planner's choices.
    pub policy: Policy,
    /// Waiting times; by default from the geometry ([`Timeouts::for_geometry`]).
    pub timeouts: Option<Timeouts>,
    /// Steps outstanding at most, below the node's own `StageLimits::in_flight`.
    pub in_flight: Option<u8>,
    /// Times the host plans again before it gives up (default 3).
    pub max_replans: Option<u32>,
    /// Lost replies in a row before it gives up (default 8).
    pub max_lost: Option<u32>,
}

/// The layout of a node with this geometry id, from the chip presets.
pub fn preset_geometry(id: u32) -> Option<&'static FlashGeometry> {
    PRESETS.iter().map(|(_, g)| *g).find(|g| g.id() == id)
}

/// The scheme a node checks seals with, as chunky names it.
pub fn scheme(seal: Seal) -> Scheme {
    match seal {
        Seal::Unsigned => Scheme::Unsigned,
        Seal::Ed25519 => Scheme::Ed25519,
    }
}

/// What a sealed image is for, checked against the node before anything is sent: product, geometry, seal scheme
/// and length. The node checks it all again (and the seal); this only saves a session that cannot succeed.
pub fn check_image(
    image: &[u8],
    node: &NodeInfoOwned,
    geometry: &FlashGeometry,
) -> Result<ImageDescriptor, UpdateError> {
    let wrong = |what: String| UpdateError::WrongImage(what);
    if image.len() != geometry.active.len as usize {
        return Err(wrong(format!(
            "{} bytes, ACTIVE is {} (seal the payload for this layout)",
            image.len(),
            geometry.active.len
        )));
    }
    let raw: &[u8; DESCRIPTOR_LEN] = image.last_chunk().expect("as long as ACTIVE");
    let info = read_descriptor_unverified(raw).map_err(|e| wrong(format!("no chunky descriptor: {e:?}")))?;
    if info.geometry_id != node.geometry_id {
        return Err(wrong(format!(
            "geometry {:#010x}, the device has {:#010x}",
            info.geometry_id, node.geometry_id
        )));
    }
    if node.product_id != 0 && info.product_id != node.product_id {
        return Err(wrong(format!(
            "product {}, the device is product {}",
            info.product_id, node.product_id
        )));
    }
    if info.scheme != scheme(node.seal) {
        return Err(wrong(format!(
            "sealed {:?}, the device checks {:?}",
            info.scheme,
            scheme(node.seal)
        )));
    }
    Ok(info)
}

/// What to seal a payload with for a node: the node's product and scheme unless given, and refused when given and
/// different.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SealFor {
    /// Product id; the node's by default.
    pub product_id: Option<u32>,
    /// Release version; the running version + 1 by default.
    pub version: Option<u32>,
    /// Anti-rollback counter; the node's floor by default (never below it).
    pub security_counter: Option<u32>,
}

/// Seals `payload` for the node: the image a plan can be made of. `signer` must be of the node's scheme.
pub fn seal_for_node(
    payload: &[u8],
    node: &NodeInfoOwned,
    geometry: &FlashGeometry,
    meta: SealFor,
    signer: &Signer,
) -> Result<Vec<u8>, UpdateError> {
    let wrong = |what: String| UpdateError::WrongImage(what);
    let want = scheme(node.seal);
    if signer.trust().scheme() != want {
        return Err(wrong(format!(
            "the device checks {want:?} seals, the key given seals {:?}",
            signer.trust().scheme()
        )));
    }
    let product_id = match meta.product_id {
        Some(p) if node.product_id != 0 && p != node.product_id => {
            return Err(wrong(format!(
                "product {p} given, the device is product {}",
                node.product_id
            )));
        }
        Some(p) => p,
        None => node.product_id,
    };
    let meta = ImageMeta {
        product_id,
        version: meta.version.unwrap_or(node.app_version.unwrap_or(0).wrapping_add(1)),
        security_counter: meta.security_counter.unwrap_or(node.version_floor),
    };
    Ok(build_image(payload, geometry, &meta, signer)?)
}

/// How an update ended.
#[derive(Clone, Debug)]
pub struct Report {
    /// The node at the end.
    pub node: NodeInfoOwned,
    /// What the plan cost.
    pub stats: PlanStats,
    /// Plans made (1 without a replan).
    pub plans: u32,
    /// Step calls.
    pub counts: Counts,
}

impl Report {
    /// The node's own account of the update.
    pub fn last_result(&self) -> LastResult {
        self.node.last_result
    }
}

/// Reconnects to the same device after it reset: by serial number on USB, by address on a network, by path in
/// process. The device may take a while; `timeout` is how long the host waits in all.
pub trait Reconnect {
    /// The device, connected again.
    fn reconnect(&mut self, timeout: Duration) -> impl Future<Output = Result<DynClient, String>>;
}

/// A node's `ww_dfu` traits with what the host learnt of it.
pub struct Node {
    /// The trait clients.
    pub dfu: Dfu,
    /// The node as it last reported.
    pub info: NodeInfoOwned,
    /// Its layout.
    pub geometry: FlashGeometry,
    /// Waiting times.
    pub timeouts: Timeouts,
}

impl Node {
    /// Reads `UpdateNode::node` and finds the layout (`geometry`, else the chip preset with the node's geometry id).
    pub async fn open(
        device: &DynClient,
        geometry: Option<FlashGeometry>,
        timeouts: Option<Timeouts>,
    ) -> Result<Self, UpdateError> {
        let dfu = Dfu::new(device);
        let info = dfu.node.node().call().await.map_err(UpdateError::Link)?;
        let geometry = match geometry {
            Some(g) if g.id() != info.geometry_id => {
                return Err(UpdateError::WrongImage(format!(
                    "the geometry given is {:#010x}, the device has {:#010x}",
                    g.id(),
                    info.geometry_id
                )));
            }
            Some(g) => g,
            None => *preset_geometry(info.geometry_id).ok_or(UpdateError::UnknownGeometry(info.geometry_id))?,
        };
        let timeouts = timeouts.unwrap_or_else(|| Timeouts::for_geometry(&geometry));
        Ok(Self {
            dfu,
            info,
            geometry,
            timeouts,
        })
    }

    /// `UpdateNode::node` again.
    pub async fn refresh(&mut self) -> Result<&NodeInfoOwned, UpdateError> {
        self.info = self
            .dfu
            .node
            .node()
            .with_timeout(self.timeouts.call)
            .call()
            .await
            .map_err(UpdateError::Link)?;
        Ok(&self.info)
    }

    /// `BootControl::boot_state`.
    pub async fn boot_state(&mut self) -> Result<BootState, UpdateError> {
        let call = self.dfu.boot.boot_state().with_timeout(self.timeouts.call).call();
        // On a link that went down (the device reset) a call waits for a reconnect instead of timing out
        // (wire_weaver CLT-36): the host's own deadline, for the poll that notices the reset.
        match tokio::time::timeout(2 * self.timeouts.call, call).await {
            Ok(state) => state.map_err(UpdateError::Link),
            Err(_) => Err(UpdateError::Link(ClientError::Timeout)),
        }
    }

    /// `BootControl::confirm`: keep the running image.
    pub async fn confirm(&mut self) -> Result<(), UpdateError> {
        self.dfu
            .boot
            .confirm()
            .with_timeout(self.timeouts.call)
            .call()
            .await
            .map_err(UpdateError::call("BootControl::confirm"))
    }

    /// `BootControl::revert`: give up the trial image (the previous one runs after the next reset), or withdraw a
    /// staged one.
    pub async fn revert(&mut self) -> Result<(), UpdateError> {
        self.dfu
            .boot
            .revert()
            .with_timeout(self.timeouts.call)
            .call()
            .await
            .map_err(UpdateError::call("BootControl::revert"))
    }

    /// `BootControl::reboot`. A reply lost to the reset itself counts as done.
    pub async fn reboot(&mut self, into: BootTarget) -> Result<(), UpdateError> {
        match self.dfu.boot.reboot(into).with_timeout(self.timeouts.call).call().await {
            Ok(()) | Err(CallError::Client(ClientError::Timeout | ClientError::Disconnected)) => Ok(()),
            Err(e) => Err(UpdateError::call("BootControl::reboot")(e)),
        }
    }

    /// The chunk states of one slot, page by page.
    pub async fn read_slot(
        &mut self,
        slot: Slot,
        progress: &mut dyn FnMut(u32),
    ) -> Result<Vec<ManifestChunk>, UpdateError> {
        let chunks = self.geometry.chunks() as usize;
        let mut states = Vec::with_capacity(chunks);
        while states.len() < chunks {
            let first = states.len() as u16;
            let count = (chunks - states.len()).min(MAX_CHUNKS_PAGE) as u16;
            let page = self
                .dfu
                .inventory
                .chunks(slot, first, count)
                .with_timeout(self.timeouts.call)
                .call()
                .await
                .map_err(UpdateError::call("Inventory::chunks"))?;
            if page.is_empty() {
                return Err(UpdateError::Refused {
                    call: "Inventory::chunks",
                    error: Error::OutOfRange,
                });
            }
            states.extend(page.iter().take(chunks - states.len()).map(|state| match *state {
                ChunkState::Hash(hash) => ManifestChunk::Hash(ChunkHash(hash)),
                ChunkState::Blank => ManifestChunk::Blank,
                ChunkState::Unreadable => ManifestChunk::Unreadable,
            }));
            progress(page.len() as u32);
        }
        Ok(states)
    }

    /// The node's manifest: both slots over `Inventory::chunks`, or only DFU when the node's ACTIVE digest is that
    /// of `known_active` (the chunks of an image the host has, e.g. the release it planned the last update for).
    pub async fn read_manifest(
        &mut self,
        known_active: Option<&[ManifestChunk]>,
        progress: &mut dyn FnMut(Event<'_>),
    ) -> Result<Manifest, UpdateError> {
        let header = manifest_header(&self.geometry, scheme(self.info.seal));
        let chunks = self.geometry.chunks();
        let known = known_active.filter(|states| {
            self.info
                .manifest_digest
                .is_some_and(|d| d == slot_digest(&header, states.iter().copied()))
        });
        let total = if known.is_some() { chunks } else { 2 * chunks };
        let mut done = 0;
        let active = match known {
            Some(states) => {
                progress(Event::KnownActive);
                states.to_vec()
            }
            None => {
                self.read_slot(Slot::Active, &mut |n| {
                    done += n;
                    progress(Event::Inventory { done, total });
                })
                .await?
            }
        };
        let dfu = self
            .read_slot(Slot::Inactive, &mut |n| {
                done += n;
                progress(Event::Inventory { done, total });
            })
            .await?;
        Ok(Manifest { header, active, dfu })
    }

    async fn limits(&mut self) -> Result<StageLimits, UpdateError> {
        self.dfu
            .stage
            .limits()
            .with_timeout(self.timeouts.call)
            .call()
            .await
            .map_err(UpdateError::Link)
    }

    /// Puts `image` (sealed, as long as ACTIVE) into DFU and stages it: plan against what the node holds, send the
    /// steps, verify. Plans again when the node finds its manifest changed under the plan.
    pub async fn stage(
        &mut self,
        image: &[u8],
        options: &Options,
        progress: &mut dyn FnMut(Event<'_>),
    ) -> Result<Report, UpdateError> {
        if !self.info.offers.stage {
            return Err(UpdateError::NoStage {
                profile: self.info.profile,
            });
        }
        check_image(image, &self.info, &self.geometry)?;
        let mut store = MemStore::default();
        let release = Release::add(image, &self.geometry, &mut store)?;
        let limits = self.limits().await?;
        let in_flight = options
            .in_flight
            .map_or(limits.in_flight, |n| n.min(limits.in_flight))
            .max(1);
        let max_piece = (limits.max_piece as usize).clamp(1, ww_dfu::MAX_PIECE_LEN);
        let max_replans = options.max_replans.unwrap_or(3);
        let max_lost = options.max_lost.unwrap_or(8);
        let mut counts = Counts::default();
        let mut plans = 0;
        loop {
            plans += 1;
            let manifest = self.read_manifest(None, progress).await?;
            let plan = plan(&self.geometry, &manifest, &release, &store, &options.policy)?;
            progress(Event::Planned {
                stats: &plan.stats,
                steps: plan.steps.len() as u32,
                attempt: plans,
            });
            let encoded = plan.encode();
            let header = MaxLen::new(encoded[..PLAN_HEADER_LEN].to_vec()).expect("a plan header fits MAX_HEADER_LEN");
            let session = loop {
                match self
                    .dfu
                    .stage
                    .begin(Target::Live, header.clone())
                    .with_timeout(self.timeouts.call)
                    .call()
                    .await
                {
                    Ok(session) => break session,
                    // a session of another plan is open (a host that stopped half way): this one replaces it
                    Err(CallError::Device(Error::Busy)) => self.abort().await?,
                    Err(e) => return Err(UpdateError::call("Stage::begin")(e)),
                }
            };
            let calls = calls(&plan, max_piece);
            let start = calls
                .iter()
                .position(|c| c.index() >= session.resume_from)
                .unwrap_or(calls.len());
            if session.resumed {
                progress(Event::Resumed {
                    from: session.resume_from,
                });
            }
            let total: u64 = calls.iter().map(|c| c.weight()).sum();
            let mut done: u64 = calls[..start].iter().map(|c| c.weight()).sum();
            let mut highest = start;
            progress(Event::Sent { done, total });
            let pushed = push::push(
                &mut self.dfu.stage,
                &calls,
                start,
                in_flight as usize,
                &self.timeouts,
                max_lost,
                &mut counts,
                &mut |i| {
                    // a resend of a call already counted moves nothing
                    if i >= highest {
                        done += calls[highest..=i].iter().map(|c| c.weight()).sum::<u64>();
                        highest = i + 1;
                        progress(Event::Sent { done, total });
                    }
                },
            )
            .await;
            match pushed {
                Ok(()) => {}
                Err(Stopped::Replan(error)) => {
                    if plans > max_replans {
                        return Err(UpdateError::Replans(plans));
                    }
                    progress(Event::Replanning(error));
                    self.abort().await?;
                    continue;
                }
                Err(Stopped::Device(error)) => return Err(UpdateError::Refused { call: "a step", error }),
                Err(Stopped::Lost(n)) => return Err(UpdateError::Lost(n)),
                Err(Stopped::Client(e)) => return Err(UpdateError::Link(e)),
                Err(Stopped::Unimplemented) => return Err(UpdateError::Unimplemented("ww_dfu::Stage".into())),
            }
            progress(Event::Verifying);
            let mut lost = 0;
            let state = loop {
                match self.dfu.stage.finish().with_timeout(self.timeouts.finish).call().await {
                    Ok(state) => break state,
                    // a resent finish answers the staged state again
                    Err(CallError::Client(ClientError::Timeout)) if lost < max_lost => lost += 1,
                    Err(CallError::Device(e @ Error::VerifyFailed)) if plans <= max_replans => {
                        progress(Event::Replanning(e));
                        break BootState::Confirmed;
                    }
                    Err(e) => return Err(UpdateError::call("Stage::finish")(e)),
                }
            };
            if state != BootState::Staged {
                // the image did not verify: DFU holds something else than the plan said, read it again
                continue;
            }
            progress(Event::Staged);
            self.refresh().await?;
            return Ok(Report {
                node: self.info.clone(),
                stats: plan.stats,
                plans,
                counts,
            });
        }
    }

    async fn abort(&mut self) -> Result<(), UpdateError> {
        self.dfu
            .stage
            .abort()
            .with_timeout(self.timeouts.call)
            .call()
            .await
            .map_err(|e| UpdateError::call("Stage::abort")(CallError::Client(e)))
    }
}

/// Reboots a node with a staged image into its trial and waits until it answers again. Returns the node as it came
/// back: `Trial` running the new version when it booted, or the old image (`Reverted`, `Confirmed`) when the
/// bootloader would not run the new one.
pub async fn reboot_into_trial(
    mut node: Node,
    reconnect: &mut impl Reconnect,
    progress: &mut dyn FnMut(Event<'_>),
) -> Result<(Node, DynClient), UpdateError> {
    progress(Event::Rebooting);
    node.reboot(BootTarget::App).await?;
    let (geometry, timeouts) = (node.geometry, node.timeouts);
    drop(node);
    let device = reconnect
        .reconnect(timeouts.reboot)
        .await
        .map_err(UpdateError::Reconnect)?;
    let node = Node::open(&device, Some(geometry), Some(timeouts)).await?;
    progress(Event::Back(&node.info));
    Ok((node, device))
}

/// Waits for the device's own confirm of its trial image (its self-test), polling `BootControl::boot_state`. A
/// device that resets instead (its trial failed) is reconnected and comes back reverted. Returns the node at the
/// end: `Confirmed` with the new version, or `Reverted`.
pub async fn wait_confirmed(
    mut node: Node,
    mut device: DynClient,
    reconnect: &mut impl Reconnect,
    progress: &mut dyn FnMut(Event<'_>),
) -> Result<(Node, DynClient), UpdateError> {
    progress(Event::WaitingForConfirm);
    let timeouts = node.timeouts;
    let deadline = tokio::time::Instant::now() + timeouts.confirm;
    loop {
        let state = match node.boot_state().await {
            Ok(state) => state,
            Err(UpdateError::Link(_)) => {
                // the trial image reset before it confirmed: the bootloader reverts
                let geometry = node.geometry;
                drop(node);
                device = reconnect
                    .reconnect(timeouts.reboot)
                    .await
                    .map_err(UpdateError::Reconnect)?;
                node = Node::open(&device, Some(geometry), Some(timeouts)).await?;
                progress(Event::Back(&node.info));
                node.info.boot_state
            }
            Err(e) => return Err(e),
        };
        if state != BootState::Trial {
            node.refresh().await?;
            return Ok((node, device));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(UpdateError::Timeout {
                what: "the device's confirm",
                waited: timeouts.confirm,
                state,
            });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The whole update: stage `image` (sealed for the node), reboot into the trial, wait for the device's confirm.
/// The report's node says how it ended: `boot_state` `Confirmed` and `app_version` the new version, or the old
/// image running again (`Reverted`) and `last_result` saying why.
pub async fn flash(
    device: DynClient,
    reconnect: &mut impl Reconnect,
    image: &[u8],
    options: &Options,
    progress: &mut dyn FnMut(Event<'_>),
) -> Result<Report, UpdateError> {
    let mut node = Node::open(&device, options.geometry, options.timeouts).await?;
    progress(Event::Node(&node.info));
    let staged = node.stage(image, options, progress).await?;
    drop(device);
    let (node, device) = reboot_into_trial(node, reconnect, progress).await?;
    let (node, _device) = if node.info.boot_state == BootState::Trial {
        wait_confirmed(node, device, reconnect, progress).await?
    } else {
        (node, device)
    };
    Ok(Report {
        node: node.info,
        ..staged
    })
}
