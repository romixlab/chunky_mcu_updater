//! `ww_ecc_flash`'s errors through `try_read` and `Masked` (ECC-2): a flash whose reads fail like an
//! `EccStrictFlash`'s, with `Error::Ecc` at a torn word and `Error::Flash` for anything else.

use chunky::ecc::{Masked, ReadFault, try_read};
use embedded_storage::nor_flash::{ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash};
use ww_ecc_flash::Error;

const WORD: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Broken;

impl NorFlashError for Broken {
    fn kind(&self) -> NorFlashErrorKind {
        NorFlashErrorKind::Other
    }
}

/// 256 bytes of `i as u8`; the word at `torn` fails ECC, a read touching `dead` fails as a flash error.
struct Strict {
    torn: u32,
    dead: Option<u32>,
}

impl ErrorType for Strict {
    type Error = Error<Broken>;
}

impl ReadNorFlash for Strict {
    const READ_SIZE: usize = 1;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        let end = offset + bytes.len() as u32;
        if self.dead.is_some_and(|d| (offset..end).contains(&d)) {
            return Err(Error::Flash(Broken));
        }
        for (i, b) in bytes.iter_mut().enumerate() {
            let at = offset + i as u32;
            if at / WORD as u32 == self.torn / WORD as u32 {
                return Err(Error::Ecc { offset: self.torn });
            }
            *b = at as u8;
        }
        Ok(())
    }

    fn capacity(&self) -> usize {
        256
    }
}

impl NorFlash for Strict {
    const WRITE_SIZE: usize = WORD;
    const ERASE_SIZE: usize = 256;

    fn erase(&mut self, _: u32, _: u32) -> Result<(), Self::Error> {
        unreachable!()
    }

    fn write(&mut self, _: u32, _: &[u8]) -> Result<(), Self::Error> {
        unreachable!()
    }
}

#[test]
fn ecc_error_is_a_read_fault_ecc() {
    let mut f = Strict { torn: 64, dead: None };
    let mut buf = [0; 100];
    assert_eq!(try_read(&mut f, 10, &mut buf), Err(ReadFault::Ecc));
    try_read(&mut f, 96, &mut buf[..100]).unwrap();
    let mut f = Strict {
        torn: 64,
        dead: Some(200),
    };
    assert_eq!(
        try_read(&mut f, 150, &mut buf),
        Err(ReadFault::Flash(Error::Flash(Broken)))
    );
}

#[test]
fn masked_reads_the_torn_word_as_zeros() {
    let mut f = Masked(Strict { torn: 64, dead: None });
    let mut buf = [0xAA; 100];
    f.read(40, &mut buf).unwrap();
    for (i, &b) in buf.iter().enumerate() {
        let at = 40 + i;
        assert_eq!(b, if (64..96).contains(&at) { 0 } else { at as u8 }, "byte at {at}");
    }
    let mut f = Masked(Strict {
        torn: 64,
        dead: Some(100),
    });
    assert_eq!(f.read(40, &mut buf), Err(Error::Flash(Broken)));
}
