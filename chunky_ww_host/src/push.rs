//! Sending a plan's steps: one call per step (a WRITE in pieces), up to `in_flight` outstanding, every call with a
//! timeout that covers the erases queued before it.

use std::collections::VecDeque;
use std::time::Duration;

use chunky_server::Plan;
use chunky_server::planner::PlanStep;
use wire_weaver::prelude::MaxLen;
use wire_weaver_client::{CallError, Error as ClientError, PendingCall};
use ww_dfu::Error;

use crate::client::Stage;
use crate::timing::Timeouts;

/// One call of a plan on the wire: a step, or a piece of a WRITE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call<'p> {
    /// `Stage::erase`.
    Erase {
        /// Step index.
        index: u32,
        /// Erase block of DFU.
        block: u16,
    },
    /// `Stage::copy`.
    Copy {
        /// Step index.
        index: u32,
        /// Chunk of DFU.
        dst: u16,
        /// Chunk of ACTIVE.
        src: u16,
        /// Hash the source must have.
        hash: [u8; 8],
    },
    /// `Stage::write`, one piece.
    Write {
        /// Step index.
        index: u32,
        /// Chunk of DFU.
        dst: u16,
        /// Hash of the whole chunk.
        hash: [u8; 8],
        /// Where the piece goes in the chunk.
        offset: u32,
        /// The piece.
        data: &'p [u8],
    },
}

impl Call<'_> {
    /// The step this call belongs to.
    pub fn index(&self) -> u32 {
        match *self {
            Call::Erase { index, .. } | Call::Copy { index, .. } | Call::Write { index, .. } => index,
        }
    }

    /// Bytes of the step that this call moves, for progress: the step's own fields once, a WRITE's data.
    pub fn weight(&self) -> u64 {
        match self {
            Call::Erase { .. } => 3,
            Call::Copy { .. } => 13,
            Call::Write { offset: 0, data, .. } => 13 + data.len() as u64,
            Call::Write { data, .. } => data.len() as u64,
        }
    }
}

/// The calls of `plan`, every WRITE cut into pieces of at most `max_piece`.
pub fn calls(plan: &Plan, max_piece: usize) -> Vec<Call<'_>> {
    let mut out = Vec::new();
    for (index, step) in plan.steps.iter().enumerate() {
        let index = index as u32;
        match step {
            PlanStep::Erase { block } => out.push(Call::Erase { index, block: *block }),
            PlanStep::Copy { dst, src, hash } => out.push(Call::Copy {
                index,
                dst: *dst,
                src: *src,
                hash: hash.0,
            }),
            PlanStep::Write { dst, hash, data } => {
                for (i, piece) in data.chunks(max_piece.max(1)).enumerate() {
                    out.push(Call::Write {
                        index,
                        dst: *dst,
                        hash: hash.0,
                        offset: (i * max_piece) as u32,
                        data: piece,
                    });
                }
            }
        }
    }
    out
}

/// Why a push stopped before every step was in.
#[derive(Debug)]
pub(crate) enum Stopped {
    /// The node answered something that needs a new manifest and plan (`CUSTOM_NOT_BLANK`, `SourceMismatch`,
    /// `Ecc`).
    Replan(Error),
    /// The node refused for good.
    Device(Error),
    /// Lost replies, more than the host retries.
    Lost(u32),
    /// The link failed.
    Client(ClientError),
    /// The device has no `Stage`.
    Unimplemented,
}

/// What a push did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Calls sent, resends included.
    pub calls: u32,
    /// Calls sent again: a lost reply, a node that asked for another step, a chunk that arrived damaged.
    pub resends: u32,
}

type Reply = PendingCall<(), CallError<Error>>;

/// Sends `calls` from `start` on, `in_flight` at most outstanding. `on_done` hears every call the node answered
/// `Ok`, with its position.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn push(
    stage: &mut Stage,
    calls: &[Call<'_>],
    start: usize,
    in_flight: usize,
    timeouts: &Timeouts,
    max_lost: u32,
    counts: &mut Counts,
    on_done: &mut dyn FnMut(usize),
) -> Result<(), Stopped> {
    let first_of = |step: u32, offset: u32| {
        calls
            .iter()
            .position(|c| match *c {
                Call::Write { index, offset: o, .. } => index == step && o == offset,
                ref c => c.index() == step && offset == 0,
            })
            .or_else(|| calls.iter().position(|c| c.index() == step))
            .unwrap_or(calls.len())
    };
    let in_flight = in_flight.max(1);
    let mut at = start;
    let mut lost = 0;
    // (position, the erases queued up to and including it, the reply)
    let mut pending: VecDeque<(usize, Reply)> = VecDeque::new();
    while at < calls.len() || !pending.is_empty() {
        while pending.len() < in_flight && at < calls.len() {
            // the node runs calls in order: this one waits for every erase ahead of it in the window
            let erases = pending
                .iter()
                .map(|(i, _)| &calls[*i])
                .chain([&calls[at]])
                .filter(|c| matches!(c, Call::Erase { .. }))
                .count() as u32;
            let timeout = timeouts.call + timeouts.erase * erases;
            let reply = send(stage, &calls[at], timeout).await.map_err(|e| match e {
                CallError::Client(e) => Stopped::Client(e),
                CallError::Unimplemented { .. } => Stopped::Unimplemented,
                CallError::Device(e) => Stopped::Device(e),
            })?;
            counts.calls += 1;
            pending.push_back((at, reply));
            at += 1;
        }
        let Some((i, reply)) = pending.pop_front() else {
            break;
        };
        let error = match reply.reply().await {
            Ok(()) => {
                on_done(i);
                continue;
            }
            Err(e) => e,
        };
        // Whatever comes after a failed call is answered by the node as it sees fit (out of order, mostly): let it
        // all arrive, then go on from where the node stands.
        let behind = pending.len() as u32;
        for (_, reply) in pending.drain(..) {
            _ = reply.reply().await;
        }
        counts.resends += behind;
        let call = &calls[i];
        at = match error {
            CallError::Device(Error::OutOfOrder(step)) => first_of(step, 0),
            // the chunk arrived damaged: all of it again
            CallError::Device(Error::HashMismatch) => {
                lost += 1;
                first_of(call.index(), 0)
            }
            CallError::Device(
                e @ (Error::Custom(chunky_ww::CUSTOM_NOT_BLANK) | Error::SourceMismatch | Error::Ecc),
            ) => return Err(Stopped::Replan(e)),
            CallError::Device(e) => return Err(Stopped::Device(e)),
            CallError::Unimplemented { .. } => return Err(Stopped::Unimplemented),
            CallError::Client(ClientError::Timeout) => {
                lost += 1;
                if lost > max_lost {
                    return Err(Stopped::Lost(lost));
                }
                // where the node is: the next step and how much of its chunk is in
                let progress = loop {
                    match stage.progress().with_timeout(timeouts.call).call().await {
                        Ok(progress) => break progress,
                        Err(CallError::Client(ClientError::Timeout)) if lost < max_lost => lost += 1,
                        Err(CallError::Device(e)) => return Err(Stopped::Device(e)),
                        Err(CallError::Client(e)) => return Err(Stopped::Client(e)),
                        Err(CallError::Unimplemented { .. }) => return Err(Stopped::Unimplemented),
                    }
                };
                first_of(progress.next_step, progress.chunk_offset)
            }
            CallError::Client(e) => return Err(Stopped::Client(e)),
        };
        if lost > max_lost {
            return Err(Stopped::Lost(lost));
        }
        counts.resends += 1;
        tracing::debug!("call {i} (step {}): {error:?}, going on from call {at}", call.index());
    }
    Ok(())
}

async fn send(stage: &mut Stage, call: &Call<'_>, timeout: Duration) -> Result<Reply, CallError<Error>> {
    match *call {
        Call::Erase { index, block } => stage.erase(index, block).with_timeout(timeout).send().await,
        Call::Copy { index, dst, src, hash } => stage.copy(index, dst, src, hash).with_timeout(timeout).send().await,
        Call::Write {
            index,
            dst,
            hash,
            offset,
            data,
        } => {
            let data = MaxLen::new(data.to_vec()).map_err(|_| CallError::Device(Error::PieceTooBig))?;
            stage
                .write(index, dst, hash, offset, data)
                .with_timeout(timeout)
                .send()
                .await
        }
    }
}
