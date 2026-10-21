//! A whole update from the host library down to the flash: `chunky_ww_host` over `wire_weaver_client`, the
//! simulated USB high speed of `in_process` (512 B packets), `ww_device::Server` with `chunky_ww` on the simulated
//! B135 (STM32H725IG layout, embassy-boot swap). Delta and full image, a lost reply, DFU changed under the plan,
//! a trial that never confirms, a revert on purpose.

mod sim;

use std::time::Duration;

use chunky::chips::STM32H725IG;
use chunky::descriptor::Trust;
use chunky_server::{ImageMeta, Signer, build_image, vectors};
use chunky_ww::Limits;
use chunky_ww_host::{Event, Node, Options, Timeouts, UpdateError, flash, reboot_into_trial, seal_for_node};
use sim::{Faults, Follow, PRODUCT, SelfTest, SimB135};
use ww_dfu::{BootState, BootTarget, Outcome};

const KB: usize = 1024;

fn signer() -> Signer {
    Signer::from_seed(&[0x35; 32])
}

fn image(payload: &[u8], version: u32, signer: &Signer) -> Vec<u8> {
    let meta = ImageMeta {
        product_id: PRODUCT,
        version,
        security_counter: 0,
    };
    build_image(payload, &STM32H725IG, &meta, signer).unwrap()
}

/// Firmware of 160 KB, and a next version of it: a changed vector table, a few changed chunks, a moved one and
/// 4 KB more at the end.
fn releases() -> (Vec<u8>, Vec<u8>) {
    let old = vectors::junk(1, 160 * KB);
    let mut new = old.clone();
    new[..64].copy_from_slice(&vectors::junk(2, 64));
    new[40 * KB..42 * KB].copy_from_slice(&vectors::junk(3, 2 * KB));
    new.copy_within(10 * KB..12 * KB, 100 * KB);
    new.extend_from_slice(&vectors::junk(4, 4 * KB));
    (old, new)
}

/// Fast timeouts: the simulator answers at once, a lost reply should not hold a test for seconds.
fn options() -> Options {
    Options {
        timeouts: Some(Timeouts {
            call: Duration::from_millis(300),
            erase: Duration::from_millis(200),
            finish: Duration::from_secs(5),
            reboot: Duration::from_secs(5),
            confirm: Duration::from_secs(5),
        }),
        ..Options::default()
    }
}

fn limits(in_flight: u8) -> Limits {
    Limits {
        max_piece: 1024,
        in_flight,
        writes_in_background: false,
    }
}

/// Events seen, for checks on progress.
#[derive(Default)]
struct Seen {
    sent: Option<(u64, u64)>,
    replans: u32,
    staged: bool,
}

impl Seen {
    fn on(&mut self, event: Event<'_>) {
        match event {
            Event::Sent { done, total } => {
                assert!(done <= total);
                if let Some((before, _)) = self.sent {
                    assert!(done >= before, "progress goes forward");
                }
                self.sent = Some((done, total));
            }
            // a new plan counts from its start
            Event::Planned { .. } => self.sent = None,
            Event::Replanning(_) => self.replans += 1,
            Event::Staged => self.staged = true,
            _ => {}
        }
    }
}

#[tokio::test]
async fn delta_update_runs_and_confirms() {
    let signer = signer();
    let (old, new) = releases();
    let mut device = SimB135::start(
        "delta",
        image(&old, 1, &signer),
        signer.trust(),
        limits(4),
        Faults::default(),
    );
    assert_eq!(device.booted().await.version, 1);
    let client = device.connect().await;

    let node = Node::open(&client, None, None).await.unwrap();
    assert_eq!(node.geometry, STM32H725IG, "found by geometry id among the presets");
    assert_eq!(node.info.product_id, PRODUCT);
    let new_image = seal_for_node(&new, &node.info, &node.geometry, Default::default(), &signer).unwrap();
    drop(node);

    let mut seen = Seen::default();
    let mut follow = Follow {
        device: &mut device,
        boots: vec![],
    };
    let report = flash(client, &mut follow, &new_image, &options(), &mut |e| seen.on(e))
        .await
        .unwrap();

    assert_eq!(report.node.boot_state, BootState::Confirmed);
    assert_eq!(report.node.app_version, Some(2), "the running version + 1 by default");
    assert_eq!(report.last_result().outcome, Outcome::Confirmed);
    assert_eq!(follow.boots.len(), 1);
    assert_eq!(follow.boots[0].state, chunky::BootState::Trial);
    assert!(report.stats.copied > 50, "most chunks are copied: {:?}", report.stats);
    assert!(
        report.stats.written <= 6,
        "only what changed goes over the wire: {:?}",
        report.stats
    );
    assert_eq!(report.plans, 1);
    assert_eq!(report.counts.resends, 0);
    let (done, total) = seen.sent.unwrap();
    assert_eq!(done, total);
    assert!(seen.staged);
}

#[tokio::test]
async fn full_image_on_an_unsigned_device() {
    let (old, _) = releases();
    let other = vectors::junk(9, 200 * KB);
    let unsigned = Signer::Unsigned;
    let mut device = SimB135::start(
        "full",
        image(&old, 7, &unsigned),
        Trust::Unsigned,
        limits(1),
        Faults::default(),
    );
    device.booted().await;
    let client = device.connect().await;
    let node = Node::open(&client, None, None).await.unwrap();
    assert_eq!(node.info.seal, ww_dfu::Seal::Unsigned);
    // a key where the device checks no signature is refused before anything is sent
    let refused = seal_for_node(&other, &node.info, &node.geometry, Default::default(), &signer());
    assert!(matches!(refused, Err(UpdateError::WrongImage(_))), "{refused:?}");
    let new_image = seal_for_node(&other, &node.info, &node.geometry, Default::default(), &unsigned).unwrap();
    drop(node);

    let mut follow = Follow {
        device: &mut device,
        boots: vec![],
    };
    let report = flash(client, &mut follow, &new_image, &options(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(report.node.boot_state, BootState::Confirmed);
    assert_eq!(report.node.app_version, Some(8));
    assert_eq!(report.stats.copied, 0, "nothing in common: {:?}", report.stats);
    // 100 chunks of payload and the descriptor's
    assert_eq!(report.stats.written, 101);
}

#[tokio::test]
async fn a_lost_reply_is_sent_again() {
    let signer = signer();
    let (old, new) = releases();
    let faults = Faults {
        lose_reply_of_step: Some(5),
        ..Faults::default()
    };
    let mut device = SimB135::start("lost", image(&old, 1, &signer), signer.trust(), limits(4), faults);
    device.booted().await;
    let new_image = image(&new, 2, &signer);
    let client = device.connect().await;
    let mut follow = Follow {
        device: &mut device,
        boots: vec![],
    };
    let report = flash(client, &mut follow, &new_image, &options(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(report.node.boot_state, BootState::Confirmed);
    assert_eq!(report.node.app_version, Some(2));
    assert!(report.counts.resends > 0, "{:?}", report.counts);
    assert_eq!(report.plans, 1, "a lost reply needs no new plan");
}

#[tokio::test]
async fn dfu_changed_under_the_plan_is_planned_again() {
    let signer = signer();
    let (old, new) = releases();
    // chunk 0 changes in the new image; the plan programs it without an erase, trusting DFU blank
    let faults = Faults {
        dirty_dfu_chunk: Some(0),
        ..Faults::default()
    };
    let mut device = SimB135::start("not-blank", image(&old, 1, &signer), signer.trust(), limits(2), faults);
    device.booted().await;
    let new_image = image(&new, 2, &signer);
    let client = device.connect().await;
    let mut seen = Seen::default();
    let mut follow = Follow {
        device: &mut device,
        boots: vec![],
    };
    let report = flash(client, &mut follow, &new_image, &options(), &mut |e| seen.on(e))
        .await
        .unwrap();
    assert_eq!(report.node.boot_state, BootState::Confirmed);
    assert_eq!(report.node.app_version, Some(2));
    assert_eq!(report.plans, 2, "CUSTOM_NOT_BLANK: manifest read again, planned again");
    assert_eq!(seen.replans, 1);
    assert!(
        report.stats.erased_blocks >= 1,
        "the second plan erases the dirty block"
    );
}

#[tokio::test]
async fn a_trial_that_never_confirms_is_reverted() {
    let signer = signer();
    let (old, new) = releases();
    let faults = Faults {
        self_test: SelfTest::Crashes(Duration::from_millis(150)),
        ..Faults::default()
    };
    let mut device = SimB135::start("crash", image(&old, 1, &signer), signer.trust(), limits(4), faults);
    device.booted().await;
    let new_image = image(&new, 2, &signer);
    let client = device.connect().await;
    let mut follow = Follow {
        device: &mut device,
        boots: vec![],
    };
    let report = flash(client, &mut follow, &new_image, &options(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(report.node.boot_state, BootState::Reverted);
    assert_eq!(report.node.app_version, Some(1), "the old image runs again");
    assert_eq!(report.last_result().outcome, Outcome::Reverted);
    let states: Vec<_> = follow.boots.iter().map(|b| (b.version, b.state)).collect();
    assert_eq!(
        states,
        [(2, chunky::BootState::Trial), (1, chunky::BootState::Reverted)]
    );
}

#[tokio::test]
async fn revert_on_purpose() {
    let signer = signer();
    let (old, new) = releases();
    let faults = Faults {
        self_test: SelfTest::Passes(Duration::from_secs(600)),
        ..Faults::default()
    };
    let mut device = SimB135::start("revert", image(&old, 1, &signer), signer.trust(), limits(4), faults);
    device.booted().await;
    let client = device.connect().await;
    let mut node = Node::open(&client, None, options().timeouts).await.unwrap();
    node.stage(&image(&new, 2, &signer), &options(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(node.info.boot_state, BootState::Staged);
    drop(client);

    let mut follow = Follow {
        device: &mut device,
        boots: vec![],
    };
    let (mut node, _client) = reboot_into_trial(node, &mut follow, &mut |_| {}).await.unwrap();
    assert_eq!(node.info.boot_state, BootState::Trial);
    assert_eq!(node.info.app_version, Some(2));
    node.revert().await.unwrap();
    // refused from now on: the trial is given up
    assert!(node.confirm().await.is_err());
    node.reboot(BootTarget::App).await.unwrap();
    drop(node);
    let client = follow.reconnect_now().await;
    let node = Node::open(&client, None, None).await.unwrap();
    assert_eq!(node.info.boot_state, BootState::Reverted);
    assert_eq!(node.info.app_version, Some(1));
    assert_eq!(node.info.last_result.outcome, Outcome::Reverted);
}
