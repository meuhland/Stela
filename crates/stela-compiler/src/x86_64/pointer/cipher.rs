//! PAC instructions computed in native code, without the memo.
//!
//! Where the host has AES-NI, the engine's PAC cipher is six AES rounds of
//! the pointer and modifier under the key (`pauth::keyed`, the PAC algorithm
//! Apple silicon leaves IMPLEMENTATION DEFINED). Native code runs those
//! rounds in fewer instructions than a memo miss spends reaching the
//! service, and a memo hit is a load from a table far larger than the
//! caches. So a block or thunk given a [`Cipher`] computes every signing,
//! successful authentication and PACGA itself, from what the engine keeps
//! there for the current keys, PAC version and TCR: each key's schedule
//! (the whitening key, five round keys and the last round's key) and each
//! key's PAC layouts.
//!
//! Arm `AddPAC` and `Auth` select their layout by one pointer bit: bit 55,
//! or bit 63 for a signing when neither range ignores the top byte, which
//! then also fixes the top byte as PAC. A [`Layout`] holds that bit and the
//! extension mask (bits 64-TnSZ to the top PAC bit) for either value of it.
//! With `E` that mask, `M` it without bit 55, `C` all ones under the classic
//! insertion and zero otherwise, and `pac` the cipher of the input:
//! - a signing of a canonical pointer (`p & E` is 0 or `E`) has the pointer
//!   as its input and gives `p ^ ((pac ^ (p & C)) & M)`;
//! - an authentication's input is `p` stripped, `(p & !E) | (bit 55 ? E :
//!   0)`; it succeeds exactly when `(pac ^ p ^ (input & !C)) & M` is zero,
//!   and then gives its input;
//! - PACGA gives the cipher's upper half.
//!
//! A key whose tag word is [`super::DISABLED`] (its SCTLR enable clear)
//! gives the pointer unchanged, as Arm does, from the tag word alone: native
//! transitions switch the tag words between two exception levels of one
//! memo generation (`transition::check_pointer`), whose enables differ while
//! their keys, version and layouts do not, so nothing else in the cipher
//! may depend on an enable. Everything else, a closed key (tag word zero),
//! a signing of a non-canonical pointer and a failed authentication, goes
//! to the service as a memo miss does, which computes it the engine's way. The computation clobbers RAX, RCX, R8 to R11, XMM0, XMM1 and flags.
use super::{GENERIC_KEY, Lookup, TAG_WORDS, operands};
use crate::Place;
use crate::x86_64::asm::{Alu, Asm, BitTest, Cond, Gpr, Mem, ShiftOp, Size};
use alloc::vec::Vec;

/// The words of a key's schedule: the whitening key, five round keys and the
/// last round's key, each low quadword first.
pub const SCHEDULE: usize = 7;
/// The layouts of a key: its signings', then its authentications'.
const SIGNING: usize = 0;
const AUTHENTICATION: usize = 1;

/// Where a key's PAC field lies, as [`Cipher`] keeps it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Layout {
    /// The pointer bit that selects the extension, as a mask.
    pub select: u64,
    /// The extension mask with that bit clear and set.
    pub extension: [u64; 2],
    padding: u64,
}

impl Layout {
    /// The layout selected by bit `select` (55 or 63), with these masks.
    pub const fn new(select: u32, extension: [u64; 2]) -> Self {
        Self {
            select: 1 << select,
            extension,
            padding: 0,
        }
    }
}

/// What native code computes PAC results from, at a fixed host address.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cipher {
    /// Each tag word's key schedule ([`SCHEDULE`]).
    pub schedules: [[[u64; 2]; SCHEDULE]; TAG_WORDS],
    /// Each key's layouts, by [`super::key_index`]: signing, then
    /// authentication.
    pub layouts: [[Layout; 2]; 4],
    /// All ones under the classic insertion (FEAT_PAuth without
    /// FEAT_PAuth2), else zero.
    pub classic: u64,
    /// Its complement.
    pub enhanced: u64,
}

const SCHEDULES: usize = core::mem::offset_of!(Cipher, schedules);
const LAYOUTS: usize = core::mem::offset_of!(Cipher, layouts);
const CLASSIC: usize = core::mem::offset_of!(Cipher, classic);
const ENHANCED: usize = core::mem::offset_of!(Cipher, enhanced);
const SELECT: usize = core::mem::offset_of!(Layout, select);
const EXTENSION: usize = core::mem::offset_of!(Layout, extension);
const _: () = assert!(core::mem::size_of::<Layout>() == 32);
const _: () = assert!(SCHEDULES.is_multiple_of(16));

/// The service's code with bit 55 clear: the range bit Arm keeps.
const RANGE_BIT: u8 = 55;

/// `operation` on XMM0 with ModRM.rm `rm`, a legacy SSE form.
fn sse(asm: &mut Asm, opcode: &[u8], rm: impl Into<crate::x86_64::asm::Rm>) {
    asm.sse(Some(0x66), opcode, 0, rm);
}

/// The cipher of `data` and `modifier` under the schedule at R8 +
/// `schedule`, into `out`: the AES rounds of `pauth::keyed`, the block's
/// halves folded. Clobbers XMM0 and XMM1.
fn rounds(asm: &mut Asm, data: Gpr, modifier: Gpr, schedule: i32, out: Gpr) {
    const PXOR: [u8; 2] = [0x0f, 0xef];
    const AESENC: [u8; 3] = [0x0f, 0x38, 0xdc];
    const AESENCLAST: [u8; 3] = [0x0f, 0x38, 0xdd];
    asm.movq_to_xmm(0, data);
    asm.movq_to_xmm(1, modifier);
    asm.punpcklqdq(0, crate::x86_64::asm::Rm::Xmm(1));
    let word = |index: i32| Mem::base(Gpr::R8, schedule + 16 * index);
    sse(asm, &PXOR, word(0));
    for round in 1..SCHEDULE as i32 - 1 {
        sse(asm, &AESENC, word(round));
    }
    sse(asm, &AESENCLAST, word(SCHEDULE as i32 - 1));
    // PSHUFD XMM1, XMM0, 0x4e: the halves swapped, then folded.
    asm.sse_imm(
        Some(0x66),
        &[0x0f, 0x70],
        1,
        crate::x86_64::asm::Rm::Xmm(0),
        0x4e,
    );
    sse(asm, &PXOR, crate::x86_64::asm::Rm::Xmm(1));
    asm.movq_from_xmm(out, 0);
}

/// The result of `form` on its operands, computed from the [`Cipher`] at
/// `cipher` under the tag words at `tags` (one anchor): RAX the result, the
/// misses' jumps with RAX the pointer and the modifier in its register, as
/// [`super::lookup`] leaves them.
pub(super) fn compute(asm: &mut Asm, form: super::Form, tags: Place, cipher: Place) -> Lookup {
    use Gpr::{R8, R9, R10, R11, Rax, Rcx};
    use Size::Qword;
    assert_eq!(
        tags.anchor, cipher.anchor,
        "the tag words and cipher share an anchor"
    );
    let modifier = operands(asm, form);
    debug_assert!(!matches!(modifier, R10 | R11));
    asm.place(R8, Place::new(tags.anchor, 0));
    let at = |offset: usize| i32::try_from(cipher.offset + offset).expect("a cipher field offset");
    let mut misses = Vec::new();
    // A closed key: the service refuses it.
    let word = i32::try_from(tags.offset + usize::from(form.key) * 8).expect("a tag word offset");
    asm.alu_imm8(Alu::Cmp, Qword, Mem::base(R8, word), 0);
    misses.push(asm.jcc(Cond::E));
    let schedule = at(SCHEDULES + usize::from(form.key) * SCHEDULE * 16);
    if form.key == GENERIC_KEY {
        rounds(asm, Rax, modifier, schedule, Rax);
        asm.shift_imm(ShiftOp::Shr, Qword, Rax, 32);
        asm.shift_imm(ShiftOp::Shl, Qword, Rax, 32);
        return Lookup { misses, modifier };
    }
    // A disabled key: the pointer, in RAX already.
    asm.alu_imm32(Alu::Cmp, Qword, Mem::base(R8, word), super::DISABLED as i32);
    let unchanged = asm.jcc(Cond::E);
    let authenticate = form.kind == super::AUTHENTICATED;
    let layout = LAYOUTS
        + (usize::from(form.key) * 2
            + if authenticate {
                AUTHENTICATION
            } else {
                SIGNING
            })
            * core::mem::size_of::<Layout>();
    // RCX = E, by the select bit.
    asm.mov_load(Qword, Rcx, Mem::base(R8, at(layout + EXTENSION)));
    asm.test(Qword, Mem::base(R8, at(layout + SELECT)), Rax);
    asm.cmov(
        Cond::Ne,
        Qword,
        Rcx,
        Mem::base(R8, at(layout + EXTENSION + 8)),
    );
    if !authenticate {
        // Canonical: p & E is 0 or E, so the input is the pointer.
        asm.mov(Qword, R9, Rax);
        asm.alu(Alu::And, Qword, R9, Rcx);
        let canonical = asm.jcc(Cond::E);
        asm.alu(Alu::Cmp, Qword, R9, Rcx);
        misses.push(asm.jcc(Cond::Ne));
        asm.bind(canonical);
        rounds(asm, Rax, modifier, schedule, R9);
        asm.bit_imm(BitTest::Btr, Qword, Rcx, RANGE_BIT);
        // p ^ ((pac ^ (p & C)) & M).
        asm.mov(Qword, R10, Rax);
        asm.alu_load(Alu::And, Qword, R10, Mem::base(R8, at(CLASSIC)));
        asm.alu(Alu::Xor, Qword, R9, R10);
        asm.alu(Alu::And, Qword, R9, Rcx);
        asm.alu(Alu::Xor, Qword, Rax, R9);
        asm.bind(unchanged);
        return Lookup { misses, modifier };
    }
    // R9 = the input: (p & !E) | (bit 55 ? E : 0).
    asm.mov(Qword, R9, Rax);
    asm.shift_imm(ShiftOp::Shl, Qword, R9, 63 - RANGE_BIT);
    asm.shift_imm(ShiftOp::Sar, Qword, R9, 63);
    asm.alu(Alu::And, Qword, R9, Rcx);
    asm.mov(Qword, R10, Rcx);
    asm.unary(crate::x86_64::asm::Unary::Not, Qword, R10);
    asm.alu(Alu::And, Qword, R10, Rax);
    asm.alu(Alu::Or, Qword, R9, R10);
    rounds(asm, R9, modifier, schedule, R10);
    asm.bit_imm(BitTest::Btr, Qword, Rcx, RANGE_BIT);
    // Authenticated when (pac ^ p ^ (input & !C)) & M is zero.
    asm.mov(Qword, R11, R9);
    asm.alu_load(Alu::And, Qword, R11, Mem::base(R8, at(ENHANCED)));
    asm.alu(Alu::Xor, Qword, R10, R11);
    asm.alu(Alu::Xor, Qword, R10, Rax);
    asm.test(Qword, R10, Rcx);
    misses.push(asm.jcc(Cond::Ne));
    asm.mov(Qword, Rax, R9);
    asm.bind(unchanged);
    Lookup { misses, modifier }
}
