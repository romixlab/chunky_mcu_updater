//! chunky on WireWeaver: the device side of `ww_dfu` (scenario `2C-APP-STEP`, A/B applied live, one step per call).
//!
//! [`ChunkyNode`] answers the methods of `ww_dfu`'s `UpdateNode`, `BootControl`, `Inventory` and `Stage` with the
//! chunky core underneath: the manifest for `Inventory`, the apply engine ([`Applier`]) for `Stage`, a
//! [`BootBackend`] (`chunky_swap`, `chunky_bank`) for the boot state. Each method takes the request's fields and
//! returns the `ww_dfu` reply, so the firmware's generated handlers only forward. What it leaves to the firmware:
//! `BootControl::reboot` (the reset, or the entry into a boot ROM), the `Stage::event` stream, and when to confirm.
//!
//! The node is the application's only handle on DFU and on the boot backend: it caches the boot state and
//! changes it only through its own methods.
//!
//! # What crosses the link and what the node makes of it
//!
//! Everything a host sends is untrusted (SECURITY.md, "the WireWeaver input path"). The plan header is the bytes of
//! the protocol spec and goes to [`Applier::begin`] as it is. The steps come as typed calls and are handed to the
//! engine as [`StepOp`]s, under the same rules as an encoded plan; a WRITE comes in pieces of at most
//! [`Limits::max_piece`] bytes, which the node puts together into one chunk before the engine sees it.
//!
//! - **Lost replies.** A step resent under an index the node has already run is answered `Ok` without touching
//!   flash, when it is one of the last [`RESEND_WINDOW`] steps, exactly the same step, and its chunk still has
//!   the step's hash ([`Applier::apply_at`], APPLY-2). A piece resent with the bytes it had is answered `Ok`.
//!   A resent `finish` answers the staged state again.
//! - **Resume.** `begin` with byte for byte the header of the open session resumes it: the answer says from which
//!   step to go on, and a WRITE half put together is dropped (it is sent again from offset 0). Another header
//!   while a session is open is [`Error::Busy`].
//! - **Reset.** A session lives in RAM: after a reset or a power cut it is gone, and the host reads the manifest
//!   again and plans what is left (SPEC §9.1). The flash is the progress record.
//!
//! # Errors
//!
//! [`ApplyError`]s map onto `ww_dfu`'s [`Error`]; one has no variant there and is
//! `Error::Custom(`[`CUSTOM_NOT_BLANK`]`)`.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use chunky::descriptor::{Expect, ImageDescriptor, ImageError, Scheme, Trust, verify_image};
use chunky::ecc::EccError;
use chunky::geometry::FlashGeometry;
use chunky::manifest::read_slot_digest;
use chunky::plan::PLAN_HEADER_LEN;
use chunky::{Applied, Applier, ApplyError, BootBackend, ChunkHash, RESEND_WINDOW, StepOp};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use wire_weaver::prelude::MaxLen;
/// The `ww_dfu` crate this one serves, for the firmware's handlers.
pub use ww_dfu;
use ww_dfu::{
    BootState, ChunkState, Error, LastResult, MAX_CHUNKS_PAGE, MAX_PIECE_LEN, MAX_PRODUCT_LEN, NodeInfo, Offers,
    Outcome, Profile, Progress, Role, Seal, Session, Slot, SlotLayout, SlotSwitch, StageLimits, Target,
};

/// `Error::Custom` code: the chunk a COPY or WRITE goes to is not blank (programmed, or torn by a cut). Nothing was
/// written. The host planned against a manifest that is no longer true: it reads the manifest again and plans
/// again (the planner then erases the block).
pub const CUSTOM_NOT_BLANK: u32 = 1;

/// What the node tells a host about receiving steps; the rest of [`StageLimits`] follows from chunky.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest WRITE piece, at most [`MAX_PIECE_LEN`] and at most a chunk: what the firmware's receive buffer holds.
    pub max_piece: u32,
    /// Step calls a host may have outstanding, `1` to [`RESEND_WINDOW`]. `1` on a part that stalls while it
    /// erases its own bank (STM32H72x: a 128 KB sector stalls the CPU for seconds; the host's timeout of an ERASE
    /// step must cover the part's worst sector erase).
    pub in_flight: u8,
    /// The firmware answers other requests while it writes flash. False on a single bank part.
    pub writes_in_background: bool,
}

/// What the node is: fixed for one boot.
#[derive(Clone, Copy, Debug)]
pub struct NodeConfig<'a> {
    /// The flash layout.
    pub geometry: &'a FlashGeometry,
    /// What images this device accepts.
    pub trust: Trust,
    /// This device's product.
    pub product_id: u32,
    /// Product name for `NodeInfo::product`, at most [`MAX_PRODUCT_LEN`] bytes.
    pub product: &'a str,
    /// The version floor as the firmware read it at start (`chunky::floor::read_floor` on FLOOR): only the
    /// bootloader raises it, at a reset.
    pub floor: u32,
    /// How steps may be sent.
    pub limits: Limits,
}

/// Why a [`ChunkyNode`] was not made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The buffer is shorter than two chunks.
    Buffer,
    /// [`Limits::max_piece`] is zero or larger than [`MAX_PIECE_LEN`] or a chunk.
    MaxPiece,
    /// [`Limits::in_flight`] is zero or larger than [`RESEND_WINDOW`].
    InFlight,
    /// The product name is longer than [`MAX_PRODUCT_LEN`].
    Product,
    /// ACTIVE does not verify (the bootloader verified it before it jumped, so the flash changed or failed).
    Running(ImageError),
    /// The boot backend failed.
    Backend,
}

/// A WRITE being put together from its pieces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
    index: u32,
    dst: u16,
    hash: [u8; 8],
    filled: u32,
}

/// The device side of `ww_dfu` on chunky. See the [crate docs](crate).
///
/// `A` is ACTIVE (read only: `chunky::ReadOnly`), `D` the image-sized part of DFU (`Part::prefix` of the image
/// length), `B` the application's handle on the boot backend.
pub struct ChunkyNode<'a, A, D, B> {
    config: NodeConfig<'a>,
    active: A,
    dfu: D,
    backend: B,
    /// One chunk of scratch for the engine and the manifest.
    scratch: &'a mut [u8],
    /// One chunk: the WRITE being put together.
    chunk: &'a mut [u8],
    running: ImageDescriptor,
    boot_state: chunky::BootState,
    /// `revert` was called on the trial image: `confirm` is refused from then on.
    reverting: bool,
    last_result: LastResult,
    active_digest: Option<[u8; 8]>,
    /// The open session.
    session: Option<Applier<'a>>,
    /// The WRITE of the open session being put together.
    piece: Option<Piece>,
    /// The last session finished and staged its image: `finish` may come again.
    finished: bool,
}

impl<'a, A, D, B> ChunkyNode<'a, A, D, B>
where
    A: ReadNorFlash,
    A::Error: EccError,
    D: NorFlash,
    D::Error: EccError,
    B: BootBackend,
{
    /// The node for this boot. Verifies ACTIVE to learn the running image (milliseconds on a fast part) and reads
    /// the boot state. `buf` is at least two chunks.
    pub fn new(
        config: NodeConfig<'a>,
        mut active: A,
        dfu: D,
        mut backend: B,
        buf: &'a mut [u8],
    ) -> Result<Self, ConfigError> {
        let geometry = config.geometry;
        let chunk = geometry.chunk as usize;
        if buf.len() < 2 * chunk {
            return Err(ConfigError::Buffer);
        }
        let limits = config.limits;
        if limits.max_piece == 0 || limits.max_piece as usize > MAX_PIECE_LEN.min(chunk) {
            return Err(ConfigError::MaxPiece);
        }
        if limits.in_flight == 0 || limits.in_flight as usize > RESEND_WINDOW {
            return Err(ConfigError::InFlight);
        }
        if config.product.len() > MAX_PRODUCT_LEN {
            return Err(ConfigError::Product);
        }
        let (scratch, rest) = buf.split_at_mut(chunk);
        let chunk_buf = &mut rest[..chunk];
        let expect = Expect {
            product_id: config.product_id,
            geometry_id: geometry.id(),
            image_len: geometry.image_len(),
            floor: 0,
        };
        let running = verify_image(&mut active, &config.trust, &expect, scratch).map_err(ConfigError::Running)?;
        let boot_state = backend.boot_state().map_err(|_| ConfigError::Backend)?;
        let last_result = match boot_state {
            chunky::BootState::Reverted => LastResult {
                outcome: Outcome::Reverted,
                version: 0,
            },
            _ => LastResult {
                outcome: Outcome::Nothing,
                version: 0,
            },
        };
        Ok(Self {
            config,
            active,
            dfu,
            backend,
            scratch,
            chunk: chunk_buf,
            running,
            boot_state,
            reverting: false,
            last_result,
            active_digest: None,
            session: None,
            piece: None,
            finished: false,
        })
    }

    /// The image that runs, as verified at start.
    pub fn running(&self) -> &ImageDescriptor {
        &self.running
    }

    /// The lowest security counter a plan's image may have: the floor, or the running image's counter once it
    /// is confirmed (the bootloader raises the floor to it only at the next reset).
    pub fn effective_floor(&self) -> u32 {
        match self.boot_state {
            chunky::BootState::Confirmed | chunky::BootState::Reverted => {
                self.config.floor.max(self.running.security_counter)
            }
            chunky::BootState::Staged | chunky::BootState::Trial => self.config.floor,
        }
    }

    // UpdateNode

    /// `UpdateNode::node`. No flash access: the boot state is the one this node last saw, the manifest digest the
    /// one `Inventory::manifest_digest` computed for ACTIVE, if it did.
    pub fn node(&self) -> NodeInfo<'a> {
        NodeInfo {
            role: Role::App,
            profile: Profile::AB(match self.config.geometry.backend {
                chunky::BackendKind::Swap => SlotSwitch::Copy,
                _ => SlotSwitch::Remap,
            }),
            product: MaxLen::new(self.config.product).expect("checked in new"),
            geometry_id: self.config.geometry.id(),
            product_id: self.running.product_id,
            seal: match self.config.trust.scheme() {
                Scheme::Unsigned => Seal::Unsigned,
                Scheme::Ed25519 => Seal::Ed25519,
            },
            app_version: Some(self.running.version),
            version_floor: self.effective_floor(),
            manifest_digest: self.active_digest,
            boot_state: ww_state(self.boot_state),
            last_result: self.last_result,
            offers: Offers {
                boot_control: true,
                inventory: true,
                stage: true,
                stage_bulk: false,
            },
        }
    }

    // BootControl

    /// `BootControl::boot_state`, read from the backend.
    pub fn boot_state(&mut self) -> Result<BootState, Error> {
        self.refresh()?;
        Ok(ww_state(self.boot_state))
    }

    /// `BootControl::confirm`: the running image works. Also what the firmware calls after its own self-test.
    /// Refused with `WrongState(Trial)` after `revert` in this boot.
    pub fn confirm(&mut self) -> Result<(), Error> {
        if self.reverting {
            return Err(Error::WrongState(BootState::Trial));
        }
        self.refresh()?;
        if self.boot_state != chunky::BootState::Trial {
            return Ok(());
        }
        self.backend.confirm().map_err(|_| Error::Flash)?;
        self.refresh()?;
        if self.boot_state == chunky::BootState::Confirmed {
            self.last_result = LastResult {
                outcome: Outcome::Confirmed,
                version: self.running.version,
            };
        }
        Ok(())
    }

    /// `BootControl::revert`. On the trial image: it is not confirmed from now on, so the next reset reverts it.
    /// With an image staged: the image is withdrawn, the running one stays.
    pub fn revert(&mut self) -> Result<(), Error> {
        self.refresh()?;
        match self.boot_state {
            chunky::BootState::Trial => {
                self.reverting = true;
                Ok(())
            }
            chunky::BootState::Staged => {
                self.backend.unstage().map_err(|_| Error::Flash)?;
                self.session = None;
                self.piece = None;
                self.finished = false;
                self.refresh()
            }
            state => Err(Error::WrongState(ww_state(state))),
        }
    }

    // Inventory

    /// `Inventory::layout`: both slots have the image's layout.
    pub fn layout(&self, _slot: Slot) -> Result<SlotLayout, Error> {
        let geometry = self.config.geometry;
        Ok(SlotLayout {
            chunk_size: geometry.chunk,
            chunk_count: geometry.chunks() as u16,
            erase_size: geometry.erase,
            erase_count: geometry.blocks() as u16,
        })
    }

    /// `Inventory::manifest_digest` (`chunky::manifest::slot_digest`): hashes the whole slot; ACTIVE's is kept and
    /// shows in `node` from then on.
    pub fn manifest_digest(&mut self, slot: Slot) -> Result<[u8; 8], Error> {
        let geometry = self.config.geometry;
        let scheme = self.config.trust.scheme();
        match slot {
            Slot::Active => {
                if let Some(digest) = self.active_digest {
                    return Ok(digest);
                }
                let digest =
                    read_slot_digest(geometry, scheme, &mut self.active, self.scratch).map_err(|_| Error::Flash)?;
                self.active_digest = Some(digest);
                Ok(digest)
            }
            Slot::Inactive => read_slot_digest(geometry, scheme, &mut self.dfu, self.scratch).map_err(|_| Error::Flash),
        }
    }

    /// `Inventory::chunks`: the states of up to `count` chunks from `first` into `out`; returns how many. Fewer
    /// come back at the end of the slot, past [`MAX_CHUNKS_PAGE`] or past `out`'s length (the firmware sizes `out`
    /// to what its reply buffer holds).
    pub fn chunks(&mut self, slot: Slot, first: u16, count: u16, out: &mut [ChunkState]) -> Result<usize, Error> {
        let geometry = self.config.geometry;
        let chunks = geometry.chunks();
        if first as u32 >= chunks {
            return Err(Error::OutOfRange);
        }
        let n = (count as usize)
            .min(MAX_CHUNKS_PAGE)
            .min(out.len())
            .min((chunks - first as u32) as usize);
        for (i, entry) in out[..n].iter_mut().enumerate() {
            let at = (first as u32 + i as u32) * geometry.chunk;
            let state = match slot {
                Slot::Active => chunky::ChunkState::read(&mut self.active, at, self.scratch).map_err(|_| ()),
                Slot::Inactive => chunky::ChunkState::read(&mut self.dfu, at, self.scratch).map_err(|_| ()),
            };
            *entry = match state.map_err(|()| Error::Flash)? {
                chunky::ChunkState::Hash(hash) => ChunkState::Hash(hash.0),
                chunky::ChunkState::Blank => ChunkState::Blank,
                chunky::ChunkState::Unreadable => ChunkState::Unreadable,
            };
        }
        Ok(n)
    }

    // Stage

    /// `Stage::limits`.
    pub fn limits(&self) -> StageLimits {
        let limits = self.config.limits;
        StageLimits {
            max_piece: limits.max_piece,
            in_flight: limits.in_flight,
            live: true,
            stored: false,
            plan_area: 0,
            writes_in_background: limits.writes_in_background,
        }
    }

    /// `Stage::begin`: checks the header and the target's descriptor (nothing is written), or resumes the open
    /// session when `header` is byte for byte its header.
    pub fn begin(&mut self, target: Target, header: &[u8]) -> Result<Session, Error> {
        if target != Target::Live {
            return Err(Error::Unsupported);
        }
        if let Some(applier) = &self.session {
            if !applier.is_plan(header) {
                return Err(Error::Busy);
            }
            self.piece = None;
            return Ok(Session {
                steps: applier.step_count(),
                resumed: true,
                resume_from: applier.steps_done(),
            });
        }
        let header: &[u8; PLAN_HEADER_LEN] = header.try_into().map_err(|_| Error::BadHeader)?;
        self.refresh()?;
        let applier = Applier::begin(
            self.config.geometry,
            header,
            &self.config.trust,
            self.config.product_id,
            self.effective_floor(),
            &mut self.backend,
        );
        let applier = match applier {
            Ok(applier) => applier,
            Err(e) => {
                self.last_result = LastResult {
                    outcome: Outcome::Rejected,
                    version: 0,
                };
                return Err(self.error(e));
            }
        };
        let steps = applier.step_count();
        self.session = Some(applier);
        self.piece = None;
        self.finished = false;
        Ok(Session {
            steps,
            resumed: false,
            resume_from: 0,
        })
    }

    /// `Stage::erase`: step `index` erases erase block `block` of DFU.
    pub fn erase(&mut self, index: u32, block: u16) -> Result<(), Error> {
        self.step(index, StepOp::Erase { block })
    }

    /// `Stage::copy`: step `index` programs chunk `dst` of DFU with chunk `src` of ACTIVE, which must have `hash`.
    pub fn copy(&mut self, index: u32, dst: u16, src: u16, hash: [u8; 8]) -> Result<(), Error> {
        self.step(
            index,
            StepOp::Copy {
                dst,
                src,
                hash: ChunkHash(hash),
            },
        )
    }

    /// `Stage::write`: a piece of step `index`, which programs chunk `dst` with a chunk that hashes to `hash`.
    /// The pieces come in order; the step runs when the chunk is complete.
    pub fn write(&mut self, index: u32, dst: u16, hash: [u8; 8], offset: u32, data: &[u8]) -> Result<(), Error> {
        let geometry = self.config.geometry;
        let chunk = geometry.chunk;
        if data.len() > self.config.limits.max_piece as usize {
            return Err(Error::PieceTooBig);
        }
        let Some(applier) = &mut self.session else {
            return Err(Error::NoSession);
        };
        let piece = &mut self.piece;
        let done = applier.steps_done();
        if index >= applier.step_count() || dst as u32 >= geometry.chunks() {
            return Err(Error::OutOfRange);
        }
        let end = offset as u64 + data.len() as u64;
        if end > chunk as u64 {
            return Err(Error::OutOfRange);
        }
        if index > done {
            return Err(Error::OutOfOrder(done));
        }
        if index < done {
            // A piece of a step that ran: the same step, and these bytes are in its chunk.
            let op = StepOp::Write {
                dst,
                data: &[],
                hash: ChunkHash(hash),
            };
            let applied = applier.apply_at(index, op, &mut self.active, &mut self.dfu, self.scratch);
            match applied {
                Ok(Applied::Repeat) => {}
                Ok(Applied::Done) => unreachable!("a step behind the next one is never run"),
                Err(e) => return Err(map_error(e, self.boot_state)),
            }
            let at = dst as u32 * chunk + offset;
            let same = &mut self.scratch[..data.len()];
            self.dfu.read(at, same).map_err(|_| Error::Flash)?;
            return if same == data { Ok(()) } else { Err(Error::StepMismatch) };
        }

        // A piece before the end of what is in is a resend (also from offset 0: a host that starts the chunk over
        // sends the same bytes); after it, a gap.
        let mut current = match *piece {
            Some(p) if (p.dst, p.hash) != (dst, hash) => return Err(Error::StepMismatch),
            Some(p) => p,
            None => Piece {
                index,
                dst,
                hash,
                filled: 0,
            },
        };
        let range = offset as usize..end as usize;
        if offset < current.filled {
            // Resent: the bytes must be the ones already in.
            return if end <= current.filled as u64 && self.chunk[range] == *data {
                Ok(())
            } else {
                Err(Error::StepMismatch)
            };
        }
        if offset > current.filled {
            return Err(Error::OutOfOrder(done));
        }
        self.chunk[range].copy_from_slice(data);
        current.filled = end as u32;
        if current.filled < chunk {
            *piece = Some(current);
            return Ok(());
        }
        *piece = None;
        let op = StepOp::Write {
            dst,
            data: &self.chunk[..chunk as usize],
            hash: ChunkHash(hash),
        };
        match applier.apply_at(index, op, &mut self.active, &mut self.dfu, self.scratch) {
            Ok(_) => Ok(()),
            Err(e) => Err(map_error(e, self.boot_state)),
        }
    }

    /// `Stage::progress`.
    pub fn progress(&self) -> Result<Progress, Error> {
        let Some(applier) = &self.session else {
            return Err(Error::NoSession);
        };
        let next_step = applier.steps_done();
        Ok(Progress {
            steps: applier.step_count(),
            next_step,
            chunk_offset: self.piece.filter(|p| p.index == next_step).map_or(0, |p| p.filled),
            plan_bytes: 0,
        })
    }

    /// `Stage::finish`: verifies the whole image against the descriptor the plan announced and stages it. A
    /// failed verification ends the session (DFU does not hold the image; the host plans again).
    pub fn finish(&mut self) -> Result<BootState, Error> {
        match &self.session {
            None if self.finished => {
                self.refresh()?;
                return match self.boot_state {
                    chunky::BootState::Staged => Ok(BootState::Staged),
                    _ => Err(Error::NoSession),
                };
            }
            None => return Err(Error::NoSession),
            Some(applier) if applier.steps_done() < applier.step_count() => return Err(Error::Incomplete),
            Some(_) => {}
        }
        let applier = self.session.take().expect("matched above");
        self.piece = None;
        let version = applier.descriptor().version;
        match applier.finish(&mut self.dfu, &mut self.backend, self.scratch) {
            Ok(_) => {
                self.finished = true;
                self.refresh()?;
                Ok(ww_state(self.boot_state))
            }
            Err(e) => {
                self.last_result = LastResult {
                    outcome: Outcome::Rejected,
                    version,
                };
                Err(match e {
                    ApplyError::Image(_) | ApplyError::WrongImage | ApplyError::BodyLength => Error::VerifyFailed,
                    e => map_error(e, self.boot_state),
                })
            }
        }
    }

    /// `Stage::abort`: drops the session. What was written to DFU stays.
    pub fn abort(&mut self) {
        if let Some(applier) = self.session.take() {
            self.last_result = LastResult {
                outcome: Outcome::Rejected,
                version: applier.descriptor().version,
            };
        }
        self.piece = None;
        self.finished = false;
    }

    fn step(&mut self, index: u32, op: StepOp<'_>) -> Result<(), Error> {
        let Some(applier) = &mut self.session else {
            return Err(Error::NoSession);
        };
        if index >= applier.step_count() {
            return Err(Error::OutOfRange);
        }
        if self.piece.is_some_and(|p| p.index == index) {
            return Err(Error::StepMismatch);
        }
        match applier.apply_at(index, op, &mut self.active, &mut self.dfu, self.scratch) {
            Ok(_) => Ok(()),
            Err(e) => Err(map_error(e, self.boot_state)),
        }
    }

    fn refresh(&mut self) -> Result<(), Error> {
        self.boot_state = self.backend.boot_state().map_err(|_| Error::Flash)?;
        Ok(())
    }

    fn error(&self, e: ApplyError) -> Error {
        map_error(e, self.boot_state)
    }
}

/// chunky's boot state as `ww_dfu` names it.
pub fn ww_state(state: chunky::BootState) -> BootState {
    match state {
        chunky::BootState::Confirmed => BootState::Confirmed,
        chunky::BootState::Reverted => BootState::Reverted,
        chunky::BootState::Staged => BootState::Staged,
        chunky::BootState::Trial => BootState::Trial,
    }
}

/// An [`ApplyError`] as `ww_dfu` names it; `state` is the boot state for [`Error::WrongState`].
pub fn map_error(e: ApplyError, state: chunky::BootState) -> Error {
    match e {
        ApplyError::NotConfirmed => Error::WrongState(ww_state(state)),
        ApplyError::Plan(_) | ApplyError::TooManySteps => Error::BadHeader,
        ApplyError::GeometryMismatch => Error::WrongGeometry,
        ApplyError::Image(image) => match image {
            ImageError::BadSignature | ImageError::WrongScheme => Error::BadSignature,
            ImageError::GeometryMismatch => Error::WrongGeometry,
            ImageError::RolledBack => Error::BelowFloor,
            ImageError::Unreadable | ImageError::Flash => Error::Flash,
            ImageError::BadMagic
            | ImageError::BadFormat
            | ImageError::Reserved
            | ImageError::WrongProduct
            | ImageError::BadLength
            | ImageError::HashMismatch => Error::BadHeader,
        },
        ApplyError::BodyLength | ApplyError::OutOfOrder | ApplyError::OutOfRange | ApplyError::BadData => {
            Error::OutOfRange
        }
        ApplyError::Ahead { expected } => Error::OutOfOrder(expected),
        ApplyError::NotSameStep => Error::StepMismatch,
        ApplyError::Incomplete => Error::Incomplete,
        ApplyError::DataMismatch => Error::HashMismatch,
        ApplyError::SourceMismatch | ApplyError::SourceUnreadable => Error::SourceMismatch,
        ApplyError::NotBlank => Error::Custom(CUSTOM_NOT_BLANK),
        ApplyError::WrongImage => Error::VerifyFailed,
        ApplyError::VerifyFailed | ApplyError::Backend | ApplyError::Flash => Error::Flash,
    }
}
