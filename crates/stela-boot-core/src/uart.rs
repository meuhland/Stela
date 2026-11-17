//! Whether a 16550-compatible UART answers at a legacy I/O port, decided as
//! Linux's 8250 `autoconfig` decides it before it uses a port.

/// The interrupt enable register's offset from the port base (Linux
/// `serial_reg.h` `UART_IER`).
pub const IER: u16 = 1;
/// The four interrupt enables a 16550 keeps (`UART_IER_ALL_INTR`).
const IER_ALL: u8 = 0x0f;

/// Whether a UART answers at `base`: its interrupt enable register keeps
/// both 0 and all four enables, Linux's "IER test", which is all Linux
/// checks at the PC's standard COM1 to COM3 (`STD_COMX_FLAGS` carries
/// `UPF_SKIP_TEST`). The register is put back as it was. A port nothing
/// answers reads the same, 0xff or 0, whatever is written. A laptop whose
/// empty port read a line status other than 0xff passed the check this
/// replaces: every later byte polled a line status that never showed the
/// transmitter empty, and each log line took seconds.
pub fn answers(
    base: u16,
    input: &mut impl FnMut(u16) -> u8,
    out: &mut impl FnMut(u16, u8),
) -> bool {
    let saved = input(base + IER);
    out(base + IER, 0);
    let cleared = input(base + IER) & IER_ALL;
    out(base + IER, IER_ALL);
    let set = input(base + IER) & IER_ALL;
    out(base + IER, saved);
    cleared == 0 && set == IER_ALL
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    const COM1: u16 = 0x3f8;

    /// A port whose registers read `read` after `written`.
    fn probe(read: impl Fn(u16, Option<u8>) -> u8) -> (bool, Vec<(u16, u8)>) {
        let last = RefCell::new([None::<u8>; 8]);
        let writes = RefCell::new(Vec::new());
        let found = answers(
            COM1,
            &mut |port| read(port - COM1, last.borrow()[usize::from(port - COM1)]),
            &mut |port, value| {
                last.borrow_mut()[usize::from(port - COM1)] = Some(value);
                writes.borrow_mut().push((port, value));
            },
        );
        (found, writes.into_inner())
    }

    #[test]
    fn a_16550_keeps_its_interrupt_enables_and_gets_its_own_back() {
        // A 16550's IER keeps its low four bits; it starts at 0x05 here.
        let (found, writes) = probe(|register, written| match (register, written) {
            (1, Some(value)) => value & 0x0f,
            (1, None) => 0x05,
            _ => 0x60,
        });
        assert!(found);
        assert_eq!(writes.last(), Some(&(COM1 + IER, 0x05)));
    }

    #[test]
    fn ports_that_read_the_same_whatever_is_written_have_no_uart() {
        // Nothing decodes the port: every read is 0xff.
        assert!(!probe(|_, _| 0xff).0);
        // A port that reads 0 and keeps nothing, whose line status never
        // shows the transmitter empty.
        assert!(!probe(|_, _| 0).0);
        // A register that keeps only some of the enables.
        assert!(
            !probe(|register, written| match register {
                1 => written.unwrap_or(0) & 0x03,
                _ => 0x60,
            })
            .0
        );
    }
}
