//! `chunky_ww` on the simulator: the `ww_dfu` calls a host makes, on the B135 layout (STM32H725IG: 2 KB chunks,
//! 128 KB sectors, embassy-boot swap), over a link that loses requests and replies, with power cut anywhere.

mod common;

use chunky::chips::STM32H725IG;
use chunky::plan::PLAN_HEADER_LEN;
use chunky::{BootState as ChunkyState, RESEND_WINDOW};
use chunky_server::{Manifest, Plan, Policy, Release, plan};
use chunky_sim::{Device, H7, H7Mini, SIM_PRODUCT, SimNode, Swap};
use chunky_ww::ww_dfu::{
    self, BootState, ChunkState, Error, MAX_CHUNKS_PAGE, Outcome, Profile, Role, Slot, SlotSwitch, Target,
};
use chunky_ww::{CUSTOM_NOT_BLANK, Limits};
use common::ww::{Call, Link, OverWw, Pushed, calls, limits, push, read_manifest};
use common::{App, World, run_update_by, signer};

/// Ways each cut is torn. `CHUNKY_SWEEP_SEEDS=500 just test` digs deeper.
fn seeds() -> u64 {
    std::env::var("CHUNKY_SWEEP_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16)
}

fn h725() -> World {
    World::with_geometry(STM32H725IG, signer())
}

/// Runs a node on `device` in a closure, as the firmware has it during one boot.
fn with_node<R>(device: &Device<H7>, limits: Limits, f: impl FnOnce(&mut SimNode<'_, H7, Swap>) -> R) -> R {
    let mut word = [0; 32];
    let mut buf = vec![0; 2 * device.geometry.chunk as usize];
    let mut node = device.node(limits, &mut word, &mut buf).expect("a node");
    f(&mut node)
}

/// A device on the old release, booted once.
fn booted(world: &World) -> Device<H7> {
    let device = world.device::<H7>();
    device.boot().expect("it boots");
    device
}

/// The plan the host makes for what the node reports.
fn plan_on(node: &mut SimNode<'_, H7, Swap>, world: &World, release: &Release) -> Plan {
    let manifest = read_manifest::<H7, Swap>(node, world, &mut Link::perfect()).expect("a manifest");
    plan(&world.geometry, &manifest, release, &world.store, &Policy::default()).expect("a plan")
}

fn header(plan: &Plan) -> Vec<u8> {
    plan.encode()[..PLAN_HEADER_LEN].to_vec()
}

/// Sends `calls` in order, each answered `Ok`.
fn send_all(node: &mut SimNode<'_, H7, Swap>, calls: &[Call]) {
    for call in calls {
        assert_eq!(call.send::<H7, Swap>(node), Ok(()), "{call:?}");
    }
}

/// The new release staged on `device` over `ww_dfu`.
fn staged(world: &World) -> Device<H7> {
    let device = booted(world);
    with_node(&device, limits(1024, 1), |node| {
        push::<H7, Swap>(
            node,
            world,
            &world.new_release,
            &mut Link::perfect(),
            &mut Pushed::default(),
        )
        .expect("staged");
        assert_eq!(node.boot_state(), Ok(BootState::Staged));
    });
    device
}

#[test]
fn node_says_who_it_is() {
    let world = h725();
    let device = booted(&world);
    with_node(&device, limits(1024, 1), |node| {
        let info = node.node();
        assert_eq!((info.role, info.profile), (Role::App, Profile::AB(SlotSwitch::Copy)));
        assert_eq!(*info.product, SIM_PRODUCT);
        assert_eq!(info.geometry_id, STM32H725IG.id());
        assert_eq!((info.product_id, info.seal), (common::PRODUCT, ww_dfu::Seal::Ed25519));
        assert_eq!(info.app_version, Some(1));
        assert_eq!((info.version_floor, info.boot_state), (1, BootState::Confirmed));
        assert_eq!(info.last_result.outcome, Outcome::Nothing);
        assert!(info.offers.boot_control && info.offers.inventory && info.offers.stage && !info.offers.stage_bulk);

        // The digest of ACTIVE is the release's: the host knows every chunk without reading them.
        assert_eq!(info.manifest_digest, None);
        let digest = node.manifest_digest(Slot::Active).unwrap();
        assert_eq!(digest, world.old_release.manifest_digest(&world.geometry));
        assert_eq!(node.node().manifest_digest, Some(digest));
        let manifest = read_manifest::<H7, Swap>(node, &world, &mut Link::perfect()).unwrap();
        assert_eq!(manifest.active_digest(), digest);
        assert_eq!(node.manifest_digest(Slot::Inactive).unwrap(), manifest.dfu_digest());
        assert_eq!(manifest, Manifest::parse(&device.manifest().unwrap()).unwrap());

        let layout = node.layout(Slot::Inactive).unwrap();
        assert_eq!((layout.chunk_size, layout.chunk_count), (2048, 128));
        assert_eq!((layout.erase_size, layout.erase_count), (128 * 1024, 2));
        let limits = node.limits();
        assert_eq!(
            (limits.max_piece, limits.in_flight, limits.live, limits.stored),
            (1024, 1, true, false)
        );
    });
}

#[test]
fn chunks_come_in_pages() {
    let world = h725();
    let device = booted(&world);
    with_node(&device, limits(1024, 1), |node| {
        let mut page = [ChunkState::Blank; 100];
        assert_eq!(node.chunks(Slot::Active, 0, 1000, &mut page), Ok(MAX_CHUNKS_PAGE));
        assert_eq!(node.chunks(Slot::Active, 0, 1000, &mut page[..10]), Ok(10));
        assert_eq!(node.chunks(Slot::Active, 126, 5, &mut page), Ok(2));
        assert_eq!(node.chunks(Slot::Active, 127, 0, &mut page), Ok(0));
        assert_eq!(node.chunks(Slot::Active, 128, 1, &mut page), Err(Error::OutOfRange));
        assert_eq!(
            node.chunks(Slot::Inactive, u16::MAX, 1, &mut page),
            Err(Error::OutOfRange)
        );
    });
}

#[test]
fn nothing_without_a_session() {
    let world = h725();
    let device = booted(&world);
    let before = device.dump(world.geometry.dfu);
    with_node(&device, limits(1024, 1), |node| {
        assert_eq!(node.erase(0, 0), Err(Error::NoSession));
        assert_eq!(node.copy(0, 0, 0, [0; 8]), Err(Error::NoSession));
        assert_eq!(node.write(0, 0, [0; 8], 0, &[1; 16]), Err(Error::NoSession));
        assert_eq!(node.progress(), Err(Error::NoSession));
        assert_eq!(node.finish(), Err(Error::NoSession));
        node.abort();
        assert_eq!(
            node.begin(Target::Stored, &[0; PLAN_HEADER_LEN]),
            Err(Error::Unsupported)
        );
        assert_eq!(node.begin(Target::Live, &[0; 10]), Err(Error::BadHeader));
        assert_eq!(
            node.begin(Target::Live, &[0; PLAN_HEADER_LEN + 1]),
            Err(Error::BadHeader)
        );
        assert_eq!(
            node.begin(Target::Live, &[0x5A; PLAN_HEADER_LEN]),
            Err(Error::BadHeader)
        );
        assert_eq!(node.node().last_result.outcome, Outcome::Rejected);
        // A plan for this geometry, refused for its image: the old release again is fine, a foreign key is not.
        let plan = plan_on(node, &world, &world.new_release);
        let mut forged = header(&plan);
        forged[PLAN_HEADER_LEN - 1] ^= 1;
        assert_eq!(node.begin(Target::Live, &forged), Err(Error::BadSignature));
    });
    assert!(device.dump(world.geometry.dfu) == before, "a refused begin wrote DFU");
}

#[test]
fn resent_steps_are_answered_and_skipped() {
    let world = h725();
    let device = booted(&world);
    with_node(&device, limits(1024, 4), |node| {
        let plan = plan_on(node, &world, &world.new_release);
        let calls = calls(&plan, 1024);
        node.begin(Target::Live, &header(&plan)).unwrap();
        let ops = || device.flash.borrow().ops();

        // The ERASE of block 0, twice: the second costs no flash operation.
        assert!(matches!(calls[0], Call::Erase { index: 0, block: 0 }));
        send_all(node, &calls[..1]);
        let before = ops();
        send_all(node, &calls[..1]);
        assert_eq!(ops(), before);

        // Ahead of its turn: the node says which step it expects.
        let ahead = calls.iter().find(|c| c.index() == 3).unwrap();
        assert_eq!(ahead.send::<H7, Swap>(node), Err(Error::OutOfOrder(1)));

        // A window of steps sent again, whole: every call is answered, nothing is written twice.
        let window: Vec<Call> = calls
            .iter()
            .take_while(|c| c.index() < 1 + RESEND_WINDOW as u32)
            .cloned()
            .collect();
        send_all(node, &window);
        let before = ops();
        send_all(node, &window[1..]);
        assert_eq!(ops(), before, "a resend touched the flash");

        // A resend that is not the same step, or too old to tell.
        let wrong = match &window[1] {
            Call::Write { index, dst, hash, .. } => Call::Copy {
                index: *index,
                dst: *dst,
                src: 0,
                hash: *hash,
            },
            Call::Copy { index, dst, src, hash } => Call::Copy {
                index: *index,
                dst: *dst,
                src: src + 1,
                hash: *hash,
            },
            other => panic!("step 1 of this plan is a program: {other:?}"),
        };
        assert_eq!(wrong.send::<H7, Swap>(node), Err(Error::StepMismatch));
        send_all(
            node,
            &calls[window.len()..calls.iter().position(|c| c.index() == 12).unwrap()],
        );
        assert_eq!(
            calls[0].send::<H7, Swap>(node),
            Err(Error::StepMismatch),
            "step 0 is out of the window"
        );

        let progress = node.progress().unwrap();
        assert_eq!((progress.next_step, progress.steps), (12, plan.steps.len() as u32));
    });
}

#[test]
fn a_write_is_put_together_from_pieces() {
    let world = h725();
    let device = booted(&world);
    with_node(&device, limits(512, 1), |node| {
        let plan = plan_on(node, &world, &world.new_release);
        let calls = calls(&plan, 512);
        node.begin(Target::Live, &header(&plan)).unwrap();
        let first = calls.iter().position(|c| matches!(c, Call::Write { .. })).unwrap();
        send_all(node, &calls[..first]);
        let Call::Write { index, dst, hash, .. } = calls[first].clone() else {
            unreachable!()
        };
        let piece = |n: usize| match &calls[first + n] {
            Call::Write { data, .. } => data.clone(),
            _ => unreachable!(),
        };

        assert_eq!(node.write(index, dst, hash, 0, &piece(0)), Ok(()));
        assert_eq!(node.progress().unwrap().chunk_offset, 512);
        // The same piece again, and the chunk sent over from its start: answered.
        assert_eq!(node.write(index, dst, hash, 0, &piece(0)), Ok(()));
        assert_eq!(node.write(index, dst, hash, 256, &piece(0)[256..]), Ok(()));
        // Other bytes where some are in, a gap, another step under this index, too much at once, past the chunk.
        assert_eq!(node.write(index, dst, hash, 0, &piece(1)), Err(Error::StepMismatch));
        assert_eq!(
            node.write(index, dst, hash, 1024, &piece(2)),
            Err(Error::OutOfOrder(index))
        );
        assert_eq!(
            node.write(index, dst + 1, hash, 512, &piece(1)),
            Err(Error::StepMismatch)
        );
        assert_eq!(node.write(index, dst, [0; 8], 512, &piece(1)), Err(Error::StepMismatch));
        assert_eq!(node.write(index, dst, hash, 512, &[0; 513]), Err(Error::PieceTooBig));
        assert_eq!(node.write(index, dst, hash, 2047, &[0; 2]), Err(Error::OutOfRange));
        assert_eq!(node.write(index, 128, hash, 512, &piece(1)), Err(Error::OutOfRange));
        assert_eq!(
            node.erase(index, 0),
            Err(Error::StepMismatch),
            "a WRITE is under way at this index"
        );

        // A whole chunk with the wrong bytes: refused when its last byte is in, and sent again from 0.
        node.write(index, dst, hash, 512, &[0x11; 512]).unwrap();
        node.write(index, dst, hash, 1024, &piece(2)).unwrap();
        assert_eq!(node.write(index, dst, hash, 1536, &piece(3)), Err(Error::HashMismatch));
        assert_eq!(node.progress().unwrap().chunk_offset, 0);
        assert_eq!(node.progress().unwrap().next_step, index);
        send_all(node, &calls[first..first + 4]);
        assert_eq!(node.progress().unwrap().next_step, index + 1);
        // And a piece of it again, after it ran: answered from the flash.
        send_all(node, &calls[first + 2..first + 3]);
        assert_eq!(node.write(index, dst, hash, 0, &[0x11; 512]), Err(Error::StepMismatch));

        send_all(node, &calls[first + 4..]);
        assert_eq!(node.finish(), Ok(BootState::Staged));
    });
    let (image, state) = device.boot_running().unwrap().unwrap();
    assert_eq!((image.version, state), (2, ChunkyState::Trial));
    assert!(device.dump(world.geometry.active) == world.new);
}

#[test]
fn begin_with_the_same_header_resumes() {
    let world = h725();
    let device = booted(&world);
    with_node(&device, limits(1024, 1), |node| {
        let plan = plan_on(node, &world, &world.new_release);
        let calls = calls(&plan, 1024);
        let header = header(&plan);
        let session = node.begin(Target::Live, &header).unwrap();
        assert_eq!((session.resumed, session.resume_from), (false, 0));
        let half = calls.iter().position(|c| c.index() == 20).unwrap();
        // Up to the middle of a WRITE: what is in of it is dropped on resume.
        let middle = half
            + calls[half..]
                .iter()
                .position(|c| matches!(c, Call::Write { offset: 1024, .. }))
                .unwrap();
        send_all(node, &calls[..middle]);
        let next = node.progress().unwrap().next_step;
        assert_eq!(node.progress().unwrap().chunk_offset, 1024);

        // The host lost its state: it asks again with the same plan.
        let session = node.begin(Target::Live, &header).unwrap();
        assert_eq!((session.resumed, session.resume_from), (true, next));
        assert_eq!(node.progress().unwrap().chunk_offset, 0);
        // Another plan while this one is open: abort first.
        let other = plan_on(node, &world, &world.old_release);
        assert_eq!(node.begin(Target::Live, &self::header(&other)), Err(Error::Busy));

        let from = calls.iter().position(|c| c.index() == next).unwrap();
        send_all(node, &calls[from..]);
        assert_eq!(node.finish(), Ok(BootState::Staged));
    });
}

#[test]
fn a_new_plan_after_abort_or_reset() {
    let world = h725();
    let device = booted(&world);
    with_node(&device, limits(1024, 1), |node| {
        let plan = plan_on(node, &world, &world.new_release);
        let calls = calls(&plan, 1024);
        node.begin(Target::Live, &header(&plan)).unwrap();
        send_all(node, &calls[..calls.len() / 2]);
        node.abort();
        assert_eq!(node.node().last_result.outcome, Outcome::Rejected);
        assert_eq!(node.erase(0, 0), Err(Error::NoSession));
    });
    // A reset: the session is gone, the flash tells where things stand, and the new plan sends less.
    device.boot().unwrap();
    with_node(&device, limits(1024, 1), |node| {
        let plan = plan_on(node, &world, &world.new_release);
        assert_eq!(node.begin(Target::Live, &header(&plan)).map(|s| s.resumed), Ok(false));
        assert!(plan.stats.written < world.new_release.chunks.len() as u32 / 2);
        send_all(node, &calls(&plan, 1024));
        assert_eq!(node.finish(), Ok(BootState::Staged));
    });
}

#[test]
fn a_destination_that_is_not_blank_is_named() {
    let world = h725();
    let device = booted(&world);
    with_node(&device, limits(1024, 1), |node| {
        // A host that believes DFU is blank (it is not: leftovers) and trusts blank chunks: no ERASE.
        let mut manifest = read_manifest::<H7, Swap>(node, &world, &mut Link::perfect()).unwrap();
        manifest.dfu.fill(chunky::ChunkState::Blank);
        let trusting = Policy {
            trust_blank: Some(true),
        };
        let plan = plan(&world.geometry, &manifest, &world.new_release, &world.store, &trusting).unwrap();
        let calls = calls(&plan, 1024);
        assert!(!matches!(calls[0], Call::Erase { .. }));
        node.begin(Target::Live, &header(&plan)).unwrap();
        let before = device.dump(world.geometry.dfu);
        // The first step is a program: refused when it runs, its last piece in.
        let pieces = calls.iter().take_while(|c| c.index() == 0).count();
        send_all(node, &calls[..pieces - 1]);
        assert_eq!(
            calls[pieces - 1].send::<H7, Swap>(node),
            Err(Error::Custom(CUSTOM_NOT_BLANK))
        );
        assert!(device.dump(world.geometry.dfu) == before);
        // What the host does then: abort, a fresh manifest, a new plan.
        node.abort();
        let mut pushed = Pushed::default();
        push::<H7, Swap>(node, &world, &world.new_release, &mut Link::perfect(), &mut pushed).unwrap();
    });
}

#[test]
fn finish_is_answered_again_and_revert_unstages() {
    let world = h725();
    let device = staged(&world);
    with_node(&device, limits(1024, 1), |node| {
        assert_eq!(node.boot_state(), Ok(BootState::Staged));
        let plan = plan_on(node, &world, &world.new_release);
        assert_eq!(
            node.begin(Target::Live, &header(&plan)),
            Err(Error::WrongState(BootState::Staged))
        );
    });
    // A new node (the firmware restarted its handler, no reset): finish has no session to answer for.
    with_node(&device, limits(1024, 1), |node| {
        assert_eq!(node.finish(), Err(Error::NoSession));
        assert_eq!(node.revert(), Ok(()));
        assert_eq!(node.boot_state(), Ok(BootState::Confirmed));
        assert_eq!(node.revert(), Err(Error::WrongState(BootState::Confirmed)));
    });
    // The staged image is withdrawn: the old one keeps running, confirmed, and DFU takes a new update.
    let (image, state) = device.boot_running().unwrap().unwrap();
    assert_eq!((image.version, state), (1, ChunkyState::Confirmed));
    assert!(device.dump(world.geometry.active) == world.old);
    with_node(&device, limits(1024, 1), |node| {
        let plan = plan_on(node, &world, &world.new_release);
        node.begin(Target::Live, &header(&plan)).unwrap();
        send_all(node, &calls(&plan, 1024));
        assert_eq!(node.finish(), Ok(BootState::Staged));
        assert_eq!(node.finish(), Ok(BootState::Staged), "a finish whose reply was lost");
    });
}

#[test]
fn revert_on_trial_and_the_floor_after_confirm() {
    let world = h725();
    // Revert: the trial image is not confirmed, the next reset brings the old one back.
    let device = staged(&world);
    let (_, state) = device.boot_running().unwrap().unwrap();
    assert_eq!(state, ChunkyState::Trial);
    with_node(&device, limits(1024, 1), |node| {
        assert_eq!(node.node().app_version, Some(2));
        let plan = plan_on(node, &world, &world.old_release);
        assert_eq!(
            node.begin(Target::Live, &header(&plan)),
            Err(Error::WrongState(BootState::Trial))
        );
        assert_eq!(node.revert(), Ok(()));
        assert_eq!(node.confirm(), Err(Error::WrongState(BootState::Trial)));
    });
    let (image, state) = device.boot_running().unwrap().unwrap();
    assert_eq!((image.version, state), (1, ChunkyState::Reverted));
    with_node(&device, limits(1024, 1), |node| {
        assert_eq!(node.node().last_result.outcome, Outcome::Reverted);
        assert_eq!(node.node().boot_state, BootState::Reverted);
    });

    // Confirm: from then on the running image's counter is the floor, before the bootloader writes it.
    let device = staged(&world);
    device.boot().unwrap();
    with_node(&device, limits(1024, 1), |node| {
        assert_eq!(node.node().version_floor, 1);
        assert_eq!(node.confirm(), Ok(()));
        let info = node.node();
        assert_eq!((info.boot_state, info.version_floor), (BootState::Confirmed, 2));
        assert_eq!(
            (info.last_result.outcome, info.last_result.version),
            (Outcome::Confirmed, 2)
        );
        assert_eq!(node.confirm(), Ok(()), "confirm on a confirmed image does nothing");
        let back = plan_on(node, &world, &world.old_release);
        assert_eq!(node.begin(Target::Live, &header(&back)), Err(Error::BelowFloor));
    });
}

#[test]
fn update_and_revert_over_a_perfect_link() {
    let world = h725();
    for app in [App::Good, App::Bad] {
        let device = world.device::<H7>();
        let mut client = OverWw::new(Link::perfect(), limits(512, 1));
        let seen = run_update_by(&device, &world, app, &[], &mut client);
        assert_eq!((seen.sessions, seen.trials), (1, 1));
        assert_eq!(client.pushed.replans, 0);
    }
}

#[test]
fn lost_requests_and_replies_cost_only_resends() {
    let world = h725();
    for (seed, in_flight, max_piece) in [(1, 1, 1024), (2, 4, 512), (3, 8, 2048), (4, 3, 700)] {
        let device = world.device::<H7>();
        let mut client = OverWw::new(Link::new(seed, 100), limits(max_piece.min(1024), in_flight));
        let seen = run_update_by(&device, &world, App::Good, &[], &mut client);
        assert_eq!((seen.sessions, seen.trials), (1, 1), "in_flight {in_flight}");
        assert!(client.link.lost_replies > 10 && client.link.lost_requests > 10);
        assert_eq!(client.pushed.replans, 0, "a lost reply never needs a new plan");
    }
}

/// A cut at every flash operation of the update over `ww_dfu` on the B135 layout, with a link that loses calls.
/// One way of tearing per cut by default (every operation of a 256 KB image on 128 KB sectors), `seeds()` / 8
/// with more.
#[test]
fn h725ig_survives_a_cut_anywhere() {
    let world = h725();
    let device = world.device::<H7>();
    let start = device.flash.borrow().ops();
    run_update_by(
        &device,
        &world,
        App::Good,
        &[],
        &mut OverWw::new(Link::new(1, 50), limits(1024, 2)),
    );
    let operations = device.flash.borrow().ops() - start;
    assert!(
        operations > 100,
        "the run is suspiciously short: {operations} operations"
    );
    let mut again = 0;
    for cut in 0..operations {
        for seed in 0..(seeds() / 8).max(1) {
            let app = if cut % 2 == 0 { App::Good } else { App::Bad };
            let device = world.device::<H7>();
            let mut client = OverWw::new(Link::new(cut ^ seed << 32, 50), limits(1024, 2));
            let seen = run_update_by(&device, &world, app, &[(cut, seed * 1_000_003 + cut)], &mut client);
            again += (seen.sessions > 1) as u32;
        }
    }
    eprintln!("{operations} operations, {again} runs with a second session");
}

/// The same on small sectors, every way of tearing: `seeds()` per cut.
#[test]
fn small_sectors_survive_a_cut_anywhere() {
    let world = World::new::<H7Mini>(2048, 3, signer());
    for app in [App::Good, App::Bad] {
        let device = world.device::<H7Mini>();
        let start = device.flash.borrow().ops();
        run_update_by(
            &device,
            &world,
            app,
            &[],
            &mut OverWw::new(Link::perfect(), limits(512, 4)),
        );
        let operations = device.flash.borrow().ops() - start;
        for cut in 0..operations {
            for seed in 0..seeds() {
                let device = world.device::<H7Mini>();
                let mut client = OverWw::new(Link::new(seed ^ cut << 20, 100), limits(512, 4));
                run_update_by(&device, &world, app, &[(cut, seed * 1_000_003 + cut)], &mut client);
            }
        }
    }
}

/// The node on the hardware bank swap backend: an update, a revert, and a staged image withdrawn.
#[test]
fn bank_backend() {
    use chunky::chips::STM32G0B1RE_BANK;
    use chunky_sim::{Bank, G0};

    let world = World::with_geometry(STM32G0B1RE_BANK, signer());
    for app in [App::Good, App::Bad] {
        let device = world.device_on::<G0, Bank>();
        let mut client = OverWw::new(Link::new(5, 100), limits(1024, 4));
        let seen = run_update_by(&device, &world, app, &[], &mut client);
        assert_eq!((seen.sessions, seen.trials), (1, 1));
    }

    let device = world.device_on::<G0, Bank>();
    device.boot().unwrap();
    let mut word = [0; 32];
    let mut buf = vec![0; 2 * world.geometry.chunk as usize];
    {
        let mut node = device.node(limits(1024, 1), &mut word[..8], &mut buf).unwrap();
        push::<G0, Bank>(
            &mut node,
            &world,
            &world.new_release,
            &mut Link::perfect(),
            &mut Pushed::default(),
        )
        .unwrap();
        assert_eq!(node.revert(), Ok(()));
        assert_eq!(node.boot_state(), Ok(BootState::Confirmed));
    }
    let (image, state) = device.boot_running().unwrap().unwrap();
    assert_eq!((image.version, state), (1, ChunkyState::Confirmed));
    let mut node = device.node(limits(1024, 1), &mut word[..8], &mut buf).unwrap();
    push::<G0, Bank>(
        &mut node,
        &world,
        &world.new_release,
        &mut Link::perfect(),
        &mut Pushed::default(),
    )
    .unwrap();
    let (image, state) = device.boot_running().unwrap().unwrap();
    assert_eq!((image.version, state), (2, ChunkyState::Trial));
}

/// A cut anywhere in the withdrawal of a staged image: the device boots the old image confirmed (withdrawn) or
/// the new one on trial (not withdrawn), on both backends, and the floor stays.
#[test]
fn withdrawing_a_staged_image_survives_a_cut() {
    use chunky::chips::STM32G0B1RE_BANK;
    use chunky_sim::{Backend, Bank, FlashModel, G0};

    fn sweep<M: FlashModel, B: Backend<M>>(world: &World) {
        for cut in 0..16u64 {
            for seed in 0..seeds() {
                let device = world.device_on::<M, B>();
                device.boot().unwrap();
                let mut word = [0; 32];
                let mut buf = vec![0; 2 * world.geometry.chunk as usize];
                {
                    let mut node = device
                        .node(limits(1024, 1), &mut word[..M::WRITE_SIZE], &mut buf)
                        .unwrap();
                    push::<M, B>(
                        &mut node,
                        world,
                        &world.new_release,
                        &mut Link::perfect(),
                        &mut Pushed::default(),
                    )
                    .unwrap();
                    device.flash.borrow_mut().cut_after(cut, seed * 7919 + cut);
                    let _ = node.revert();
                }
                let withdrawn = !device.flash.borrow().is_dead();
                device.flash.borrow_mut().disarm();
                device.flash.borrow_mut().power_on();
                let (image, state) = device.boot_running().unwrap().expect("it runs a release");
                match (image.version, state) {
                    // A cut in the swap backend's "progress invalid" word, written before "boot": the bootloader
                    // takes it for a swap begun and reverts one that never moved a page, and says so.
                    (1, ChunkyState::Confirmed | ChunkyState::Reverted) => {
                        assert!(device.dump(world.geometry.active) == world.old)
                    }
                    (2, ChunkyState::Trial) => {
                        assert!(!withdrawn, "withdrawn, yet switched");
                        assert!(device.dump(world.geometry.active) == world.new);
                    }
                    other => panic!("after a cut at {cut}: {other:?}"),
                }
                assert_eq!(device.floor(), 1);
            }
        }
    }
    sweep::<H7, Swap>(&h725());
    sweep::<G0, Bank>(&World::with_geometry(STM32G0B1RE_BANK, signer()));
}
