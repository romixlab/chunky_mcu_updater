//! The apply engine: runs a plan against DFU, trusting none of it.
//!
//! What a plan can and cannot do, whoever sent it:
//!
//! - Nothing is touched before the target's descriptor passes: seal (signature) over its exact bytes first, then
//!   scheme, product, geometry, length and floor. The header's own fields (geometry, step count, body length)
//!   are outside the seal: they can only make the plan refused, and are bounded by the geometry.
//! - A step is exactly one encoded step, a WRITE's data exactly one chunk, and the steps add up to exactly the
//!   step count and the body length of the header.
//! - It writes DFU only, and only the part an image occupies. ACTIVE is handed in as a read-only flash.
//! - Steps come in ascending order, an erase before the programs of its block. That bounds the work (each
//!   block is erased at most once, each chunk programmed at most once) and makes a plan with a COPY whose
//!   source was already overwritten impossible to express: sources are in ACTIVE, which no step can write.
//! - A chunk is programmed only when it is blank, so no write unit is ever programmed twice.
//! - A COPY source and WRITE data must have the hash the step names, and the chunk is read back afterwards.
//! - The image is switched to only after the whole of it verifies against the descriptor announced up front.
//!
//! A step that breaks a rule stops the plan with a named error. What is already in DFU stays: the next manifest
//! tells the server where things stand.

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

use crate::backend::BootBackend;
use crate::descriptor::{Expect, ImageDescriptor, ImageError, Trust, open_descriptor, verify_image};
use crate::ecc::{EccError, ReadFault, try_read};
use crate::geometry::FlashGeometry;
use crate::hash::{ChunkHash, HASH_LEN, is_blank};
use crate::plan::{
    PLAN_HEADER_LEN, PlanError, Step, StepView, read_header_fixed, read_step, step_len, step_len_from_head,
};

/// Why a plan was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The device is on a trial boot or has an update staged: DFU must not be written now.
    NotConfirmed,
    /// The plan is malformed.
    Plan(PlanError),
    /// The plan was made for another flash layout (`GEOMETRY_MISMATCH`).
    GeometryMismatch,
    /// The target image is refused.
    Image(ImageError),
    /// More steps than the header announced, or more than any plan for this geometry needs.
    TooManySteps,
    /// A body longer than any plan for this geometry needs, or steps that do not add up to the body length the
    /// header announced.
    BodyLength,
    /// Fewer steps than the header announced.
    Incomplete,
    /// A step is not after the one before it.
    OutOfOrder,
    /// [`Applier::apply_at`]: the step index is ahead of the next step, which is `expected`.
    Ahead {
        /// Index of the next step.
        expected: u32,
    },
    /// [`Applier::apply_at`]: a step index already run comes with another step, or too far back to tell.
    NotSameStep,
    /// A step names a chunk or block outside the image.
    OutOfRange,
    /// The bytes handed in are not exactly one step, or a WRITE's data is not exactly one chunk.
    BadData,
    /// The data of a WRITE does not have the hash the step names.
    DataMismatch,
    /// The source of a COPY does not have the hash the step names.
    SourceMismatch,
    /// The source of a COPY faults on ECC.
    SourceUnreadable,
    /// The chunk to program is not blank.
    NotBlank,
    /// The chunk does not read back as programmed.
    VerifyFailed,
    /// The staged image is not the one the plan announced.
    WrongImage,
    /// The boot backend failed.
    Backend,
    /// The flash failed.
    Flash,
}

/// Runs one plan. Create it with [`begin`](Self::begin), feed every step, then [`finish`](Self::finish).
pub struct Applier<'g> {
    geometry: &'g FlashGeometry,
    trust: Trust,
    expect: Expect,
    descriptor: ImageDescriptor,
    /// The header as received: what makes a session the same (SPEC §9.2).
    header: [u8; PLAN_HEADER_LEN],
    /// Lowest position the next step may have: twice the chunk index, plus one for a program.
    next_position: u32,
    step_count: u32,
    steps_left: u32,
    body_left: u32,
    /// The last steps run, by index modulo [`RESEND_WINDOW`].
    recent: [Option<StepId>; RESEND_WINDOW],
}

impl<'g> Applier<'g> {
    /// Checks the plan header and the target's descriptor. Writes nothing.
    pub fn begin<B: BootBackend>(
        geometry: &'g FlashGeometry,
        header: &[u8; PLAN_HEADER_LEN],
        trust: &Trust,
        product_id: u32,
        floor: u32,
        backend: &mut B,
    ) -> Result<Self, ApplyError> {
        let bytes = *header;
        let header = read_header_fixed(header).map_err(ApplyError::Plan)?;
        if header.geometry_id != geometry.id() {
            return Err(ApplyError::GeometryMismatch);
        }
        if header.step_count > geometry.blocks() + geometry.chunks() {
            return Err(ApplyError::TooManySteps);
        }
        if header.body_len as u64 > max_body_len(geometry) {
            return Err(ApplyError::BodyLength);
        }
        let expect = Expect {
            product_id,
            geometry_id: geometry.id(),
            image_len: geometry.image_len(),
            floor,
        };
        let descriptor = open_descriptor(&header.descriptor, trust, &expect).map_err(ApplyError::Image)?;
        if !backend.boot_state().map_err(|_| ApplyError::Backend)?.may_stage() {
            return Err(ApplyError::NotConfirmed);
        }
        Ok(Self {
            geometry,
            trust: *trust,
            expect,
            descriptor,
            header: bytes,
            next_position: 0,
            step_count: header.step_count,
            steps_left: header.step_count,
            body_left: header.body_len,
            recent: [None; RESEND_WINDOW],
        })
    }

    /// Bytes of the step that starts with `head`, for a receiver that frames steps out of a stream: `Ok(None)`
    /// until `head` holds the step's head ([`MAX_STEP_HEAD_LEN`](crate::plan::MAX_STEP_HEAD_LEN) bytes always
    /// do). Refuses a WRITE whose data is not one chunk before a byte of it needs a buffer.
    pub fn step_len(&self, head: &[u8]) -> Result<Option<usize>, ApplyError> {
        let Some(len) = step_len_from_head(head).map_err(ApplyError::Plan)? else {
            return Ok(None);
        };
        if head[0] == Step::WRITE_TAG && len != write_len(self.geometry) {
            return Err(ApplyError::BadData);
        }
        Ok(Some(len))
    }

    /// Steps the plan still owes.
    pub fn steps_left(&self) -> u32 {
        self.steps_left
    }

    /// Runs one step: `step` holds exactly one encoded step, a WRITE with its data. `buf` is scratch of one
    /// chunk.
    pub fn step<A, D>(&mut self, step: &[u8], active: &mut A, dfu: &mut D, buf: &mut [u8]) -> Result<(), ApplyError>
    where
        A: ReadNorFlash,
        A::Error: EccError,
        D: NorFlash,
        D::Error: EccError,
    {
        if self.steps_left == 0 {
            return Err(ApplyError::TooManySteps);
        }
        let view = read_step(step).map_err(ApplyError::Plan)?;
        if step_len(&view) != step.len() {
            return Err(ApplyError::BadData);
        }
        let op = match view {
            StepView::Erase(e) => StepOp::Erase { block: e.block() },
            StepView::Copy(c) => StepOp::Copy {
                dst: c.dst(),
                src: c.src(),
                hash: ChunkHash(c.hash()),
            },
            StepView::Write(w) => StepOp::Write {
                dst: w.dst(),
                data: w.data().as_bytes(),
                hash: ChunkHash(w.hash()),
            },
        };
        self.run(op, active, dfu, buf)
    }

    /// Runs the next step, delivered with its fields read (a carrier with one typed message per step). The
    /// rules are those of [`step`](Self::step): `op` counts as the step's encoded bytes against the body length.
    pub fn apply<A, D>(&mut self, op: StepOp<'_>, active: &mut A, dfu: &mut D, buf: &mut [u8]) -> Result<(), ApplyError>
    where
        A: ReadNorFlash,
        A::Error: EccError,
        D: NorFlash,
        D::Error: EccError,
    {
        if self.steps_left == 0 {
            return Err(ApplyError::TooManySteps);
        }
        self.run(op, active, dfu, buf)
    }

    /// Runs step `index` of the plan (APPLY-2), for a carrier whose replies can be lost, so the same step may
    /// come again.
    ///
    /// The next step is run as [`apply`](Self::apply) runs it. A step ahead of it is
    /// [`ApplyError::Ahead`]. A step behind it is a resend: when it is one of the last [`RESEND_WINDOW`] steps
    /// run and exactly the same step, and (COPY, WRITE) its chunk still has the step's hash, the answer is
    /// [`Applied::Repeat`] and no flash operation is made; anything else is [`ApplyError::NotSameStep`] (or
    /// [`ApplyError::VerifyFailed`] when the chunk changed since).
    pub fn apply_at<A, D>(
        &mut self,
        index: u32,
        op: StepOp<'_>,
        active: &mut A,
        dfu: &mut D,
        buf: &mut [u8],
    ) -> Result<Applied, ApplyError>
    where
        A: ReadNorFlash,
        A::Error: EccError,
        D: NorFlash,
        D::Error: EccError,
    {
        let done = self.steps_done();
        if index > done {
            return Err(ApplyError::Ahead { expected: done });
        }
        if index == done {
            return self.apply(op, active, dfu, buf).map(|()| Applied::Done);
        }
        let seen = self.recent[index as usize % RESEND_WINDOW];
        if done - index > RESEND_WINDOW as u32 || seen != Some(StepId::of(index, &op)) {
            return Err(ApplyError::NotSameStep);
        }
        match op {
            StepOp::Erase { .. } => {}
            StepOp::Copy { dst, hash, .. } | StepOp::Write { dst, hash, .. } => {
                let chunk = self.geometry.chunk as usize;
                assert!(buf.len() >= chunk);
                read_back(dfu, dst as u32 * self.geometry.chunk, &mut buf[..chunk], hash)?;
            }
        }
        Ok(Applied::Repeat)
    }

    /// Steps run so far: the index of the next one.
    pub fn steps_done(&self) -> u32 {
        self.step_count - self.steps_left
    }

    /// Steps of the plan, from its header.
    pub fn step_count(&self) -> u32 {
        self.step_count
    }

    /// True when `header` is byte for byte the header this plan began with: the same session (SPEC §9.2).
    pub fn is_plan(&self, header: &[u8]) -> bool {
        header == self.header.as_slice()
    }

    /// The image the plan announced.
    pub fn descriptor(&self) -> &ImageDescriptor {
        &self.descriptor
    }

    fn run<A, D>(&mut self, op: StepOp<'_>, active: &mut A, dfu: &mut D, buf: &mut [u8]) -> Result<(), ApplyError>
    where
        A: ReadNorFlash,
        A::Error: EccError,
        D: NorFlash,
        D::Error: EccError,
    {
        let geometry = self.geometry;
        let chunk = geometry.chunk as usize;
        assert!(buf.len() >= chunk);
        let buf = &mut buf[..chunk];

        let len = match op {
            StepOp::Erase { .. } => 1 + Step::ERASE_HEAD_LEN,
            StepOp::Copy { .. } => 1 + Step::COPY_HEAD_LEN,
            StepOp::Write { data, .. } => {
                if data.len() != chunk {
                    return Err(ApplyError::BadData);
                }
                write_len(geometry)
            }
        };
        if len as u64 > self.body_left as u64 {
            return Err(ApplyError::BodyLength);
        }
        let position = match op {
            StepOp::Erase { block } => {
                if block as u32 >= geometry.blocks() {
                    return Err(ApplyError::OutOfRange);
                }
                block as u32 * geometry.chunks_per_block() * 2
            }
            StepOp::Copy { dst, src, .. } => {
                if dst as u32 >= geometry.chunks() || src as u32 >= geometry.chunks() {
                    return Err(ApplyError::OutOfRange);
                }
                dst as u32 * 2 + 1
            }
            StepOp::Write { dst, .. } => {
                if dst as u32 >= geometry.chunks() {
                    return Err(ApplyError::OutOfRange);
                }
                dst as u32 * 2 + 1
            }
        };
        if position < self.next_position {
            return Err(ApplyError::OutOfOrder);
        }

        match op {
            StepOp::Erase { block } => {
                let from = block as u32 * geometry.erase;
                dfu.erase(from, from + geometry.erase).map_err(|_| ApplyError::Flash)?;
            }
            StepOp::Copy { dst, src, hash } => {
                let at = dst as u32 * geometry.chunk;
                require_blank(dfu, at, buf)?;
                match try_read(active, src as u32 * geometry.chunk, buf) {
                    Ok(()) => {}
                    Err(ReadFault::Ecc) => return Err(ApplyError::SourceUnreadable),
                    Err(ReadFault::Flash(_)) => return Err(ApplyError::Flash),
                }
                if ChunkHash::of(buf) != hash {
                    return Err(ApplyError::SourceMismatch);
                }
                dfu.write(at, buf).map_err(|_| ApplyError::Flash)?;
                read_back(dfu, at, buf, hash)?;
            }
            StepOp::Write { dst, data, hash } => {
                if ChunkHash::of(data) != hash {
                    return Err(ApplyError::DataMismatch);
                }
                let at = dst as u32 * geometry.chunk;
                require_blank(dfu, at, buf)?;
                dfu.write(at, data).map_err(|_| ApplyError::Flash)?;
                read_back(dfu, at, buf, hash)?;
            }
        }
        let index = self.steps_done();
        self.recent[index as usize % RESEND_WINDOW] = Some(StepId::of(index, &op));
        self.next_position = position + 1;
        self.steps_left -= 1;
        self.body_left -= len as u32;
        Ok(())
    }

    /// Verifies the whole staged image and tells the backend to switch to it at the next reset.
    pub fn finish<D, B>(self, dfu: &mut D, backend: &mut B, buf: &mut [u8]) -> Result<ImageDescriptor, ApplyError>
    where
        D: NorFlash,
        D::Error: EccError,
        B: BootBackend,
    {
        if self.steps_left != 0 {
            return Err(ApplyError::Incomplete);
        }
        if self.body_left != 0 {
            return Err(ApplyError::BodyLength);
        }
        let descriptor = verify_image(dfu, &self.trust, &self.expect, buf).map_err(ApplyError::Image)?;
        if descriptor != self.descriptor {
            return Err(ApplyError::WrongImage);
        }
        backend.mark_staged().map_err(|_| ApplyError::Backend)?;
        Ok(descriptor)
    }
}

/// One step with its fields read: what [`Applier::apply`] takes from a carrier that delivers steps as typed
/// messages. Untrusted like the encoded form; the same rules apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOp<'a> {
    /// Erase erase block `block` of DFU.
    Erase {
        /// Erase block of DFU.
        block: u16,
    },
    /// Program chunk `dst` of DFU with chunk `src` of ACTIVE, which must have `hash`.
    Copy {
        /// Chunk of DFU.
        dst: u16,
        /// Chunk of ACTIVE.
        src: u16,
        /// Hash of the source chunk.
        hash: ChunkHash,
    },
    /// Program chunk `dst` of DFU with `data`, exactly one chunk with `hash`.
    Write {
        /// Chunk of DFU.
        dst: u16,
        /// One chunk.
        data: &'a [u8],
        /// Hash of `data`.
        hash: ChunkHash,
    },
}

/// How many of the last steps run [`Applier::apply_at`] recognises when they come again.
pub const RESEND_WINDOW: usize = 8;

/// What [`Applier::apply_at`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The step was run.
    Done,
    /// The step had been run already (its reply was lost); nothing was done.
    Repeat,
}

/// What makes a step the same step: its index and its fields (a WRITE's data by its hash).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StepId {
    index: u32,
    op: u8,
    dst: u16,
    src: u16,
    hash: [u8; HASH_LEN],
}

impl StepId {
    fn of(index: u32, op: &StepOp<'_>) -> Self {
        let (op, dst, src, hash) = match *op {
            StepOp::Erase { block } => (Step::ERASE_TAG, block, 0, [0; HASH_LEN]),
            StepOp::Copy { dst, src, hash } => (Step::COPY_TAG, dst, src, hash.0),
            StepOp::Write { dst, hash, .. } => (Step::WRITE_TAG, dst, 0, hash.0),
        };
        Self {
            index,
            op,
            dst,
            src,
            hash,
        }
    }
}

/// Bytes of a WRITE of one chunk.
fn write_len(geometry: &FlashGeometry) -> usize {
    1 + Step::WRITE_HEAD_LEN + geometry.chunk as usize
}

/// The longest body a plan for `geometry` can need: every block erased, every chunk written.
fn max_body_len(geometry: &FlashGeometry) -> u64 {
    geometry.blocks() as u64 * (1 + Step::ERASE_HEAD_LEN) as u64 + geometry.chunks() as u64 * write_len(geometry) as u64
}

fn require_blank<D>(dfu: &mut D, at: u32, buf: &mut [u8]) -> Result<(), ApplyError>
where
    D: NorFlash,
    D::Error: EccError,
{
    match try_read(dfu, at, buf) {
        Ok(()) if is_blank(buf) => Ok(()),
        Ok(()) | Err(ReadFault::Ecc) => Err(ApplyError::NotBlank),
        Err(ReadFault::Flash(_)) => Err(ApplyError::Flash),
    }
}

fn read_back<D>(dfu: &mut D, at: u32, buf: &mut [u8], hash: ChunkHash) -> Result<(), ApplyError>
where
    D: NorFlash,
    D::Error: EccError,
{
    match try_read(dfu, at, buf) {
        Ok(()) if ChunkHash::of(buf) == hash => Ok(()),
        Ok(()) | Err(ReadFault::Ecc) => Err(ApplyError::VerifyFailed),
        Err(ReadFault::Flash(_)) => Err(ApplyError::Flash),
    }
}
