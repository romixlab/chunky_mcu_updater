//! Firmware payloads from ELF files: the bytes of the loadable segments, placed at their load addresses in ACTIVE,
//! ready for [`build_image`](crate::build_image). Untrusted input: the ELF is parsed by `object`, every segment is
//! checked against ACTIVE before a byte is copied.

use chunky::geometry::{Family, FlashGeometry};
use object::read::elf::{ElfFile32, ProgramHeader as _};
use object::{Endianness, elf};

/// Where the flash device is mapped in the address space of a family: chunky's offsets are from here.
pub const fn flash_base(family: Family) -> u32 {
    match family {
        Family::Stm32F1 | Family::Stm32G0 | Family::Stm32H7 => 0x0800_0000,
        // XIP window of the QSPI flash
        Family::Rp2350 => 0x1000_0000,
    }
}

/// Why an ELF gives no payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// Not a 32-bit little-endian ELF, or a malformed one.
    Parse(String),
    /// No loadable segment with bytes in it.
    Empty,
    /// A segment lies outside the image area of ACTIVE (a bootloader, another slot, RAM-only data linked to load
    /// from somewhere else).
    Outside {
        /// Load address of the segment.
        addr: u32,
        /// Its length in the file.
        len: u32,
    },
    /// The lowest segment does not start at ACTIVE: the bootloader jumps to the vector table there.
    NotAtStart {
        /// Lowest load address.
        first: u32,
        /// Where ACTIVE starts.
        active: u32,
    },
}

impl std::fmt::Display for ElfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ElfError::Parse(e) => write!(f, "not a 32-bit ELF: {e}"),
            ElfError::Empty => write!(f, "the ELF has no loadable bytes"),
            ElfError::Outside { addr, len } => write!(
                f,
                "segment at {addr:#010x} ({len} bytes) is outside the image area of ACTIVE (linked for another slot?)"
            ),
            ElfError::NotAtStart { first, active } => write!(
                f,
                "the image starts at {first:#010x}, ACTIVE at {active:#010x}: link the app for ACTIVE (memory.x)"
            ),
        }
    }
}

impl std::error::Error for ElfError {}

/// The payload of `elf` for ACTIVE of `geometry`: every `PT_LOAD` segment's file bytes at its physical (load)
/// address, gaps filled with `0xFF`. ACTIVE is at [`flash_base`] plus its offset; everything must fit the image area
/// (ACTIVE without the descriptor) and start at its first byte.
pub fn payload_from_elf(elf: &[u8], geometry: &FlashGeometry) -> Result<Vec<u8>, ElfError> {
    let file = ElfFile32::<Endianness>::parse(elf).map_err(|e| ElfError::Parse(e.to_string()))?;
    let endian = file.endian();
    let start = flash_base(geometry.family) as u64 + geometry.active.offset as u64;
    let end = start + geometry.image_len() as u64;

    let mut segments = Vec::new();
    for header in file.elf_program_headers() {
        if header.p_type(endian) != elf::PT_LOAD || header.p_filesz(endian) == 0 {
            continue;
        }
        let addr = header.p_paddr(endian);
        let data = header
            .data(endian, elf)
            .map_err(|()| ElfError::Parse("segment outside the file".into()))?;
        let outside = ElfError::Outside {
            addr,
            len: data.len() as u32,
        };
        if (addr as u64) < start || addr as u64 + data.len() as u64 > end {
            return Err(outside);
        }
        segments.push(((addr as u64 - start) as usize, data));
    }
    let first = segments.iter().map(|(at, _)| *at).min().ok_or(ElfError::Empty)?;
    if first != 0 {
        return Err(ElfError::NotAtStart {
            first: (start + first as u64) as u32,
            active: start as u32,
        });
    }
    let len = segments
        .iter()
        .map(|(at, data)| at + data.len())
        .max()
        .expect("not empty");
    let mut payload = vec![0xFF; len];
    for (at, data) in segments {
        payload[at..at + data.len()].copy_from_slice(data);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunky::chips::STM32H725IG;

    /// A minimal ELF32 little-endian ARM executable with the given `(paddr, bytes)` load segments.
    pub(crate) fn elf(segments: &[(u32, &[u8])]) -> Vec<u8> {
        const EHDR: usize = 52;
        const PHDR: usize = 32;
        let data_at = EHDR + PHDR * segments.len();
        let mut out = vec![0u8; data_at];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = 1; // 32-bit
        out[5] = 1; // little endian
        out[6] = 1; // version
        out[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        out[18..20].copy_from_slice(&40u16.to_le_bytes()); // EM_ARM
        out[20..24].copy_from_slice(&1u32.to_le_bytes());
        out[28..32].copy_from_slice(&(EHDR as u32).to_le_bytes()); // e_phoff
        out[40..42].copy_from_slice(&(EHDR as u16).to_le_bytes());
        out[42..44].copy_from_slice(&(PHDR as u16).to_le_bytes());
        out[44..46].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        let mut offset = data_at;
        for (i, (paddr, bytes)) in segments.iter().enumerate() {
            let h = EHDR + i * PHDR;
            let fields = [
                1,
                offset as u32,
                *paddr,
                *paddr,
                bytes.len() as u32,
                bytes.len() as u32,
                5,
                4,
            ];
            for (j, v) in fields.iter().enumerate() {
                out[h + 4 * j..h + 4 * j + 4].copy_from_slice(&v.to_le_bytes());
            }
            offset += bytes.len();
        }
        for (_, bytes) in segments {
            out.extend_from_slice(bytes);
        }
        out
    }

    fn active() -> u32 {
        flash_base(STM32H725IG.family) + STM32H725IG.active.offset
    }

    #[test]
    fn segments_land_at_their_load_addresses() {
        let image = elf(&[(active(), &[1, 2, 3, 4]), (active() + 8, &[5, 6])]);
        let payload = payload_from_elf(&image, &STM32H725IG).unwrap();
        assert_eq!(payload, [1, 2, 3, 4, 0xFF, 0xFF, 0xFF, 0xFF, 5, 6]);
    }

    #[test]
    fn refuses_what_is_not_for_active() {
        let bootloader = elf(&[(flash_base(STM32H725IG.family), &[1; 16])]);
        assert!(matches!(
            payload_from_elf(&bootloader, &STM32H725IG),
            Err(ElfError::Outside { .. })
        ));
        let past_the_descriptor = elf(&[(active() + STM32H725IG.image_len() - 2, &[1; 4])]);
        assert!(matches!(
            payload_from_elf(&past_the_descriptor, &STM32H725IG),
            Err(ElfError::Outside { .. })
        ));
        let late = elf(&[(active() + 4, &[1; 4])]);
        assert!(matches!(
            payload_from_elf(&late, &STM32H725IG),
            Err(ElfError::NotAtStart { .. })
        ));
        assert_eq!(payload_from_elf(&elf(&[]), &STM32H725IG), Err(ElfError::Empty));
    }

    #[test]
    fn garbage_is_an_error() {
        for len in 0..200 {
            let junk = crate::vectors::junk(len as u64, len);
            assert!(payload_from_elf(&junk, &STM32H725IG).is_err());
        }
        let mut cut = elf(&[(active(), &[1; 64])]);
        cut.truncate(cut.len() - 10);
        assert!(payload_from_elf(&cut, &STM32H725IG).is_err());
    }
}
