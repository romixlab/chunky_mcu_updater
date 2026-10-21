//! A simulated B135 for the host: `chunky_sim`'s device on the STM32H725IG layout, its app serving `ww_dfu` with
//! `chunky_ww` on `ww_device::Server`, over the simulated USB high speed (512 B packets) of
//! `wire_weaver_client::in_process`. Every reset runs the bootloader (`chunky_swap::boot` on the simulated flash)
//! and starts a new node, as the firmware does; the path stands in for the USB serial number.
//!
//! [`Faults`] make the device misbehave the ways a host must survive: a reply lost, DFU changed under the plan, a
//! trial image that never confirms.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chunky::chips::STM32H725IG;
use chunky::descriptor::Trust;
use chunky_sim::{Device, H7, SimNode, Swap};
use chunky_ww::Limits;
use chunky_ww_host::Reconnect;
use tokio::sync::mpsc;
use wire_weaver::prelude::*;
use wire_weaver::{EventWriter, MessageSink, WireWeaverAsyncApiBackend};
use wire_weaver_client::DynClient;
use wire_weaver_client::in_process::{self, TokioClock};
use ww_client_server::StreamSideband;
use ww_device::{DisconnectReason, LinkConfig};
use ww_dfu::{
    BootState, BootTarget, ChunkState, Error, MAX_CHUNKS_PAGE, NodeInfo, Progress, Session, Slot, SlotLayout,
    StageLimits, Target,
};

const MAX_MESSAGE: usize = 2048;
/// USB high speed bulk endpoints.
const USB_PACKET: usize = 512;
/// A reset and the USB enumeration after it.
const REENUMERATE: Duration = Duration::from_millis(30);

/// Product id of the simulated device.
pub const PRODUCT: u32 = 0x0B13_5001;

/// What a trial image does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfTest {
    /// Confirms itself this long after it started.
    Passes(Duration),
    /// Resets this long after it started, without a confirm (it crashed): the bootloader reverts.
    Crashes(Duration),
}

/// Ways the device misbehaves.
#[derive(Clone, Debug)]
pub struct Faults {
    /// The reply to the first call of this step is lost (the node did the step).
    pub lose_reply_of_step: Option<u32>,
    /// At the first `begin`, this chunk of DFU is programmed behind the host's back (it planned on it being blank).
    pub dirty_dfu_chunk: Option<u16>,
    /// What a trial image does.
    pub self_test: SelfTest,
}

impl Default for Faults {
    fn default() -> Self {
        Self {
            lose_reply_of_step: None,
            dirty_dfu_chunk: None,
            self_test: SelfTest::Passes(Duration::from_millis(100)),
        }
    }
}

/// A boot of the device, as the test sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boot {
    /// Version of the image that runs.
    pub version: u32,
    /// Its state.
    pub state: chunky::BootState,
}

/// A running simulated device on its own thread (the simulator is single threaded, as a firmware is), stopped
/// when dropped.
pub struct SimB135 {
    /// Where the host finds it.
    pub path: String,
    boots: mpsc::UnboundedReceiver<Boot>,
    _stop: mpsc::UnboundedSender<()>,
}

impl SimB135 {
    /// A device named `name` (unique per test) whose ACTIVE holds `image`, trusting `trust`, with `limits`.
    pub fn start(name: &str, image: Vec<u8>, trust: Trust, limits: Limits, faults: Faults) -> SimB135 {
        let path = format!("chunky/{name}");
        let faults = Arc::new(Mutex::new(faults));
        let (boots_tx, boots) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = mpsc::unbounded_channel();
        let t_path = path.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(device_thread(t_path, image, trust, limits, faults, boots_tx, stop_rx))
        });
        SimB135 {
            path,
            boots,
            _stop: stop_tx,
        }
    }

    /// Waits for the next boot.
    pub async fn booted(&mut self) -> Boot {
        tokio::time::timeout(Duration::from_secs(10), self.boots.recv())
            .await
            .expect("the device boots")
            .expect("the device thread runs")
    }

    /// Connects to the device that runs now.
    pub async fn connect(&self) -> DynClient {
        connect(&self.path, Duration::from_secs(5)).await.expect("connects")
    }
}

async fn connect(path: &str, timeout: Duration) -> Result<DynClient, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let path = path.to_string();
        match DynClient::traits_only(move |c| c.in_process_path(path.clone()))
            .connect()
            .await
        {
            Ok(device) => return Ok(device),
            Err(e) if tokio::time::Instant::now() >= deadline => return Err(format!("{e:#}")),
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

/// Reconnects the host as a real one does on USB: waits until the device has reset and enumerated again, then
/// connects by its serial number (the path). Records what booted.
pub struct Follow<'a> {
    pub device: &'a mut SimB135,
    pub boots: Vec<Boot>,
}

impl Follow<'_> {
    /// The device after its next reset.
    pub async fn reconnect_now(&mut self) -> DynClient {
        self.reconnect(Duration::from_secs(5))
            .await
            .expect("the device comes back")
    }
}

impl Reconnect for Follow<'_> {
    async fn reconnect(&mut self, timeout: Duration) -> Result<DynClient, String> {
        let boot = tokio::time::timeout(timeout, self.device.boots.recv())
            .await
            .map_err(|_| "the device did not come back".to_string())?
            .ok_or("the device thread stopped")?;
        self.boots.push(boot);
        connect(&self.device.path, timeout).await
    }
}

async fn device_thread(
    path: String,
    image: Vec<u8>,
    trust: Trust,
    limits: Limits,
    faults: Arc<Mutex<Faults>>,
    boots: mpsc::UnboundedSender<Boot>,
    mut stop: mpsc::UnboundedReceiver<()>,
) {
    // a firmware's statics: the node borrows the flash for as long as the device runs
    let device: &'static Device<H7, Swap> = Box::leak(Box::new(Device::new(STM32H725IG, trust, PRODUCT)));
    device.install(&image);
    loop {
        tokio::time::sleep(REENUMERATE).await;
        let (running, state) = device
            .boot_running()
            .expect("no power cuts here")
            .expect("an image to run");
        _ = boots.send(Boot {
            version: running.version,
            state,
        });
        let word: &'static mut [u8] = Box::leak(vec![0u8; 32].into_boxed_slice());
        let buf: &'static mut [u8] = Box::leak(vec![0u8; 2 * STM32H725IG.chunk as usize].into_boxed_slice());
        let node = device.node(limits, word, buf).expect("a node");
        let app = App {
            device,
            node,
            page: [ChunkState::Blank; MAX_CHUNKS_PAGE],
            faults: faults.clone(),
            reboot: None,
            lose_reply: false,
            begun: false,
        };
        if run_app(app, &path, &faults, &mut stop).await.is_none() {
            return;
        }
    }
}

/// The app: `chunky_ww`'s node behind the generated server.
pub struct App {
    device: &'static Device<H7, Swap>,
    node: SimNode<'static, H7, Swap>,
    page: [ChunkState; MAX_CHUNKS_PAGE],
    faults: Arc<Mutex<Faults>>,
    reboot: Option<BootTarget>,
    /// The reply of the call being handled is lost.
    lose_reply: bool,
    begun: bool,
}

impl App {
    fn step(&mut self, index: u32) {
        let mut faults = self.faults.lock().unwrap();
        if faults.lose_reply_of_step == Some(index) {
            faults.lose_reply_of_step = None;
            self.lose_reply = true;
        }
    }

    fn node_node(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<NodeInfo<'_>> {
        Ready(self.node.node())
    }

    fn boot_boot_state(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<BootState> {
        Ready(self.node.boot_state().expect("the simulated flash reads"))
    }

    fn boot_confirm(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<Result<(), Error>> {
        Ready(self.node.confirm())
    }

    fn boot_revert(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<Result<(), Error>> {
        Ready(self.node.revert())
    }

    fn boot_reboot(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        into: BootTarget,
    ) -> RpcResult<Result<(), Error>> {
        // B135: the updater is the ROM DFU, not simulated here
        if into == BootTarget::Updater {
            return Ready(Err(Error::Unsupported));
        }
        self.reboot = Some(into);
        Ready(Ok(()))
    }

    fn inventory_layout(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        slot: Slot,
    ) -> RpcResult<Result<SlotLayout, Error>> {
        Ready(self.node.layout(slot))
    }

    fn inventory_manifest_digest(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        slot: Slot,
    ) -> RpcResult<Result<[u8; 8], Error>> {
        Ready(self.node.manifest_digest(slot))
    }

    fn inventory_chunks(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        slot: Slot,
        first: u16,
        count: u16,
    ) -> RpcResult<Result<MaxLen<RefVec<'_, ChunkState>, { MAX_CHUNKS_PAGE }>, Error>> {
        Ready(match self.node.chunks(slot, first, count, &mut self.page) {
            Ok(n) => Ok(MaxLen::new(RefVec::Slice { slice: &self.page[..n] }).unwrap()),
            Err(e) => Err(e),
        })
    }

    fn stage_limits(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<StageLimits> {
        Ready(self.node.limits())
    }

    fn stage_begin(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        target: Target,
        header: MaxLen<RefVec<'_, u8>, { ww_dfu::MAX_HEADER_LEN }>,
    ) -> RpcResult<Result<Session, Error>> {
        let dirty = self.faults.lock().unwrap().dirty_dfu_chunk;
        if let (Some(chunk), false) = (dirty, self.begun) {
            let g = STM32H725IG;
            let at = (g.dfu.offset + chunk as u32 * g.chunk) as usize;
            self.device.flash.borrow_mut().load(at, &vec![0x5A; g.chunk as usize]);
        }
        self.begun = true;
        Ready(self.node.begin(target, &bytes(&header)))
    }

    fn stage_erase(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        index: u32,
        block: u16,
    ) -> RpcResult<Result<(), Error>> {
        self.step(index);
        Ready(self.node.erase(index, block))
    }

    fn stage_copy(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        index: u32,
        dst: u16,
        src: u16,
        hash: [u8; 8],
    ) -> RpcResult<Result<(), Error>> {
        self.step(index);
        Ready(self.node.copy(index, dst, src, hash))
    }

    fn stage_write(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        index: u32,
        dst: u16,
        hash: [u8; 8],
        offset: u32,
        data: MaxLen<RefVec<'_, u8>, { ww_dfu::MAX_PIECE_LEN }>,
    ) -> RpcResult<Result<(), Error>> {
        self.step(index);
        Ready(self.node.write(index, dst, hash, offset, &bytes(&data)))
    }

    fn stage_progress(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<Result<Progress, Error>> {
        Ready(self.node.progress())
    }

    fn stage_finish(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<Result<BootState, Error>> {
        Ready(self.node.finish())
    }

    fn stage_abort(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<()> {
        self.node.abort();
        Ready(())
    }

    fn stage_event(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) -> RpcResult<()> {
        Ready(())
    }

    fn close_stage_event(&mut self, _cx: &mut Context<'_, impl BlockingEventOut>) {}

    fn sideband_stage_event(
        &mut self,
        _cx: &mut Context<'_, impl BlockingEventOut>,
        _sideband: StreamSideband,
    ) -> Option<StreamSideband> {
        None
    }
}

fn bytes(v: &RefVec<'_, u8>) -> Vec<u8> {
    v.iter().map(|b| b.unwrap()).collect()
}

mod api_impl {
    wire_weaver::ww_codegen!(
        chunky_ww_test_api :: ChunkyApp for super::App,
        server = true, no_alloc = true, use_async = false,
        method_model = "_=immediate",
        property_model = "_=get_set",
    );
}

struct Backend(App);

impl WireWeaverAsyncApiBackend for Backend {
    type Medium = ();

    async fn process_bytes<'a>(
        &mut self,
        out: &mut EventWriter<'_, impl MessageSink>,
        _medium: (),
        data: &[u8],
        scratch: &'a mut [u8],
    ) -> Result<&'a [u8], ShrinkWrapError> {
        let reply = out
            .queued(|queue| self.0.process_request_bytes(data, scratch, queue, ()))
            .await;
        if std::mem::take(&mut self.0.lose_reply) {
            return Ok(&[]);
        }
        reply
    }

    fn version(&self) -> ww_device::ww_link::FullVersion<'_> {
        chunky_ww_test_api::CHUNKY_APP_FULL_GID
    }
}

/// Runs the app until it resets (a reboot request, or a trial that crashes); `None` when stopped.
async fn run_app(
    app: App,
    path: &str,
    faults: &Arc<Mutex<Faults>>,
    stop: &mut mpsc::UnboundedReceiver<()>,
) -> Option<()> {
    let mut tx_buf = vec![0u8; USB_PACKET];
    let mut rx_buf = vec![0u8; MAX_MESSAGE + USB_PACKET];
    let mut scratch = vec![0u8; MAX_MESSAGE];
    let mut event_scratch = vec![0u8; MAX_MESSAGE];
    let (sink, source) = in_process::usb_device(path, USB_PACKET);
    let tx = ww_device::FramedTx::new(sink, &mut tx_buf);
    let rx = ww_device::FramedRx::new(source, &mut rx_buf);
    let config = LinkConfig::new(
        chunky_ww_test_api::CHUNKY_APP_FULL_GID,
        api_impl::api_hash(),
        ww_client_server::COMPACT_VERSION,
    );
    let mut server = ww_device::Server::new(config, tx, rx, TokioClock::new(), &mut scratch, &mut event_scratch);
    let trial = app.node.node().boot_state == BootState::Trial;
    let self_test = faults.lock().unwrap().self_test;
    let mut backend = Backend(app);
    let started = tokio::time::Instant::now();
    let (after, crashes) = match self_test {
        SelfTest::Passes(after) => (after, false),
        SelfTest::Crashes(after) => (after, true),
    };
    let mut tested = !trial;
    loop {
        let self_test = tokio::time::sleep_until(started + after);
        tokio::select! {
            ready = server.wait() => {
                _ = server.handle(ready, &mut backend).await;
            }
            _ = self_test, if !tested => {
                tested = true;
                if crashes {
                    return Some(());
                }
                backend.0.node.confirm().expect("the trial confirms");
            }
            _ = stop.recv() => return None,
        }
        if backend.0.reboot.take().is_some() {
            // the reply is out with the Disconnect, then the device resets
            server.disconnect(DisconnectReason::RequestByUser).await;
            return Some(());
        }
    }
}
