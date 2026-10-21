//! Updates a `chunky_ww` device on USB with a sealed image: `cargo run --release -p chunky_ww_host --example
//! usb_flash -- IMAGE (--serial SERIAL | --product NAME)`. The device is found by its USB serial number, or by a part
//! of its product string, and found again by its serial number after each reset.
//!
//! A stand-in until `ww dfu flash` (wire_weaver CLT-33) does the same with a progress bar.

use std::time::Duration;

use chunky_ww_host::{Event, Options, Reconnect, flash};
use wire_weaver_client::DynClient;

/// Finds the device on USB by its serial number.
struct BySerial(String);

impl Reconnect for BySerial {
    async fn reconnect(&mut self, timeout: Duration) -> Result<DynClient, String> {
        // the device resets and enumerates again
        tokio::time::sleep(Duration::from_millis(500)).await;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let serial = self.0.clone();
            match DynClient::traits_only(move |c| c.usb().serial_eq(serial.clone()))
                .connect()
                .await
            {
                Ok(device) => return Ok(device),
                Err(e) if tokio::time::Instant::now() >= deadline => return Err(e.to_string()),
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: usb_flash IMAGE (--serial SERIAL | --product NAME)");
    std::process::exit(2)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [image, how, value] = args.as_slice() else { usage() };
    let image = std::fs::read(image).unwrap_or_else(|e| {
        eprintln!("{image}: {e}");
        std::process::exit(1)
    });
    let devices = wire_weaver_client::list_usb_devices().await.unwrap_or_else(|e| {
        eprintln!("listing USB devices: {e}");
        std::process::exit(1)
    });
    let found: Vec<_> = devices
        .iter()
        .filter(|d| match how.as_str() {
            "--serial" => d.serials.contains(value),
            "--product" => d.product.contains(value.as_str()),
            _ => usage(),
        })
        .collect();
    let [found] = found.as_slice() else {
        eprintln!("{} devices match {how} {value}: {devices:#?}", found.len());
        std::process::exit(1)
    };
    let Some(serial) = found.serials.first().cloned() else {
        eprintln!("{found} has no serial number: it can't be found again after the reset");
        std::process::exit(1)
    };
    println!("updating {found}");
    let mut reconnect = BySerial(serial);
    let device = reconnect.reconnect(Duration::from_secs(5)).await.unwrap_or_else(|e| {
        eprintln!("{found}: {e}");
        std::process::exit(1)
    });
    let mut last = 0;
    let mut progress = |event: Event<'_>| match event {
        Event::Sent { done, total } => {
            let percent = done * 100 / total.max(1);
            if percent >= last + 10 || done == total {
                last = percent;
                println!("sent {percent}% of {total} B");
            }
        }
        event => println!("{event:?}"),
    };
    match flash(device, &mut reconnect, &image, &Options::default(), &mut progress).await {
        Ok(report) => {
            let node = &report.node;
            println!(
                "{:?}, version {:?}, {:?}; plan: {:?}",
                node.boot_state,
                node.app_version,
                report.last_result(),
                report.stats
            );
        }
        Err(e) => {
            eprintln!("update failed: {e}");
            std::process::exit(1)
        }
    }
}
