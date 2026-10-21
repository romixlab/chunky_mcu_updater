//! A host for `chunky_ww` without WireWeaver: the `ww_dfu` calls made directly on the node, over a link that
//! loses requests and replies.

use chunky::manifest::manifest_header;
use chunky_server::{Manifest, Policy, Release, plan};
use chunky_sim::{Backend, FlashModel, SimNode};
use chunky_ww::CUSTOM_NOT_BLANK;
use chunky_ww::ww_dfu::{ChunkState, Error, MAX_CHUNKS_PAGE, Slot, Target};

use super::World;

/// A link that loses a request or a reply now and then. A lost request never reaches the node; a lost reply
/// leaves the node having done the call while the host hears nothing.
pub struct Link {
    state: u64,
    /// Chance of a loss, per mille, for the request and again for the reply.
    pub loss: u64,
    /// Requests lost.
    pub lost_requests: u32,
    /// Replies lost.
    pub lost_replies: u32,
}

impl Link {
    pub fn new(seed: u64, loss: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            loss,
            lost_requests: 0,
            lost_replies: 0,
        }
    }

    /// A link that loses nothing.
    pub fn perfect() -> Self {
        Self::new(0, 0)
    }

    fn lose(&mut self) -> bool {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state % 1000 < self.loss
    }

    /// One call: `None` when the host hears no reply.
    pub fn call<T>(&mut self, f: impl FnOnce() -> T) -> Option<T> {
        if self.lose() {
            self.lost_requests += 1;
            return None;
        }
        let reply = f();
        if self.lose() {
            self.lost_replies += 1;
            return None;
        }
        Some(reply)
    }
}

/// Retries a call until a reply comes. A host gives up after a while; the node must never make it.
pub fn until_heard<T>(link: &mut Link, mut f: impl FnMut() -> T) -> T {
    for _ in 0..1000 {
        if let Some(reply) = link.call(&mut f) {
            return reply;
        }
    }
    panic!("no reply in 1000 tries");
}

/// Why a push ended without a staged image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The node failed: the flash (power cut), or a refusal the test did not expect.
    Failed(Error),
}

/// The manifest as the host reads it over `Inventory::chunks`, page by page.
pub fn read_manifest<M: FlashModel, B: Backend<M>>(
    node: &mut SimNode<'_, M, B>,
    world: &World,
    link: &mut Link,
) -> Result<Manifest, Stopped> {
    let chunks = world.geometry.chunks() as u16;
    let mut slot_states = |slot: Slot| -> Result<Vec<chunky::ChunkState>, Stopped> {
        let mut states = Vec::new();
        let mut page = [ChunkState::Blank; MAX_CHUNKS_PAGE];
        while states.len() < chunks as usize {
            let first = states.len() as u16;
            let n =
                until_heard(link, || node.chunks(slot, first, chunks - first, &mut page)).map_err(Stopped::Failed)?;
            assert!(n > 0, "an empty page before the end of the slot");
            states.extend(page[..n].iter().map(|state| match *state {
                ChunkState::Hash(hash) => chunky::ChunkState::Hash(chunky::ChunkHash(hash)),
                ChunkState::Blank => chunky::ChunkState::Blank,
                ChunkState::Unreadable => chunky::ChunkState::Unreadable,
            }));
        }
        Ok(states)
    };
    let active = slot_states(Slot::Active)?;
    let dfu = slot_states(Slot::Inactive)?;
    Ok(Manifest {
        header: manifest_header(&world.geometry, world.signer.trust().scheme()),
        active,
        dfu,
    })
}

/// One call of a plan on the wire: a step, or a piece of a WRITE.
#[derive(Clone, Debug)]
pub enum Call {
    Erase {
        index: u32,
        block: u16,
    },
    Copy {
        index: u32,
        dst: u16,
        src: u16,
        hash: [u8; 8],
    },
    Write {
        index: u32,
        dst: u16,
        hash: [u8; 8],
        offset: u32,
        data: Vec<u8>,
    },
}

impl Call {
    pub fn index(&self) -> u32 {
        match *self {
            Call::Erase { index, .. } | Call::Copy { index, .. } | Call::Write { index, .. } => index,
        }
    }

    /// Sends it.
    pub fn send<M: FlashModel, B: Backend<M>>(&self, node: &mut SimNode<'_, M, B>) -> Result<(), Error> {
        match self {
            Call::Erase { index, block } => node.erase(*index, *block),
            Call::Copy { index, dst, src, hash } => node.copy(*index, *dst, *src, *hash),
            Call::Write {
                index,
                dst,
                hash,
                offset,
                data,
            } => node.write(*index, *dst, *hash, *offset, data),
        }
    }
}

/// The calls of a plan, a WRITE cut into pieces of `max_piece`.
pub fn calls(plan: &chunky_server::Plan, max_piece: usize) -> Vec<Call> {
    let mut out = Vec::new();
    for (index, step) in plan.steps.iter().enumerate() {
        let index = index as u32;
        match step {
            chunky_server::planner::PlanStep::Erase { block } => out.push(Call::Erase { index, block: *block }),
            chunky_server::planner::PlanStep::Copy { dst, src, hash } => out.push(Call::Copy {
                index,
                dst: *dst,
                src: *src,
                hash: hash.0,
            }),
            chunky_server::planner::PlanStep::Write { dst, hash, data } => {
                for (i, piece) in data.chunks(max_piece).enumerate() {
                    out.push(Call::Write {
                        index,
                        dst: *dst,
                        hash: hash.0,
                        offset: (i * max_piece) as u32,
                        data: piece.to_vec(),
                    });
                }
            }
        }
    }
    out
}

/// What a push did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pushed {
    /// Calls sent, resends included.
    pub calls: u32,
    /// Times the host started over with a new manifest and plan.
    pub replans: u32,
    /// Times `begin` answered with a resumed session.
    pub resumed: u32,
}

/// What the host does with a device: read its manifest, plan, begin, send every call with up to `in_flight`
/// outstanding, finish. A lost reply is a resend; a call the node puts elsewhere (`OutOfOrder`) moves the host
/// there; a destination that is not blank makes the host plan again.
pub fn push<M: FlashModel, B: Backend<M>>(
    node: &mut SimNode<'_, M, B>,
    world: &World,
    release: &Release,
    link: &mut Link,
    pushed: &mut Pushed,
) -> Result<(), Stopped> {
    let limits = node.limits();
    'replan: loop {
        let manifest = read_manifest::<M, B>(node, world, link)?;
        let plan = plan(&world.geometry, &manifest, release, &world.store, &Policy::default()).expect("a plan");
        let encoded = plan.encode();
        let header = &encoded[..chunky::PLAN_HEADER_LEN];
        let session = loop {
            match until_heard(link, || node.begin(Target::Live, header)) {
                Ok(session) => break session,
                Err(Error::Busy) => until_heard(link, || node.abort()),
                Err(e) => return Err(Stopped::Failed(e)),
            }
        };
        assert_eq!(session.steps as usize, plan.steps.len());
        if session.resumed {
            pushed.resumed += 1;
        }
        let calls = calls(&plan, limits.max_piece as usize);
        let first_of = |step: u32| calls.iter().position(|c| c.index() == step).unwrap_or(calls.len());
        let mut at = first_of(session.resume_from);
        while at < calls.len() {
            // Up to `in_flight` calls go out; the node runs those that arrive, in order.
            let batch = &calls[at..(at + limits.in_flight as usize).min(calls.len())];
            let replies: Vec<Option<Result<(), Error>>> = batch
                .iter()
                .map(|call| {
                    pushed.calls += 1;
                    link.call(|| call.send::<M, B>(node))
                })
                .collect();
            let mut next = at + batch.len();
            for (i, reply) in replies.iter().enumerate() {
                match reply {
                    Some(Ok(())) => continue,
                    // No reply: send again from here.
                    None => next = at + i,
                    Some(Err(Error::OutOfOrder(step))) => next = first_of(*step),
                    Some(Err(Error::Custom(CUSTOM_NOT_BLANK))) => {
                        pushed.replans += 1;
                        until_heard(link, || node.abort());
                        continue 'replan;
                    }
                    Some(Err(e)) => return Err(Stopped::Failed(*e)),
                }
                break;
            }
            at = next;
        }
        return match until_heard(link, || node.finish()) {
            Ok(_) => Ok(()),
            Err(e) => Err(Stopped::Failed(e)),
        };
    }
}

/// The application serves `ww_dfu` with `chunky_ww`; the host reaches it over `link`. A node is made at every
/// boot, as the firmware makes it at start.
pub struct OverWw {
    pub link: Link,
    pub limits: chunky_ww::Limits,
    pub pushed: Pushed,
}

impl OverWw {
    pub fn new(link: Link, limits: chunky_ww::Limits) -> Self {
        Self {
            link,
            limits,
            pushed: Pushed::default(),
        }
    }
}

/// Limits of a node on a fast link: pieces of `max_piece`, `in_flight` outstanding.
pub fn limits(max_piece: u32, in_flight: u8) -> chunky_ww::Limits {
    chunky_ww::Limits {
        max_piece,
        in_flight,
        writes_in_background: false,
    }
}

impl<M: FlashModel, B: Backend<M>> super::Client<M, B> for OverWw {
    fn confirm(&mut self, device: &chunky_sim::Device<M, B>) -> bool {
        let mut word = [0; 32];
        let mut buf = vec![0; 2 * device.geometry.chunk as usize];
        let Ok(mut node) = device.node(self.limits, &mut word[..M::WRITE_SIZE], &mut buf) else {
            return false;
        };
        until_heard(&mut self.link, || node.confirm()).is_ok()
    }

    fn session(&mut self, device: &chunky_sim::Device<M, B>, world: &World) -> Option<usize> {
        let mut word = [0; 32];
        let mut buf = vec![0; 2 * device.geometry.chunk as usize];
        let mut node = match device.node(self.limits, &mut word[..M::WRITE_SIZE], &mut buf) {
            Ok(node) => node,
            Err(e) => {
                assert!(device.flash.borrow().is_dead(), "no node without a power cut: {e:?}");
                return None;
            }
        };
        match push::<M, B>(&mut node, world, &world.new_release, &mut self.link, &mut self.pushed) {
            Ok(()) => Some(0),
            Err(stopped) => {
                assert!(
                    device.flash.borrow().is_dead(),
                    "the push stopped without a power cut: {stopped:?}"
                );
                None
            }
        }
    }
}
