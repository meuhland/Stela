//! Allocation-free COM1 output and the guest console's nonblocking input.
use core::{
    arch::asm,
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};
const UART: u16 = 0x3f8;
unsafe fn out(port: u16, value: u8) {
    // SAFETY: the caller guarantees CPL0 and that `port` is a COM1 register.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nostack, preserves_flags));
    }
}
unsafe fn input(port: u16) -> u8 {
    let value;
    // SAFETY: the caller guarantees CPL0 and that `port` is a COM1 register.
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") value, options(nostack, preserves_flags));
    }
    value
}
pub fn initialize() {
    let answers = stela_boot_core::uart::answers(
        UART,
        &mut |port| {
            // SAFETY: a COM1 register, with exclusive BSP I/O ownership.
            unsafe { input(port) }
        },
        &mut |port, value| {
            // SAFETY: as above; a port nothing answers ignores the write,
            // and a UART's interrupt enables are put back.
            unsafe { out(port, value) }
        },
    );
    ABSENT.store(!answers, Ordering::Relaxed);
    if !answers {
        return;
    }
    // SAFETY: the UART just found at COM1, exclusive BSP I/O ownership.
    unsafe {
        out(UART + 1, 0);
        out(UART + 3, 0x80);
        out(UART, 1);
        out(UART + 1, 0);
        out(UART + 3, 3);
        out(UART + 2, 0xc7);
        out(UART + 4, 0x0b);
    }
}

/// Whether [`initialize`] found no UART at COM1 ([`stela_boot_core::uart::
/// answers`]), or one stopped taking bytes ([`send`]). A port nothing
/// answers reads the same whatever is written: as 0xff its line status
/// reads as data ready and transmitter empty, and without this the guest's
/// console was fed 0xff bytes without end. XNU's console takes each such
/// byte as a break (`cons_cinput` sign-extends it into `TTY_BI`), which
/// with `BRKINT` sends SIGINT to the console's foreground process group:
/// the single-user shell, as soon as it starts. As 0 it never shows the
/// transmitter empty, and each byte waited out its poll.
static ABSENT: AtomicBool = AtomicBool::new(false);

/// Whether COM1 has a UART.
pub fn present() -> bool {
    !ABSENT.load(Ordering::Relaxed)
}

/// Take the loader's [`initialize`] result in the engine, whose image copy
/// was taken before it ran and so starts with a UART assumed present.
pub fn adopt(present: bool) {
    ABSENT.store(!present, Ordering::Relaxed);
}
/// One input byte for the guest console's device owner, when available.
pub fn read() -> Option<u8> {
    if !present() {
        return None;
    }
    // SAFETY: the engine runs at CPL0 after COM1 initialization. Only its
    // console device owner reads RBR; output accesses the separate THR register.
    unsafe { (input(UART + 5) & 1 != 0).then(|| input(UART)) }
}

/// Whether the transmitter is empty: its FIFO takes 16 bytes now.
pub fn ready() -> bool {
    if !present() {
        // What is sent is dropped at once.
        return true;
    }
    // SAFETY: reads the line status register of the configured UART.
    let status = unsafe { input(UART + 5) };
    status & 0x20 != 0
}
/// Send `bytes`, line feeds as CR LF, into a transmit FIFO that
/// [`ready`] found empty: at most 16 bytes in all, sent without polling,
/// as one string output, which a hypervisor takes in one exit.
pub fn burst(bytes: &[u8]) {
    if !present() {
        return;
    }
    let mut fifo = [0u8; 16];
    let mut length = 0;
    for &byte in bytes {
        if byte == b'\n' {
            fifo[length] = b'\r';
            length += 1;
        }
        fifo[length] = byte;
        length += 1;
    }
    // SAFETY: the configured UART's transmit register, with room for all of
    // them; RSI and RCX address and count `fifo`, and DF is clear.
    unsafe {
        asm!(
            "rep outsb",
            in("dx") UART,
            inout("rsi") fifo.as_ptr() => _,
            inout("rcx") length => _,
            options(nostack, preserves_flags, readonly),
        );
    }
}
fn send(byte: u8) {
    if !present() {
        return;
    }
    // Bounded polling has no allocator/runtime callback or lock. A broken
    // UART cannot convert panic into deadlock or resumed execution.
    for _ in 0..100_000 {
        // SAFETY: the line status register is a COM1 register, read at CPL0.
        if unsafe { input(UART + 5) } & 0x20 != 0 {
            // SAFETY: the transmit holding register is a COM1 register,
            // written at CPL0.
            unsafe { out(UART, byte) };
            return;
        }
        core::hint::spin_loop();
    }
    // A transmitter that does not empty in the whole poll, far longer than
    // a 16-byte FIFO takes at any rate the port runs at, takes no more bytes
    // this boot: each would wait out the poll again.
    ABSENT.store(true, Ordering::Relaxed);
}
pub struct Serial;
impl Serial {
    /// Send `bytes`, which may end or start inside a character.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            // A serial terminal treats LF and CR independently.
            if byte == b'\n' {
                send(b'\r');
            }
            send(byte);
        }
    }
}
impl fmt::Write for Serial {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.write_bytes(value.as_bytes());
        Ok(())
    }
}
