//! How long a host waits: for a plain call, for a step that erases, for the image check, for the device to come back.

use std::time::Duration;

use chunky::geometry::{Family, FlashGeometry};

/// Worst case of one ERASE step on `geometry`: one erase block, at the slowest the part's datasheet allows.
///
/// - STM32H7: 4 s for a 128 KB sector (STM32H723/733 datasheet DS13313, table "Flash memory programming",
///   `tERASE` max at program/erase parallelism x8, the slowest; the H725/735 share the flash). The same bound
///   for the 8 KB sectors of the H7A3/H7B3, which erase faster.
/// - STM32F1, G0, G4, L4: 40 ms per page (their datasheets give 20 to 40 ms); taken as 100 ms per started 2 KB.
/// - RP2350: external QSPI NOR, 400 ms per 4 KB sector (the max of common parts such as the W25Q series).
///
/// An erase on a single bank stalls the CPU, and with it the reply; the host's timeout of an ERASE call has to
/// cover this.
pub fn max_erase_time(geometry: &FlashGeometry) -> Duration {
    let started = |unit: u32| geometry.erase.div_ceil(unit) as u64;
    match geometry.family {
        Family::Stm32H7 => Duration::from_secs(4) * started(128 * 1024) as u32,
        Family::Stm32F1 | Family::Stm32G0 => Duration::from_millis(100 * started(2048)),
        Family::Rp2350 => Duration::from_millis(400 * started(4096)),
    }
}

/// What a host waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// A call that does not touch flash, and the link's share of every other call.
    pub call: Duration,
    /// One ERASE step on top of [`Timeouts::call`] ([`max_erase_time`]).
    pub erase: Duration,
    /// `Stage::finish`: the node hashes the whole image and checks the seal.
    pub finish: Duration,
    /// From `reboot(App)` until the device answers again: the bootloader's swap (two erases per ACTIVE sector,
    /// three per DFU sector on embassy-boot) and the USB enumeration.
    pub reboot: Duration,
    /// From the trial boot until the device's own self-test confirms it.
    pub confirm: Duration,
}

impl Timeouts {
    /// A call to a node that may be erasing: the boot state calls. A single bank part that confirms its trial
    /// image erases STATE (B135: right after the first request after the reboot) and answers nothing meanwhile.
    pub fn boot_call(&self) -> Duration {
        self.call + self.erase
    }

    /// The timeouts for a device with this layout.
    pub fn for_geometry(geometry: &FlashGeometry) -> Self {
        let erase = max_erase_time(geometry);
        let call = Duration::from_secs(2);
        // every erase block of ACTIVE and DFU erased once more than a swap needs, and 10 s for the rest
        let blocks = (geometry.active.len + geometry.dfu.len).div_ceil(geometry.erase) + 1;
        Self {
            call,
            erase,
            finish: call + Duration::from_secs(30),
            reboot: erase * blocks + Duration::from_secs(10),
            confirm: Duration::from_secs(30),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunky::chips::{STM32G0B1RE, STM32H725IG};

    #[test]
    fn an_h7_sector_takes_seconds() {
        assert_eq!(max_erase_time(&STM32H725IG), Duration::from_secs(4));
        assert_eq!(max_erase_time(&STM32G0B1RE), Duration::from_millis(100));
        // B135: 256 K ACTIVE + 384 K DFU in 128 K sectors
        assert_eq!(
            Timeouts::for_geometry(&STM32H725IG).reboot,
            Duration::from_secs(4 * 6 + 10)
        );
    }
}
