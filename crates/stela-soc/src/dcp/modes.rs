//! The one mode the modeled DCP advertises for its fixed, connected SDR
//! monitor, in the forms the 26A434 AP reads: the EDID base block, the
//! `IOAVVideoTimingData` and `IOAVVideoColorData` records `A364
//! getVideoDataForIDs` returns, and the serialized `TimingElements` and
//! `ColorElements` arrays the coprocessor publishes on the IOMFB service with
//! `D126`..`D128 setDCPAVPropStart/Chunk/End`.
//!
//! Every layout and key is the exact build's (kernel fileset offsets; VA =
//! offset + 0xfffffe0007004000), from the IOAVFamily code that serializes
//! the same elements:
//!
//! - `IOAVVideoTimingElement::initWithDetailedTiming` (0x39671ac) decodes an
//!   EDID detailed timing into the 0x50-byte timing record (flags +0, the
//!   horizontal axis +0x4 and the vertical +0x24, each `{total, active, sync
//!   width, back porch, front porch, precise sync rate 16.16, sync polarity,
//!   pixel repetition}`, the pixel clock in Hz +0x44, the aspect ratio +0x48,
//!   the scan information +0x4c); `_IOAVVideoTimingSetPixelClock` (0x3973970)
//!   and `_IOAVVideoTimingUpdateSyncRates` (0x39739b0) derive both sync rates;
//!   `_IOAVVideoActiveFormatAspectRatio` (0x396fad4, table `arRef` 0xa52edc)
//!   picks the aspect-ratio code nearest the precise ratio.
//!   `restored_external`'s own `timing_data_struct` log reads the same
//!   offsets (active +0x28 / +0x8, interlaced +0 bit 0, clock +0x44, ratio
//!   +0x48, scan +0x4c).
//! - `IOAVVideoColorElement::withCapabilities` (0x3965c60) packs the 0x20-byte
//!   color record: depth (bits per component) +0, pixel encoding +4, dynamic
//!   range +8, colorimetry +0xc, EOTF +0x10, the supports-DSC byte +0x14.
//! - `IOAVElement::copyProperties` (0x39a31e4: `Score` 32-bit when not
//!   virtual, `ID` 64-bit, `IsVirtual`), `IOAVVideoTimingElement::
//!   copyProperties` (0x39acfec) with `IOAVVideoTimingAxisCopy` (0x3970928),
//!   and `IOAVVideoColorElement::copyProperties` (0x3966258) give the keys and
//!   their widths. The enumerations are the IOAV string tables
//!   (`_IOAVVideoPixelEncodingString` 0x396ed34 → 0 "RGB 4:4:4",
//!   `_IOAVVideoColorEOTFString` → 0 "Traditional gamma (SDR)",
//!   `_IOAVVideoColorDynamicRangeString` → 0 "Full", `_IOAVVideoColorimetryString`
//!   → 10 "sRGB", `_IOAVVideoTimingTypeString` → 3 "Detailed",
//!   `_IOAVStandardTypeString` → 1 "VESA", `_IOAVElementTypeString` → 0
//!   "Video Timing", 1 "Video Color").
//! - The scores follow `IOAVVideoTimingElement::setScoreOffsetMask` (0x3967004)
//!   with no offset bits and `__scoreColorData` (0x3965cc0, stored
//!   `* 1000 + 999` by `initWithData` 0x3965b2c). The firmware's own offset
//!   mask is not pinned; with one advertised timing the score only has to be
//!   positive, which `restored_external` requires (it keeps the timing whose
//!   `Score` exceeds the best so far, starting from 0).
//!
//! The timing's standard type (VESA, an EDID base-block timing without a CEA
//! extension), its being preferred and the color's sRGB colorimetry (the EDID
//! declares sRGB its default color space) are Inferred from the EDID the
//! model serves.
use super::super::scanout::{DEFAULT_REFRESH_MILLIHERTZ, Mode, MonitorId};
use crate::Result;
use alloc::format;
use alloc::string::String;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use core::fmt::Write;
use stela_contracts::refuse;

/// The timing element's ID. `IOMobileFramebufferShim::set_digital_out_mode`
/// (0x2a66aac) treats timing IDs in 500..=10027 as display-wall modes, so
/// the single-display ID lies outside them.
pub const TIMING_ID: u32 = 1;
/// The color element's ID (a separate namespace: `getVideoDataForIDs` takes
/// both).
pub const COLOR_ID: u32 = 2;
/// The first of the IDs of the timings at the mode's other refresh rates,
/// one after another, slower rates later.
pub const OTHER_TIMING_IDS: u32 = 3;
/// `sizeof(IOAVVideoTimingData)`: the `ElementData` restored copies and
/// `A364`'s first argument.
pub const TIMING_BYTES: usize = 0x50;
/// `sizeof(IOAVVideoColorData)`: `A364`'s second argument.
pub const COLOR_BYTES: usize = 0x20;

/// `IOAVVideoPixelEncoding` RGB 4:4:4.
const RGB_444: u32 = 0;
/// `IOAVVideoColorDynamicRange` full.
const FULL_RANGE: u32 = 0;
/// `IOAVVideoColorEOTF` traditional gamma (SDR).
const SDR: u32 = 0;
/// `IOAVVideoColorimetry` sRGB.
const SRGB: u32 = 10;
/// `IOAVStandardType` VESA.
const VESA: u32 = 1;
/// `IOAVVideoTimingType` detailed.
const DETAILED: u32 = 3;
/// `IOAVElementType` video timing and video color.
const TIMING_ELEMENT: u32 = 0;
const COLOR_ELEMENT: u32 = 1;
/// 8 bits per component.
const DEPTH: u32 = 8;

/// `IOAVVideoActiveFormatAspectRatio::arRef` (0xa52edc): each code's
/// width/height as 16.16, ascending.
const ASPECT_RATIOS: [(u32, u32); 18] = [
    (0x6c00, 1),
    (0x8700, 2),
    (0x9000, 3),
    (0x999a, 4),
    (0xa000, 5),
    (0xa492, 6),
    (0xc000, 7),
    (0xcccd, 8),
    (0x10000, 9),
    (0x14000, 10),
    (0x15555, 11),
    (0x18e39, 12),
    (0x1999a, 13),
    (0x1aaab, 14),
    (0x1c71c, 15),
    (0x1e574, 16),
    (0x25ed1, 17),
    (0x3097b, 0),
];

/// Nominal CVT-style blanking of the advertised timing.
const H_FRONT: u32 = 48;
const H_SYNC: u32 = 32;
const H_BLANK: u32 = 320;
const V_FRONT: u32 = 3;
const V_SYNC: u32 = 10;
const V_BLANK: u32 = 45;
/// CVT reduced blanking v2's horizontal blanking (VESA CVT 2.0 section
/// 5.2: front porch 8, sync 32, back porch 40), which a mode takes when its
/// nominal blanking needs more pixel clock than a detailed timing holds.
const RB_H_FRONT: u32 = 8;
const RB_H_BLANK: u32 = 80;
/// CVT's minimum vertical blanking time, in nanoseconds.
const RB_V_BLANK_NANOS: u64 = 460_000;
/// A detailed timing's pixel clock, in its 10 kHz units.
const MAX_CLOCK: u64 = 0xffff;

/// A detailed timing's blanking: `(h_front, h_blank, v_blank)`, with the
/// syncs and the vertical front porch fixed.
type Blanking = (u32, u32, u32);

/// The pixel clock, in 10 kHz units, of `width` by `height` with `blanking`
/// at `refresh` millihertz, rounded to the nearest unit.
fn clock(width: u32, height: u32, (_, h_blank, v_blank): Blanking, refresh: u32) -> u64 {
    let total = (u64::from(width) + u64::from(h_blank)) * (u64::from(height) + u64::from(v_blank));
    total
        .saturating_mul(u64::from(refresh))
        .saturating_add(5_000_000)
        / 10_000_000
}

/// The blanking of `width` by `height` at `refresh` millihertz: the nominal
/// one, or reduced blanking (vertical blanking of at least CVT's 460 us)
/// when the nominal clock does not fit a detailed timing.
fn blanking(width: u32, height: u32, refresh: u32) -> Blanking {
    let nominal = (H_FRONT, H_BLANK, V_BLANK);
    if clock(width, height, nominal, refresh) <= MAX_CLOCK {
        return nominal;
    }
    // A fraction k of each frame is blank: v_blank / (height + v_blank) >= k,
    // with k = 460 us * refresh.
    let k = RB_V_BLANK_NANOS * u64::from(refresh) / 1000;
    let lines = (k * u64::from(height)).div_ceil(1_000_000_000u64.saturating_sub(k).max(1));
    let v_blank = (lines as u32).max(V_FRONT + V_SYNC + 1);
    (RB_H_FRONT, RB_H_BLANK, v_blank)
}

/// The image size in millimetres to report for a `width` by `height` mode
/// whose monitor reported none: a desktop monitor's usual diagonal for the
/// resolution (27" from 2560 pixels across, 24" from 1920, else 21.5"), so
/// that macOS's default scale follows a sane pixel density.
pub fn assumed_size_mm(width: u32, height: u32) -> (u32, u32) {
    let tenths_of_inch: u64 = match width {
        2560.. => 270,
        1920.. => 240,
        _ => 215,
    };
    let (w, h) = (u64::from(width.max(1)), u64::from(height.max(1)));
    let diagonal_pixels = (w * w + h * h).isqrt().max(1);
    let diagonal_mm = tenths_of_inch * 254 / 100;
    let mm = |pixels: u64| (diagonal_mm * pixels / diagonal_pixels) as u32;
    (mm(w), mm(h))
}

/// Whether `mode` at `refresh` millihertz has a pixel clock past a detailed
/// timing's, so its record is built directly ([`fast_timing`]).
fn fast(mode: Mode, refresh: u32) -> bool {
    let (width, height) = (mode.width, mode.height);
    clock(width, height, blanking(width, height, refresh), refresh) > MAX_CLOCK
}

/// The timing record of `mode` at `refresh` millihertz: a detailed timing's,
/// or for a fast mode one built directly.
fn timing_record(
    mode: Mode,
    refresh: u32,
    size_mm: Option<(u32, u32)>,
) -> Result<[u8; TIMING_BYTES]> {
    if fast(mode, refresh) {
        return record(&fast_timing(mode.width, mode.height, refresh)?);
    }
    timing_data(&detailed_timing_sized(mode, refresh, size_mm)?)
}

/// Whether a mode of this size at `refresh` millihertz can be published to
/// macOS: everything [`Advertised`] makes for it is made. A mode whose pixel
/// clock passes a detailed timing's 655.35 MHz, as 2560x1440 from about
/// 165 Hz or 3840x2160 from about 75 Hz, is published with its timing record
/// built directly ([`fast_timing`]). A mode that cannot be published, as one
/// wider than 4095 pixels or past the record's 32-bit pixel clock, is not
/// taken: whoever picks the output's mode takes one that can.
pub fn advertisable(width: u32, height: u32, refresh: u32) -> bool {
    let mode = Mode {
        width,
        height,
        format: super::super::scanout::PixelFormat::Bgra8888,
    };
    Advertised::at(mode, refresh).is_ok()
}

/// An 18-byte EDID detailed timing descriptor for `mode` at `refresh`
/// millihertz, digital separate sync, both polarities positive. A mode
/// whose fields do not fit the descriptor refuses rather than wraps.
pub fn detailed_timing(mode: Mode, refresh: u32) -> Result<[u8; 18]> {
    detailed_timing_sized(mode, refresh, None)
}

/// [`detailed_timing`] with the monitor's image size in millimetres, else
/// [`assumed_size_mm`].
pub fn detailed_timing_sized(
    mode: Mode,
    refresh: u32,
    size_mm: Option<(u32, u32)>,
) -> Result<[u8; 18]> {
    let (w, h) = (mode.width, mode.height);
    if w == 0 || h == 0 || w > 0xfff || h > 0xfff {
        return Err(refuse!("DCP mode does not fit an EDID detailed timing"));
    }
    let (h_front, h_blank, v_blank) = blanking(w, h, refresh);
    let clock = u16::try_from(clock(w, h, (h_front, h_blank, v_blank), refresh))
        .ok()
        .filter(|&clock| clock != 0)
        .ok_or(refuse!(
            "DCP mode's pixel clock does not fit an EDID detailed timing"
        ))?;
    if v_blank > 0xfff {
        return Err(refuse!(
            "DCP mode's vertical blanking does not fit an EDID detailed timing"
        ));
    }
    let mut d = [0u8; 18];
    d[0..2].copy_from_slice(&clock.to_le_bytes());
    d[2] = w as u8;
    d[3] = h_blank as u8;
    d[4] = (((w >> 8) << 4) | (h_blank >> 8)) as u8;
    d[5] = h as u8;
    d[6] = v_blank as u8;
    d[7] = (((h >> 8) << 4) | (v_blank >> 8)) as u8;
    d[8] = h_front as u8;
    d[9] = H_SYNC as u8;
    d[10] = ((V_FRONT << 4) | V_SYNC) as u8;
    d[11] = 0;
    // Image size in mm, no borders.
    let (width_mm, height_mm) = size_mm.unwrap_or_else(|| assumed_size_mm(w, h));
    let (width_mm, height_mm) = (width_mm.min(0xfff), height_mm.min(0xfff));
    d[12] = width_mm as u8;
    d[13] = height_mm as u8;
    d[14] = (((width_mm >> 8) << 4) | (height_mm >> 8)) as u8;
    // Digital separate sync (bits 4:3), vsync and hsync positive.
    d[17] = 0x1e;
    Ok(d)
}

/// The range limits descriptor's offsets byte and five rate bytes (E-EDID
/// 1.4 section 3.10.3.3) for detailed timing `d`: vertical rates in Hz and
/// horizontal in kHz, each range holding the timing's and at least 50..=75 Hz
/// and 30..=80 kHz, and the maximum pixel clock in 10 MHz units. A maximum
/// rate past 255 is stored less 255 with its offset flag set, as the
/// standard has it: 1920x1080 at 240 Hz has 291 kHz lines.
fn range_limits(d: &[u8; 18]) -> Result<(u8, [u8; 5])> {
    /// The offsets byte's flags: the maximum vertical and horizontal rates
    /// are 255 more than their bytes.
    const VERTICAL_MAX_OFFSET: u8 = 0b10;
    const HORIZONTAL_MAX_OFFSET: u8 = 0b10 << 2;
    let clock = u64::from(u16::from_le_bytes([d[0], d[1]])) * 10_000;
    let h_total = u64::from(u32::from(d[2]) | u32::from(d[4] >> 4) << 8)
        + u64::from(u32::from(d[3]) | u32::from(d[4] & 0xf) << 8);
    let v_total = u64::from(u32::from(d[5]) | u32::from(d[7] >> 4) << 8)
        + u64::from(u32::from(d[6]) | u32::from(d[7] & 0xf) << 8);
    let line = clock / h_total;
    let frame = line / v_total;
    let refusal = || refuse!("DCP mode's rates exceed EDID range limits");
    let byte = |value: u64| u8::try_from(value).map_err(|_| refusal());
    // A maximum rate's byte and whether its offset applies.
    let maximum = |value: u64| match value {
        0..=255 => Ok((value as u8, false)),
        256..=510 => Ok(((value - 255) as u8, true)),
        _ => Err(refusal()),
    };
    let (vertical, vertical_offset) = maximum(line.div_ceil(v_total).max(75))?;
    let (horizontal, horizontal_offset) = maximum(line.div_ceil(1000).max(80))?;
    let offsets = if vertical_offset {
        VERTICAL_MAX_OFFSET
    } else {
        0
    } | if horizontal_offset {
        HORIZONTAL_MAX_OFFSET
    } else {
        0
    };
    Ok((
        offsets,
        [
            byte(frame.min(50))?,
            vertical,
            byte((line / 1000).min(30))?,
            horizontal,
            byte(clock.div_ceil(10_000_000).max(17))?,
        ],
    ))
}

/// A truthful EDID 1.4 base block whose one detailed timing is `mode`'s at
/// `refresh` millihertz, of [`assumed_size_mm`].
pub fn edid(mode: Mode, refresh: u32) -> Result<[u8; 128]> {
    edid_sized(mode, refresh, None)
}

/// [`edid`] for a monitor whose image is `size_mm` millimetres, as its own
/// EDID reported, when known.
pub fn edid_sized(mode: Mode, refresh: u32, size_mm: Option<(u32, u32)>) -> Result<[u8; 128]> {
    edid_of(mode, refresh, size_mm, None)
}

/// [`edid_sized`] for the monitor `monitor`, when its EDID is known: its
/// identity goes into the product and serial ([`identity`]).
fn edid_of(
    mode: Mode,
    refresh: u32,
    size_mm: Option<(u32, u32)>,
    monitor: Option<MonitorId>,
) -> Result<[u8; 128]> {
    let size_mm = size_mm.unwrap_or_else(|| assumed_size_mm(mode.width, mode.height));
    let mut e = [0u8; 128];
    e[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    // Manufacturer "STE" (Stela), the product and serial of the monitor as
    // it is presented, week 1, year 2026.
    let man = ((b'S' as u16 - 64) << 10) | ((b'T' as u16 - 64) << 5) | (b'E' as u16 - 64);
    e[8..10].copy_from_slice(&man.to_be_bytes());
    let (product, serial) = identity(mode, size_mm, monitor);
    e[10..12].copy_from_slice(&product.to_le_bytes());
    e[12..16].copy_from_slice(&serial.to_le_bytes());
    e[16] = 1;
    e[17] = 2026u16.wrapping_sub(1990) as u8;
    e[18] = 1; // EDID 1.4
    e[19] = 4;
    // Digital input, 8 bpc, DisplayPort.
    e[20] = 0xa5;
    // The image size in whole centimetres.
    e[21] = (size_mm.0.div_ceil(10)).clamp(1, 255) as u8;
    e[22] = (size_mm.1.div_ceil(10)).clamp(1, 255) as u8;
    e[23] = 0x78; // gamma 2.2
    e[24] = 0x06; // RGB, sRGB default color space, preferred timing native
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    let timing = detailed_timing_sized(mode, refresh, Some(size_mm))?;
    e[54..72].copy_from_slice(&timing);
    e[75] = 0xfc; // display name
    e[77..77 + 6].copy_from_slice(b"Stela\n");
    e[93] = 0xfd; // range limits
    let (offsets, rates) = range_limits(&timing)?;
    e[94] = offsets;
    e[95..95 + 5].copy_from_slice(&rates);
    e[101] = 0x0a;
    e[111] = 0x10; // dummy descriptor
    let sum = e[0..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    e[127] = 0u8.wrapping_sub(sum);
    Ok(e)
}

/// The EDID product code and serial number of `monitor` presented at
/// native `mode` with an image of `size_mm` millimetres: the same for the
/// same monitor and presentation on every boot, whatever its refresh, and
/// different for another monitor ([`MonitorId`]: its own EDID's identity,
/// a serial alone included), a different native size or a different image
/// size. Without the monitor's EDID the size alone decides.
///
/// macOS keeps each display's chosen mode under its identity
/// (`/Library/Preferences/com.apple.windowserver.displays.plist`, written
/// by SkyLight's `CAWSPersistence.mm`: a config's `UUID` and its
/// `CurrentInfo` `Wide`/`High`/`Scale`) and restores that mode when a display
/// of that identity offers it again. One fixed identity made a monitor run
/// at a smaller native size (the firmware framebuffer's 1920 x 1080 on a 4K
/// panel) and the same monitor at its own native size one display to
/// macOS, which then restored the small size's 1x mode, a 1920 x 1080
/// surface stretched over 3840 x 2160, instead of the larger display's
/// default scaled mode. Serial 1 with product 1, the identity before this
/// one, is never produced, so a mode saved under it is not restored either.
fn identity(
    mode: Mode,
    (width_mm, height_mm): (u32, u32),
    monitor: Option<MonitorId>,
) -> (u16, u32) {
    // FNV-1a, 64-bit.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for word in [mode.width, mode.height, width_mm, height_mm] {
        feed(&word.to_le_bytes());
    }
    if let Some(monitor) = monitor {
        feed(&[1]);
        feed(&monitor.hash().to_le_bytes());
    }
    let product = (hash >> 48) as u16;
    let serial = hash as u32;
    (
        if product <= 1 { product + 2 } else { product },
        serial.max(2),
    )
}

fn put(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn get(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes"))
}

/// A detailed timing's fields, as `IOAVVideoTimingElement::
/// initWithDetailedTiming` decodes them: each axis `[total, active, sync
/// width, back porch, front porch]` (field lines for an interlaced timing's
/// vertical axis, already doubled), the pixel clock in Hz and each axis's
/// sync polarity when the sync is digital.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Detailed {
    interlaced: u32,
    horizontal: [u32; 5],
    vertical: [u32; 5],
    clock_hz: u32,
    polarity: Option<(u32, u32)>,
}

/// The timing of a `width` by `height` mode at `refresh` millihertz whose
/// pixel clock passes a detailed timing's: CVT reduced blanking (VESA CVT
/// 2.0 section 5.2: horizontal front porch 8, sync 32, back porch 40;
/// vertical blanking of at least 460 us), digital separate sync, both
/// polarities positive, the clock rounded to the nearest kilohertz, as
/// DisplayID 2.0's type VII timing carries it. The AP reads a mode's timing
/// only from its record (the model serves no EDID bytes: [`Advertised::
/// edid`] gives the monitor's identity), so the record is built from these
/// fields directly. A clock past the record's 32-bit hertz refuses.
fn fast_timing(width: u32, height: u32, refresh: u32) -> Result<Detailed> {
    if width == 0 || height == 0 || refresh == 0 {
        return Err(refuse!("DCP mode has no active area or refresh"));
    }
    let k = RB_V_BLANK_NANOS * u64::from(refresh) / 1000;
    if k >= 1_000_000_000 {
        return Err(refuse!(
            "DCP mode's refresh leaves no time for its vertical blanking"
        ));
    }
    let lines = (k * u64::from(height)).div_ceil(1_000_000_000 - k);
    let v_blank = u32::try_from(lines)
        .map_err(|_| refuse!("DCP mode's vertical blanking overflows"))?
        .max(V_FRONT + V_SYNC + 1);
    let total =
        (u64::from(width) + u64::from(RB_H_BLANK)) * (u64::from(height) + u64::from(v_blank));
    let clock_khz = (total * u64::from(refresh) + 500_000) / 1_000_000;
    let clock_hz = u32::try_from(clock_khz * 1000)
        .ok()
        .filter(|&clock| clock != 0)
        .ok_or(refuse!(
            "DCP mode's pixel clock does not fit the timing record's 32 bits"
        ))?;
    let h_back = RB_H_BLANK - RB_H_FRONT - H_SYNC;
    let v_back = v_blank - V_FRONT - V_SYNC;
    Ok(Detailed {
        interlaced: 0,
        horizontal: [width + RB_H_BLANK, width, H_SYNC, h_back, RB_H_FRONT],
        vertical: [height + v_blank, height, V_SYNC, v_back, V_FRONT],
        clock_hz,
        polarity: Some((1, 1)),
    })
}

/// The `IOAVVideoTimingData` of an EDID detailed timing, decoded as
/// `IOAVVideoTimingElement::initWithDetailedTiming` does.
pub fn timing_data(d: &[u8; 18]) -> Result<[u8; TIMING_BYTES]> {
    record(&decode(d)?)
}

/// The fields of an EDID detailed timing ([`timing_data`]).
fn decode(d: &[u8; 18]) -> Result<Detailed> {
    let clock = u32::from(u16::from_le_bytes([d[0], d[1]]));
    let interlaced = u32::from(d[17] >> 7);
    let h_active = u32::from(d[2]) | u32::from(d[4] >> 4) << 8;
    let h_blank = u32::from(d[3]) | u32::from(d[4] & 0xf) << 8;
    let h_front = u32::from(d[8]) | u32::from(d[11] >> 6 & 3) << 8;
    let h_sync = u32::from(d[9]) | u32::from(d[11] >> 4 & 3) << 8;
    let h_back = h_blank.checked_sub(h_front + h_sync).ok_or(refuse!(
        "DCP detailed timing's horizontal porches exceed its blanking"
    ))?;
    let field = |lines: u32| {
        if interlaced != 0 {
            lines * 2 + 1
        } else {
            lines
        }
    };
    let v_active = (u32::from(d[5]) | u32::from(d[7] >> 4) << 8) << interlaced;
    let v_front = field(u32::from(d[10] >> 4) | u32::from(d[11] >> 2 & 3) << 4);
    let v_sync = (u32::from(d[10] & 0xf) | u32::from(d[11] & 3) << 4) << interlaced;
    let v_blank = field(u32::from(d[6]) | u32::from(d[7] & 0xf) << 8);
    let v_back = v_blank.checked_sub(v_front + v_sync).ok_or(refuse!(
        "DCP detailed timing's vertical porches exceed its blanking"
    ))?;
    // Digital sync: bit 3 separate (vsync polarity bit 2) or composite.
    let polarity = (d[17] & 0x10 != 0).then(|| {
        let h = u32::from(d[17] >> 1 & 1);
        let v = if d[17] & 0x8 == 0 {
            h
        } else {
            u32::from(d[17] >> 2 & 1)
        };
        (h, v)
    });
    Ok(Detailed {
        interlaced,
        horizontal: [h_active + h_blank, h_active, h_sync, h_back, h_front],
        vertical: [v_active + v_blank, v_active, v_sync, v_back, v_front],
        clock_hz: clock * 10_000,
        polarity,
    })
}

/// The `IOAVVideoTimingData` of a timing's fields.
fn record(d: &Detailed) -> Result<[u8; TIMING_BYTES]> {
    let mut t = [0u8; TIMING_BYTES];
    put(&mut t, 0x00, d.interlaced);
    for (axis, [total, active, sync, back, front]) in [(0x04, d.horizontal), (0x24, d.vertical)] {
        put(&mut t, axis, total);
        put(&mut t, axis + 0x4, active);
        put(&mut t, axis + 0x8, sync);
        put(&mut t, axis + 0xc, back);
        put(&mut t, axis + 0x10, front);
    }
    if let Some((h, v)) = d.polarity {
        put(&mut t, 0x1c, h);
        put(&mut t, 0x3c, v);
    }
    // _IOAVVideoTimingSetPixelClock, then _IOAVVideoTimingUpdateSyncRates.
    let pixel_clock = d.clock_hz;
    put(&mut t, 0x44, pixel_clock);
    let line = u64::from(get(&t, 0x04)) * u64::from(get(&t, 0x20) + 1);
    let frame = u64::from(get(&t, 0x24)) * line;
    if line == 0 || frame == 0 {
        return Err(refuse!("DCP detailed timing has no blanking totals"));
    }
    let q16 = u64::from(pixel_clock) << 16;
    let h_rate = (q16 + line * 500) / (line * 1000);
    let mut v_rate = (q16 + frame / 2) / frame;
    if d.interlaced != 0 {
        v_rate *= 2;
    }
    put(&mut t, 0x18, h_rate as u32);
    put(&mut t, 0x38, v_rate as u32);
    if v_rate == 0 || d.horizontal[1] == 0 || d.vertical[1] == 0 {
        return Err(refuse!("DCP detailed timing has no active area or refresh"));
    }
    let ratio = aspect_ratio(precise_aspect_ratio(&t));
    put(&mut t, 0x48, ratio);
    Ok(t)
}

/// `getPreciseAspectRatio`: horizontal over vertical active, 16.16.
fn precise_aspect_ratio(t: &[u8; TIMING_BYTES]) -> u32 {
    ((u64::from(get(t, 0x08)) << 16) / u64::from(get(t, 0x28))) as u32
}

/// `_IOAVVideoActiveFormatAspectRatio`: the code of the nearest ratio.
fn aspect_ratio(precise: u32) -> u32 {
    let distance = |ratio: u32| ratio.abs_diff(precise);
    let mut best = ASPECT_RATIOS.len() - 1;
    for (index, &(ratio, _)) in ASPECT_RATIOS.iter().enumerate() {
        if distance(ratio) < distance(ASPECT_RATIOS[best].0) {
            best = index;
        }
    }
    ASPECT_RATIOS[best].1
}

/// The `IOAVVideoColorData` of the one color mode: RGB 4:4:4, 8 bits per
/// component, full range, SDR, sRGB, no DSC.
pub fn color_data() -> [u8; COLOR_BYTES] {
    let mut c = [0u8; COLOR_BYTES];
    put(&mut c, 0x00, DEPTH);
    put(&mut c, 0x04, RGB_444);
    put(&mut c, 0x08, FULL_RANGE);
    put(&mut c, 0x0c, SRGB);
    put(&mut c, 0x10, SDR);
    c
}

/// `setScoreOffsetMask(0)`: the active pixel clock in MHz, rounded, plus
/// the vertical sync rate, rounded.
fn timing_score(t: &[u8; TIMING_BYTES]) -> u32 {
    let active =
        (u64::from(get(t, 0x08)) * u64::from(get(t, 0x28)) * u64::from(get(t, 0x38))) >> 16;
    let active = (active / u64::from(get(t, 0x20) + 1)) >> (get(t, 0) & 1);
    let megahertz = (active + 500_000) / 1_000_000;
    let refresh = (get(t, 0x38) + 0x8000) >> 16;
    megahertz as u32 + refresh
}

/// `__scoreColorData` for 8-bit RGB 4:4:4 sRGB (depth 200, encoding 40,
/// colorimetry 2), stored as `score * 1000 + 999`.
const COLOR_SCORE: u32 = (200 + 40 + 2) * 1000 + 999;

/// The manufacturer the EDID encodes.
const MANUFACTURER: &str = "STE";

/// An OSUnserializeXML document under construction.
struct Xml(String);

impl Xml {
    fn key(&mut self, key: &str) -> &mut Self {
        let _ = write!(self.0, "<key>{key}</key>");
        self
    }
    fn int(&mut self, key: &str, value: u64, bits: u32) -> &mut Self {
        self.key(key);
        let _ = write!(self.0, "<integer size=\"{bits}\">{value}</integer>");
        self
    }
    fn bool(&mut self, key: &str, value: bool) -> &mut Self {
        self.key(key);
        self.0.push_str(if value { "<true/>" } else { "<false/>" });
        self
    }
    fn string(&mut self, key: &str, value: &str) -> &mut Self {
        self.key(key);
        let _ = write!(self.0, "<string>{value}</string>");
        self
    }
    fn data(&mut self, key: &str, bytes: &[u8]) -> &mut Self {
        self.key(key);
        self.0.push_str("<data>");
        base64(&mut self.0, bytes);
        self.0.push_str("</data>");
        self
    }
    fn raw(&mut self, text: &str) -> &mut Self {
        self.0.push_str(text);
        self
    }
}

fn base64(out: &mut String, bytes: &[u8]) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let word = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * index) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
}

/// A chromaticity coordinate pair.
fn xy(xml: &mut Xml, key: &str, x: u64, y: u64) {
    xml.key(key)
        .raw("<dict>")
        .int("X", x, 32)
        .int("Y", y, 32)
        .raw("</dict>");
}

/// The one color element's properties (`IOAVVideoColorElement::
/// copyProperties`).
fn color_dict(xml: &mut Xml) {
    xml.raw("<dict>")
        .int("Score", u64::from(COLOR_SCORE), 32)
        .int("ID", u64::from(COLOR_ID), 64)
        .bool("IsVirtual", false)
        .int("ElementType", u64::from(COLOR_ELEMENT), 32)
        .data("ElementData", &color_data())
        .int("Depth", u64::from(DEPTH), 32)
        .int("PixelEncoding", u64::from(RGB_444), 32)
        .int("DynamicRange", u64::from(FULL_RANGE), 32)
        .int("EOTF", u64::from(SDR), 32)
        .int("Colorimetry", u64::from(SRGB), 32)
        .bool("SupportsDSC", false)
        .int("StandardType", u64::from(VESA), 32)
        .raw("</dict>");
}

/// One axis's attributes (`IOAVVideoTimingAxisCopy`).
fn axis_dict(xml: &mut Xml, t: &[u8; TIMING_BYTES], axis: usize) {
    let precise = get(t, axis + 0x14);
    xml.raw("<dict>")
        .int("Total", u64::from(get(t, axis)), 32)
        .int("FrontPorch", u64::from(get(t, axis + 0x10)), 32)
        .int("SyncWidth", u64::from(get(t, axis + 0x8)), 32)
        .int("BackPorch", u64::from(get(t, axis + 0xc)), 32)
        .int("Active", u64::from(get(t, axis + 0x4)), 32)
        .int("SyncRate", u64::from((precise + 0x8000) & 0xffff_0000), 32)
        .int("PreciseSyncRate", u64::from(precise), 32)
        .int("SyncPolarity", u64::from(get(t, axis + 0x18)), 32)
        .int("PixelRepetition", u64::from(get(t, axis + 0x1c)), 32)
        .raw("</dict>");
}

/// The advertised mode: its EDID, records and published properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertised {
    /// The EDID base block, the monitor's identity: its detailed timing is
    /// the mode's when the mode's clock fits one, else the size at 60 Hz.
    pub edid: [u8; 128],
    pub timing: [u8; TIMING_BYTES],
    pub color: [u8; COLOR_BYTES],
    /// The refresh in millihertz of [`Self::timing`], the preferred mode.
    refresh: u32,
    /// The same size at the monitor's other rates: each one's refresh and
    /// timing record, IDs from [`OTHER_TIMING_IDS`] on.
    others: Vec<(u32, [u8; TIMING_BYTES])>,
    /// The identity is the monitor's own EDID's with the size, not the
    /// size alone.
    from_monitor: bool,
}

impl Advertised {
    /// The one mode for the scanout's `mode` at 60 Hz, or why it cannot be
    /// advertised.
    pub fn new(mode: Mode) -> Result<Self> {
        Self::at(mode, DEFAULT_REFRESH_MILLIHERTZ)
    }

    /// The one mode for the scanout's `mode` at `refresh` millihertz, or why
    /// it cannot be advertised.
    pub fn at(mode: Mode, refresh: u32) -> Result<Self> {
        Self::sized(mode, refresh, None)
    }

    /// [`Self::at`] for a monitor whose image is `size_mm` millimetres,
    /// when its own EDID said.
    ///
    /// A mode whose pixel clock passes a detailed timing's has its record
    /// built from [`fast_timing`], and its EDID, which gives only the
    /// monitor's identity ([`Self::display_attributes`]), holds the same
    /// size at 60 Hz, as a monitor's base block often does for its fast
    /// modes, which it lists in an extension.
    pub fn sized(mode: Mode, refresh: u32, size_mm: Option<(u32, u32)>) -> Result<Self> {
        Self::of_monitor(mode, refresh, size_mm, None)
    }

    /// [`Self::sized`] for `monitor`, the identity of the real monitor
    /// the scanout shows on, when its EDID is known: the published product,
    /// serial and UUID follow it as well as the size ([`identity`]).
    pub fn of_monitor(
        mode: Mode,
        refresh: u32,
        size_mm: Option<(u32, u32)>,
        monitor: Option<MonitorId>,
    ) -> Result<Self> {
        let from_monitor = monitor.is_some();
        if fast(mode, refresh) {
            return Ok(Self {
                edid: edid_of(mode, DEFAULT_REFRESH_MILLIHERTZ, size_mm, monitor)?,
                timing: timing_record(mode, refresh, size_mm)?,
                color: color_data(),
                refresh,
                others: Vec::new(),
                from_monitor,
            });
        }
        let edid = edid_of(mode, refresh, size_mm, monitor)?;
        let dtd: [u8; 18] = edid[54..72].try_into().expect("18 bytes");
        Ok(Self {
            edid,
            timing: timing_data(&dtd)?,
            color: color_data(),
            refresh,
            others: Vec::new(),
            from_monitor,
        })
    }

    /// Whether the identity follows the monitor's own EDID
    /// ([`Self::of_monitor`]) rather than the size alone.
    pub fn from_monitor(&self) -> bool {
        self.from_monitor
    }

    /// This mode also at each of `rates` (millihertz) other than its own
    /// whose timing can be published, slower ones after faster: macOS
    /// offers them in Displays settings and sets one with
    /// `set_digital_out_mode`.
    pub fn with_rates(mut self, rates: &[u32], size_mm: Option<(u32, u32)>) -> Self {
        let mode = Mode {
            width: get(&self.timing, 0x08),
            height: get(&self.timing, 0x28),
            format: super::super::scanout::PixelFormat::Bgra8888,
        };
        let mut rates: Vec<u32> = rates
            .iter()
            .copied()
            .filter(|&r| r != self.refresh)
            .collect();
        rates.sort_unstable_by(|a, b| b.cmp(a));
        rates.dedup();
        self.others = rates
            .into_iter()
            .filter_map(|rate| Some((rate, timing_record(mode, rate, size_mm).ok()?)))
            .collect();
        self
    }

    /// Each timing's ID, refresh in millihertz and record, the preferred
    /// one first.
    fn timings(&self) -> impl Iterator<Item = (u32, u32, &[u8; TIMING_BYTES])> {
        core::iter::once((TIMING_ID, self.refresh, &self.timing)).chain(
            (OTHER_TIMING_IDS..)
                .zip(&self.others)
                .map(|(id, (rate, timing))| (id, *rate, timing)),
        )
    }

    /// The refresh in millihertz of the advertised timing `timing`.
    pub fn refresh_of(&self, timing: u32) -> Option<u32> {
        self.timings()
            .find(|&(id, _, _)| id == timing)
            .map(|(_, rate, _)| rate)
    }

    /// The video records for `(timing, color)` IDs, when both are
    /// advertised ones.
    pub fn video_data(&self, timing: u32, color: u32) -> Option<(&[u8], &[u8])> {
        let (_, _, record) = self.timings().find(|&(id, _, _)| id == timing)?;
        (color == COLOR_ID).then_some((&record[..], &self.color[..]))
    }

    /// The monitor's `DisplayAttributes`, the EDID's identity as WindowServer
    /// reads it. Chromaticities are sRGB's, in 16.16 fixed point.
    pub fn display_attributes(&self) -> String {
        let e = &self.edid;
        let name = e[77..90].split(|&byte| byte == b'\n').next().unwrap_or(&[]);
        let (width, height) = (get(&self.timing, 0x08), get(&self.timing, 0x28));
        let mut xml = Xml(String::new());
        xml.raw("<dict>")
            .key("ProductAttributes")
            .raw("<dict>")
            .string("ManufacturerID", MANUFACTURER)
            .int(
                "LegacyManufacturerID",
                u64::from(u16::from_be_bytes([e[8], e[9]])),
                32,
            )
            .int(
                "ProductID",
                u64::from(u16::from_le_bytes([e[10], e[11]])),
                32,
            )
            .int(
                "SerialNumber",
                u64::from(u32::from_le_bytes([e[12], e[13], e[14], e[15]])),
                32,
            )
            .int("WeekOfManufacture", u64::from(e[16]), 32)
            .int("YearOfManufacture", 1990 + u64::from(e[17]), 32)
            .string("ProductName", core::str::from_utf8(name).unwrap_or(""))
            .raw("</dict>")
            .int("NativeFormatHorizontalPixels", u64::from(width), 32)
            .int("NativeFormatVerticalPixels", u64::from(height), 32)
            // The base block's image size in whole centimetres, as Apple's
            // EDID parser publishes it. QuartzCore's IOMFBDisplay reads these
            // two keys alone for the panel's physical size; without them
            // WindowServer assumes 72 dpi and CGDisplayScreenSize is zero.
            .int("MaxHorizontalImageSize", u64::from(e[21]), 32)
            .int("MaxVerticalImageSize", u64::from(e[22]), 32)
            .bool("ContinuousFrequencySupport", false)
            .bool("SupportsActiveOff", false)
            .bool("SupportsSuspend", false)
            .key("Chromaticity")
            .raw("<dict>");
        for (key, x, y) in [
            ("Red", 41943, 21627),
            ("Green", 19661, 39322),
            ("Blue", 9830, 3932),
        ] {
            xy(&mut xml, key, x, y);
        }
        xml.raw("</dict>");
        xy(&mut xml, "DefaultWhite", 20493, 21561);
        xml.raw("</dict>");
        xml.0
    }

    /// How the monitor connects: DCP's DisplayPort through the Mac's HDMI port.
    pub fn transport() -> String {
        let mut xml = Xml(String::new());
        xml.raw("<dict>")
            .string("Upstream", "DP")
            .string("Downstream", "HDMI")
            .raw("</dict>");
        xml.0
    }

    /// `IOMFBUUID`, the display's identity as a plist string: its EDID
    /// product and serial ([`identity`]), so the same monitor presented the
    /// same way keeps its settings across boots and a different
    /// presentation is a different display.
    pub fn uuid(&self) -> String {
        format!("<string>{}</string>", self.display_uuid())
    }

    /// The UUID [`Self::uuid`] publishes.
    pub fn display_uuid(&self) -> String {
        let e = &self.edid;
        let product = u16::from_le_bytes([e[10], e[11]]);
        let serial = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
        format!("53544c41-{product:04x}-4000-8000-0000{serial:08x}")
    }

    /// The native size, the size of the preferred timing.
    pub fn native(&self) -> (u32, u32) {
        (get(&self.timing, 0x08), get(&self.timing, 0x28))
    }

    /// The `TimingElements` array (`IOAVVideoTimingElement::copyProperties`
    /// of each timing, the preferred one first, each one's `ColorModes` the
    /// one color element).
    pub fn timing_elements(&self) -> String {
        let mut xml = Xml(String::from("<array>"));
        for (id, _, t) in self.timings() {
            timing_element(&mut xml, id, t, id == TIMING_ID);
        }
        xml.raw("</array>");
        xml.0
    }

    /// The `ColorElements` array: the one color element.
    pub fn color_elements(&self) -> String {
        let mut xml = Xml(String::from("<array>"));
        color_dict(&mut xml);
        xml.raw("</array>");
        xml.0
    }
}

/// One timing's dictionary of the `TimingElements` array.
fn timing_element(xml: &mut Xml, id: u32, t: &[u8; TIMING_BYTES], preferred: bool) {
    xml.raw("<dict>")
        .int("Score", u64::from(timing_score(t)), 32)
        .int("ID", u64::from(id), 64)
        .bool("IsVirtual", false)
        .int("ElementType", u64::from(TIMING_ELEMENT), 32)
        .data("ElementData", t)
        .key("ColorModes")
        .raw("<array>");
    color_dict(xml);
    xml.raw("</array>")
        .bool("IsInterlaced", get(t, 0) & 1 != 0)
        .bool("IsSplit", get(t, 0) & 2 != 0)
        .bool("IsOverscanned", get(t, 0x4c) == 1)
        .bool("IsPreferred", preferred)
        .bool("IsPromoted", false)
        .int("ScanInformation", u64::from(get(t, 0x4c)), 32)
        .int("ValidPixelEncodings", 1 << RGB_444, 32)
        .int("StandardType", u64::from(VESA), 32)
        .int("TimingType", u64::from(DETAILED), 32)
        .int("AspectRatio", u64::from(get(t, 0x48)), 32)
        .int("PreciseAspectRatio", u64::from(precise_aspect_ratio(t)), 32)
        .key("HorizontalAttributes");
    axis_dict(xml, t, 0x04);
    xml.key("VerticalAttributes");
    axis_dict(xml, t, 0x24);
    xml.raw("</dict>");
}

/// The `IOMFBDisplayRefresh` dictionary for a display that refreshes every
/// `period` ticks of a `counter_hz` counter, at that one rate. The kernel's
/// own builder of it (`IOMobileFramebufferLegacy::setDisplayRefreshProperties`
/// 0xfffffe000adb0b58, 26A434 kernelcache) sets these six keys in this order,
/// each a 64-bit number: an interval in 32.32 fixed-point seconds (60 Hz is
/// 0x04444444), then the same interval in counter ticks. A fixed-rate mode
/// has its shortest interval equal to its longest (the kernel's fixed-rate
/// branch also sets both to one value) and no step between rates (step 0,
/// which the kernel also sets). Core Animation reads
/// `displayRefreshStepMachTime` when it is nonzero and otherwise
/// `displayMinRefreshInterval` for its shortest frame interval (QuartzCore
/// 0x18ddc4e8c), with 1/60 s when the dictionary is missing.
pub fn display_refresh(period: u64, counter_hz: u64) -> String {
    let seconds = (u128::from(period) << 32) / u128::from(counter_hz.max(1));
    let seconds = u64::try_from(seconds).unwrap_or(u64::MAX);
    let mut xml = Xml(String::new());
    xml.raw("<dict>")
        .int("displayMinRefreshInterval", seconds, 64)
        .int("displayMinRefreshIntervalMachTime", period, 64)
        .int("displayMaxRefreshInterval", seconds, 64)
        .int("displayMaxRefreshIntervalMachTime", period, 64)
        .int("displayRefreshStep", 0, 64)
        .int("displayRefreshStepMachTime", 0, 64)
        .raw("</dict>");
    xml.0
}

#[cfg(test)]
mod tests {
    use super::super::super::scanout::PixelFormat;
    use super::*;
    use alloc::format;

    #[test]
    fn the_refresh_dictionary_has_the_kernels_keys_order_and_units() {
        // 60 Hz on a 24 MHz counter: the kernel's own 1/60 s word.
        assert_eq!(
            display_refresh(400_000, 24_000_000),
            "<dict>\
             <key>displayMinRefreshInterval</key><integer size=\"64\">71582788</integer>\
             <key>displayMinRefreshIntervalMachTime</key><integer size=\"64\">400000</integer>\
             <key>displayMaxRefreshInterval</key><integer size=\"64\">71582788</integer>\
             <key>displayMaxRefreshIntervalMachTime</key><integer size=\"64\">400000</integer>\
             <key>displayRefreshStep</key><integer size=\"64\">0</integer>\
             <key>displayRefreshStepMachTime</key><integer size=\"64\">0</integer>\
             </dict>"
        );
        assert_eq!(71_582_788, 0x0444_4444);
        // 144 Hz: 1/144 s in 32.32 and the counter's own period.
        let fast = display_refresh(166_667, 24_000_000);
        let seconds = (166_667u128 << 32) / 24_000_000;
        assert!(fast.contains(&format!(
            "<key>displayMinRefreshInterval</key><integer size=\"64\">{seconds}</integer>"
        )));
        assert!(fast.contains(
            "<key>displayMaxRefreshIntervalMachTime</key><integer size=\"64\">166667</integer>"
        ));
    }

    #[test]
    fn a_fast_modes_rates_past_255_are_stored_with_their_offsets() {
        // 1920x1080 at 240 Hz: 291 kHz lines, which one byte cannot hold.
        let e = edid(FULL_HD, 240_000).unwrap();
        assert_eq!(e[93], 0xfd);
        assert_eq!(e[94], 0b10 << 2, "the horizontal maximum is offset");
        let d: [u8; 18] = e[54..72].try_into().unwrap();
        let clock = u64::from(u16::from_le_bytes([d[0], d[1]])) * 10_000;
        let h_total = u64::from(d[2])
            + (u64::from(d[4] >> 4) << 8)
            + u64::from(d[3])
            + (u64::from(d[4] & 0xf) << 8);
        let v_total = u64::from(d[5])
            + (u64::from(d[7] >> 4) << 8)
            + u64::from(d[6])
            + (u64::from(d[7] & 0xf) << 8);
        let line = clock / h_total;
        assert!(line > 255_000);
        assert!(u64::from(e[98]) + 255 >= line.div_ceil(1000));
        assert!(
            u64::from(e[96]) >= line.div_ceil(v_total),
            "240 Hz fits a byte"
        );
        assert_eq!(e.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0, "checksum");
        // 360 Hz also needs the vertical offset.
        let fast = Mode {
            width: 1280,
            height: 720,
            ..FULL_HD
        };
        let e = edid(fast, 360_000).unwrap();
        assert_eq!(e[94], 0b1010);
        assert_eq!(u32::from(e[96]) + 255, 360);
        // A mode within one byte keeps a zero offsets byte.
        assert_eq!(edid(FULL_HD, 60_000).unwrap()[94], 0);
        assert!(advertisable(1920, 1080, 240_000));
        assert!(Advertised::at(FULL_HD, 240_000).is_ok());
    }

    #[test]
    fn a_mode_is_advertisable_exactly_when_everything_advertised_is_made() {
        for (width, height, refresh) in [
            (1920, 1080, 60_000),
            (2560, 1440, 60_000),
            (2560, 1440, 144_000),
            (2560, 1440, 180_000),
            (3840, 2160, 60_000),
            (3840, 2160, 120_000),
            (5120, 2880, 60_000),
            (0, 1080, 60_000),
            (1920, 1080, 240_000),
            (1920, 1080, 360_000),
            (1280, 720, 480_000),
            (640, 480, 600_000),
        ] {
            let mode = Mode {
                width,
                height,
                format: PixelFormat::Bgra8888,
            };
            assert_eq!(
                advertisable(width, height, refresh),
                Advertised::at(mode, refresh).is_ok(),
                "{width}x{height}@{refresh}"
            );
        }
        assert!(advertisable(2560, 1440, 144_000));
        assert!(advertisable(2560, 1440, 180_000));
        assert!(advertisable(3840, 2160, 120_000));
        assert!(advertisable(3840, 2160, 240_000));
        assert!(!advertisable(5120, 2880, 60_000), "wider than 4095");
        assert!(!advertisable(3840, 2160, 600_000), "past 4.29 GHz");
    }

    const FULL_HD: Mode = Mode {
        width: 1920,
        height: 1080,
        format: PixelFormat::Bgra8888,
    };

    #[test]
    fn the_edid_is_well_formed_for_the_mode() {
        let e = edid(FULL_HD, 60_000).unwrap();
        assert_eq!(&e[0..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        assert_eq!(e.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0, "checksum");
        let dtd = &e[54..72];
        assert_eq!(u32::from(dtd[2]) | (u32::from(dtd[4] >> 4) << 8), 1920);
        // A mode the descriptor cannot hold refuses rather than wraps.
        let huge = Mode {
            width: 5120,
            ..FULL_HD
        };
        assert!(edid(huge, 60_000).is_err());
        // The range limits are the old fixed 50..=75 Hz, 30..=80 kHz, 170 MHz
        // for a mode within them.
        assert_eq!(&e[95..101], &[0x32, 0x4b, 0x1e, 0x50, 0x11, 0x00]);
    }

    fn dtd_size_mm(e: &[u8; 128]) -> (u32, u32) {
        let d = &e[54..72];
        (
            u32::from(d[12]) | u32::from(d[14] >> 4) << 8,
            u32::from(d[13]) | u32::from(d[14] & 0xf) << 8,
        )
    }

    #[test]
    fn a_monitors_reported_size_reaches_both_size_fields() {
        let uhd = Mode {
            width: 3840,
            height: 2160,
            ..FULL_HD
        };
        // A 27" 4K panel: 597 x 336 mm.
        let e = edid_sized(uhd, 60_000, Some((597, 336))).unwrap();
        assert_eq!(dtd_size_mm(&e), (597, 336));
        assert_eq!((e[21], e[22]), (60, 34));
        assert_eq!(e.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0, "checksum");
        let a = Advertised::sized(uhd, 60_000, Some((597, 336))).unwrap();
        assert_eq!(a.edid, e);
        // WindowServer learns the size only from DisplayAttributes.
        let attributes = a.display_attributes();
        for key in [
            "<key>MaxHorizontalImageSize</key><integer size=\"32\">60</integer>",
            "<key>MaxVerticalImageSize</key><integer size=\"32\">34</integer>",
        ] {
            assert!(attributes.contains(key), "{key} in {attributes}");
        }
    }

    #[test]
    fn an_unreported_size_follows_a_usual_diagonal_for_the_resolution() {
        // 24" 1080p, 27" 1440p and 4K, 21.5" below 1920 pixels across.
        assert_eq!(assumed_size_mm(1920, 1080), (531, 298));
        assert_eq!(assumed_size_mm(2560, 1440), (597, 335));
        assert_eq!(assumed_size_mm(3840, 2160), (597, 335));
        assert_eq!(assumed_size_mm(1280, 720), (476, 267));
        let e = edid(FULL_HD, 60_000).unwrap();
        assert_eq!(dtd_size_mm(&e), (531, 298));
        assert_eq!((e[21], e[22]), (54, 30));
    }

    #[test]
    fn the_timing_record_is_the_ioav_decode_of_the_edid_timing() {
        let a = Advertised::new(FULL_HD).unwrap();
        let t = &a.timing;
        // 1920 + 320 by 1080 + 45 at 151.2 MHz: exactly 60 Hz, 67.5 kHz.
        assert_eq!(
            [
                get(t, 0x04),
                get(t, 0x08),
                get(t, 0x0c),
                get(t, 0x10),
                get(t, 0x14)
            ],
            [2240, 1920, 32, 240, 48]
        );
        assert_eq!(
            [
                get(t, 0x24),
                get(t, 0x28),
                get(t, 0x2c),
                get(t, 0x30),
                get(t, 0x34)
            ],
            [1125, 1080, 10, 32, 3]
        );
        assert_eq!(get(t, 0x44), 151_200_000);
        assert_eq!(get(t, 0x38), 60 << 16);
        assert_eq!(get(t, 0x18), 0x0043_8000);
        assert_eq!((get(t, 0x1c), get(t, 0x3c)), (1, 1));
        // 16:9 is code 15 (IOAV string table index 14 plus one).
        assert_eq!(get(t, 0x48), 15);
        assert_eq!(get(t, 0x4c), 0);
        // 124 MHz of active pixels at 60 Hz.
        assert_eq!(timing_score(t), 184);
    }

    /// The timing record's vertical rate in Hz, rounded.
    fn hertz(a: &Advertised) -> u32 {
        (get(&a.timing, 0x38) + 0x8000) >> 16
    }

    #[test]
    fn a_mode_at_several_rates_publishes_a_timing_for_each_the_preferred_first() {
        let qhd = Mode {
            width: 2560,
            height: 1440,
            format: crate::scanout::PixelFormat::Bgra8888,
        };
        // 7000 Hz passes the record's 32-bit pixel clock: left out.
        let rates = [144_000, 60_000, 240_000, 120_000, 60_000, 7_000_000];
        let a = Advertised::at(qhd, 144_000)
            .unwrap()
            .with_rates(&rates, None);
        let ids = [
            TIMING_ID,
            OTHER_TIMING_IDS,
            OTHER_TIMING_IDS + 1,
            OTHER_TIMING_IDS + 2,
        ];
        assert_eq!(
            ids.map(|id| a.refresh_of(id)),
            [Some(144_000), Some(240_000), Some(120_000), Some(60_000)]
        );
        assert_eq!(a.refresh_of(OTHER_TIMING_IDS + 3), None);
        let rate_of = |id| {
            let (timing, color) = a.video_data(id, COLOR_ID).unwrap();
            assert_eq!(color, &a.color[..]);
            (get(timing, 0x38) + 0x8000) >> 16
        };
        assert_eq!(ids.map(rate_of), [144, 240, 120, 60]);
        assert!(a.video_data(OTHER_TIMING_IDS, TIMING_ID).is_none());
        // The 240 Hz record is built directly, the others from detailed
        // timings; the EDID stays the preferred mode's.
        assert_eq!(&a.edid[..], &edid(qhd, 144_000).unwrap()[..]);
        let elements = a.timing_elements();
        assert_eq!(
            elements.matches("<key>HorizontalAttributes</key>").count(),
            4
        );
        assert_eq!(elements.matches("<key>IsPreferred</key><true/>").count(), 1);
        let first = elements.find("<key>IsPreferred</key><true/>").unwrap();
        let second = elements.find("<key>IsPreferred</key><false/>").unwrap();
        assert!(first < second);
        assert!(elements.starts_with("<array><dict><key>Score</key>"));
        assert!(elements.ends_with("</dict></array>"));
        // One rate alone publishes the one element as before.
        let alone = Advertised::at(qhd, 144_000)
            .unwrap()
            .with_rates(&[144_000], None);
        assert_eq!(alone, Advertised::at(qhd, 144_000).unwrap());
    }

    #[test]
    fn a_fast_monitor_is_advertised_at_its_refresh() {
        let qhd = Mode {
            width: 2560,
            height: 1440,
            ..FULL_HD
        };
        // 2880 by 1485 at 144 Hz fits a detailed timing with the nominal
        // blanking: 615.86 MHz.
        let a = Advertised::at(qhd, 144_000).unwrap();
        assert_eq!(hertz(&a), 144);
        assert_eq!((get(&a.timing, 0x04), get(&a.timing, 0x24)), (2880, 1485));
        assert_eq!(get(&a.timing, 0x44), 615_860_000);
        // The range limits hold the timing's 144 Hz and 214 kHz, 620 MHz.
        assert_eq!(&a.edid[95..100], &[50, 144, 30, 214, 62]);
        // 2560x1600 at 144 Hz needs reduced blanking: 80 pixels, and 114
        // lines for 460 us.
        let wide = Mode {
            height: 1600,
            ..qhd
        };
        let a = Advertised::at(wide, 144_000).unwrap();
        assert_eq!(hertz(&a), 144);
        assert_eq!((get(&a.timing, 0x04), get(&a.timing, 0x24)), (2640, 1714));
        assert_eq!([get(&a.timing, 0x14), get(&a.timing, 0x10)], [8, 40]);
        // Without it the clock would not fit. 4K at 120 Hz does not fit a
        // detailed timing even with it, so its record is built directly.
        let uhd = Mode {
            width: 3840,
            height: 2160,
            ..FULL_HD
        };
        assert_eq!(hertz(&Advertised::at(uhd, 60_000).unwrap()), 60);
        assert_eq!(hertz(&Advertised::at(uhd, 120_000).unwrap()), 120);
    }

    #[test]
    fn a_mode_past_a_detailed_timings_clock_gets_a_reduced_blanking_record() {
        let uhd = Mode {
            width: 3840,
            height: 2160,
            ..FULL_HD
        };
        let a = Advertised::at(uhd, 240_000).unwrap();
        // 3920 by 2429: 80 pixels of blanking, and 269 lines for 460 us at
        // 240 Hz (0.1104 of a frame: 2160 x 0.1104 / 0.8896 = 268.06, up).
        assert_eq!((get(&a.timing, 0x04), get(&a.timing, 0x08)), (3920, 3840));
        assert_eq!((get(&a.timing, 0x24), get(&a.timing, 0x28)), (2429, 2160));
        // Front porch 8, sync 32, back porch 40; vertical 3, 10, the rest.
        assert_eq!(
            [
                get(&a.timing, 0x14),
                get(&a.timing, 0x0c),
                get(&a.timing, 0x10)
            ],
            [8, 32, 40]
        );
        assert_eq!(
            [
                get(&a.timing, 0x34),
                get(&a.timing, 0x2c),
                get(&a.timing, 0x30)
            ],
            [3, 10, 256]
        );
        // 3920 x 2429 x 240 Hz = 2285.2 MHz, to the kilohertz.
        assert_eq!(get(&a.timing, 0x44), 2_285_203_000);
        assert_eq!(hertz(&a), 240);
        assert_eq!([get(&a.timing, 0x1c), get(&a.timing, 0x3c)], [1, 1]);
        assert_eq!(get(&a.timing, 0x48), aspect_ratio(0x1c71c), "16:9");
        // The identity block holds the size at 60 Hz, which a detailed
        // timing holds, and the published element carries the fast rate.
        assert_eq!(&a.edid[..], &edid(uhd, 60_000).unwrap()[..]);
        let elements = a.timing_elements();
        assert!(elements.contains(&format!(
            "<key>PreciseSyncRate</key><integer size=\"32\">{}</integer>",
            get(&a.timing, 0x38)
        )));
        // 2560x1440 at 240 Hz and 1920x1080 at 500 Hz the same way.
        let qhd = Mode {
            width: 2560,
            height: 1440,
            ..FULL_HD
        };
        assert_eq!(hertz(&Advertised::at(qhd, 240_000).unwrap()), 240);
        assert_eq!(hertz(&Advertised::at(FULL_HD, 500_000).unwrap()), 500);
    }

    #[test]
    fn the_color_record_packs_like_with_capabilities() {
        let c = color_data();
        assert_eq!(
            [
                get(&c, 0),
                get(&c, 4),
                get(&c, 8),
                get(&c, 0xc),
                get(&c, 0x10)
            ],
            [8, 0, 0, 10, 0]
        );
        assert!(c[0x14..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn video_data_answers_only_the_advertised_ids() {
        let a = Advertised::new(FULL_HD).unwrap();
        let (timing, color) = a.video_data(TIMING_ID, COLOR_ID).unwrap();
        assert_eq!((timing.len(), color.len()), (TIMING_BYTES, COLOR_BYTES));
        assert!(a.video_data(COLOR_ID, TIMING_ID).is_none());
        assert!(!(500..=10027).contains(&TIMING_ID));
    }

    #[test]
    fn the_properties_carry_the_keys_restore_reads() {
        let a = Advertised::new(FULL_HD).unwrap();
        let timings = a.timing_elements();
        for key in [
            "<key>ID</key><integer size=\"64\">1</integer>",
            "<key>Score</key><integer size=\"32\">184</integer>",
            "<key>ElementData</key><data>",
            "<key>ColorModes</key><array><dict>",
            "<key>Active</key><integer size=\"32\">1920</integer>",
            "<key>Active</key><integer size=\"32\">1080</integer>",
            "<key>SyncRate</key><integer size=\"32\">3932160</integer>",
            "<key>PixelEncoding</key><integer size=\"32\">0</integer>",
            "<key>Depth</key><integer size=\"32\">8</integer>",
            "<key>EOTF</key><integer size=\"32\">0</integer>",
            "<key>DynamicRange</key><integer size=\"32\">0</integer>",
        ] {
            assert!(timings.contains(key), "{key}");
        }
        assert!(timings.starts_with("<array><dict>") && timings.ends_with("</dict></array>"));
        let colors = a.color_elements();
        assert!(colors.contains("<key>ID</key><integer size=\"64\">2</integer>"));
        assert!(colors.contains("<key>Score</key><integer size=\"32\">242999</integer>"));
    }

    /// The value of integer `key` in a serialized property.
    fn integer(xml: &str, key: &str) -> u64 {
        let open = format!("<key>{key}</key><integer size=\"");
        let start = xml.find(&open).unwrap_or_else(|| panic!("{key}")) + open.len();
        let value = &xml[start..];
        let value = &value[value.find('>').unwrap() + 1..];
        value[..value.find('<').unwrap()].parse().unwrap()
    }

    /// The UI scale SkyLight 27's density rule picks as the default for a
    /// display publishing `attributes` (`GenerateModeListForDisplay`, guest
    /// dyld cache 0x1828f9cec..0x1828fad38, no override file for a vendor
    /// that is not Apple's): physical width in inches from
    /// `MaxHorizontalImageSize` cm x 0.3937 (QuartzCore 0x18e0bdfe0); half
    /// the native density from 3840 x 2160 up, else whichever of the native
    /// density and its half is nearer 110 ppi.
    fn density_default_scale(attributes: &str) -> u32 {
        let width = integer(attributes, "NativeFormatHorizontalPixels") as f64;
        let height = integer(attributes, "NativeFormatVerticalPixels") as f64;
        let inches = integer(attributes, "MaxHorizontalImageSize") as f64 * 0.3937;
        assert!(inches > 6.5, "a physical width SkyLight uses");
        let ppi = width / inches;
        let half = ppi / 2.0;
        let target =
            if width >= 3840.0 && height >= 2160.0 || (half - 110.0).abs() < (ppi - 110.0).abs() {
                half
            } else {
                ppi
            };
        if target == ppi { 1 } else { 2 }
    }

    #[test]
    fn the_first_published_attributes_already_give_the_settled_default_mode() {
        // The monitor of the measured runs (EDID 650 x 369 mm) and the sizes
        // assumed when a monitor reports none. WindowServer's settled mode in
        // those runs, 2560 x 1440 at scale 1, carries the default flag from
        // the first mode list on (run 20261110-221312, `FMD` lines).
        let qhd = Mode {
            width: 2560,
            height: 1440,
            format: PixelFormat::Bgra8888,
        };
        let uhd = Mode {
            width: 3840,
            height: 2160,
            format: PixelFormat::Bgra8888,
        };
        for (mode, size, scale) in [
            (qhd, Some((650, 369)), 1),
            (qhd, None, 1),
            (FULL_HD, None, 1),
            (uhd, None, 2),
            // A 27" 4K monitor (EDID 699 x 395 mm) at its native size, and
            // at the firmware framebuffer's 1920 x 1080 when it hangs off
            // another display adapter.
            (uhd, Some((699, 395)), 2),
            (FULL_HD, Some((699, 395)), 1),
        ] {
            let a = Advertised::sized(mode, 60_000, size).unwrap();
            let attributes = a.display_attributes();
            assert_eq!(
                integer(&attributes, "NativeFormatHorizontalPixels"),
                u64::from(mode.width)
            );
            assert_eq!(
                integer(&attributes, "NativeFormatVerticalPixels"),
                u64::from(mode.height)
            );
            // The image size in the attributes is the EDID's, which the
            // detailed timing gives in millimetres.
            let (width_mm, height_mm) = dtd_size_mm(&a.edid);
            assert_eq!(
                integer(&attributes, "MaxHorizontalImageSize"),
                u64::from(width_mm.div_ceil(10))
            );
            assert_eq!(
                integer(&attributes, "MaxVerticalImageSize"),
                u64::from(height_mm.div_ceil(10))
            );
            // The one timing is the native size and preferred, so the
            // native size SkyLight reads matches the attributes.
            let timings = a.timing_elements();
            assert_eq!(timings.matches("<key>IsPreferred</key><true/>").count(), 1);
            assert_eq!(timings.matches("<key>TimingType</key>").count(), 1);
            assert!(timings.contains(&format!(
                "<key>Active</key><integer size=\"32\">{}</integer>",
                mode.width
            )));
            assert!(timings.contains(&format!(
                "<key>Active</key><integer size=\"32\">{}</integer>",
                mode.height
            )));
            assert_eq!(
                density_default_scale(&attributes),
                scale,
                "{mode:?} {size:?}"
            );
            // Building the answers again gives the same bytes: nothing in
            // them depends on when they are asked.
            assert_eq!(Advertised::sized(mode, 60_000, size).unwrap(), a);
        }
    }

    fn product_and_serial(xml: &str) -> (u64, u64) {
        (integer(xml, "ProductID"), integer(xml, "SerialNumber"))
    }

    #[test]
    fn a_monitor_at_another_native_size_is_another_display_to_macos() {
        let uhd = Mode {
            width: 3840,
            height: 2160,
            ..FULL_HD
        };
        let size = Some((699, 395));
        // The same 4K monitor shown through the firmware framebuffer at
        // 1920 x 1080 and on its own GPU output at 3840 x 2160: macOS must
        // not restore the first one's 1x 1920 x 1080 on the second.
        let copied = Advertised::sized(FULL_HD, 60_000, size).unwrap();
        let native = Advertised::sized(uhd, 60_000, size).unwrap();
        assert_ne!(copied.uuid(), native.uuid());
        assert_ne!(
            product_and_serial(&copied.display_attributes()),
            product_and_serial(&native.display_attributes())
        );
        // Another image size at the same resolution is another monitor.
        let smaller = Advertised::sized(uhd, 60_000, Some((597, 336))).unwrap();
        assert_ne!(smaller.uuid(), native.uuid());
        // The identity before this one is never given again.
        let old = "<string>53544c41-0001-4000-8000-000000000001</string>";
        for a in [&copied, &native, &smaller] {
            assert_ne!(a.uuid(), old);
            let (product, serial) = product_and_serial(&a.display_attributes());
            assert!(product > 1 && serial > 1);
            // The UUID names the EDID's product and serial.
            assert_eq!(
                a.uuid(),
                format!("<string>53544c41-{product:04x}-4000-8000-0000{serial:08x}</string>")
            );
            assert_eq!(a.edid.iter().fold(0u8, |s, b| s.wrapping_add(*b)), 0);
        }
        // The same presentation keeps its identity, whatever its refresh, so
        // a mode chosen for it is kept across boots and rate changes.
        let fast = Advertised::sized(uhd, 120_000, size)
            .unwrap()
            .with_rates(&[120_000, 60_000], size);
        assert_eq!(fast.uuid(), native.uuid());
        assert_eq!(
            product_and_serial(&fast.display_attributes()),
            product_and_serial(&native.display_attributes())
        );
        assert_eq!(
            Advertised::sized(uhd, 60_000, size).unwrap().uuid(),
            native.uuid()
        );
        // An unreported size is the assumed one, consistently.
        assert_eq!(
            Advertised::sized(uhd, 60_000, None).unwrap().uuid(),
            Advertised::sized(uhd, 60_000, Some(assumed_size_mm(3840, 2160)))
                .unwrap()
                .uuid()
        );
    }

    /// A real monitor's EDID base block: manufacturer, product, `serial`,
    /// week and year, and a serial-number descriptor `text` and a name.
    fn monitor_edid(serial: u32, text: &[u8]) -> [u8; 128] {
        let mut e = [0u8; 128];
        e[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        e[8..12].copy_from_slice(&[0x10, 0xac, 0x34, 0x12]);
        e[12..16].copy_from_slice(&serial.to_le_bytes());
        e[16..18].copy_from_slice(&[12, 33]);
        e[72 + 3] = 0xff;
        e[72 + 5..72 + 5 + text.len()].copy_from_slice(text);
        e[90 + 3] = 0xfc;
        e[90 + 5..90 + 12].copy_from_slice(b"Monitor");
        e
    }

    #[test]
    fn a_monitors_own_edid_identity_reaches_the_published_identity() {
        let uhd = Mode {
            width: 3840,
            height: 2160,
            ..FULL_HD
        };
        let size = Some((699, 395));
        let one = MonitorId::from_edid(&monitor_edid(7, b"AB01\n")).unwrap();
        let shown =
            |mode, refresh, monitor| Advertised::of_monitor(mode, refresh, size, monitor).unwrap();
        let native = shown(uhd, 60_000, Some(one));
        assert!(native.from_monitor());
        // The same monitor on the same path: the same identity on every
        // boot and at every refresh.
        assert_eq!(shown(uhd, 60_000, Some(one)), native);
        let fast = shown(uhd, 120_000, Some(one)).with_rates(&[120_000, 60_000], size);
        assert_eq!(fast.uuid(), native.uuid());
        assert_eq!(
            product_and_serial(&fast.display_attributes()),
            product_and_serial(&native.display_attributes())
        );
        // Two monitors that differ only in their serial number, in the
        // base block or in the serial descriptor, are two displays.
        for other in [monitor_edid(8, b"AB01\n"), monitor_edid(7, b"AB02\n")] {
            let other = MonitorId::from_edid(&other).unwrap();
            assert_ne!(other, one);
            assert_ne!(shown(uhd, 60_000, Some(other)).uuid(), native.uuid());
        }
        // The same monitor at another native size stays another display.
        assert_ne!(shown(FULL_HD, 60_000, Some(one)).uuid(), native.uuid());
        // Without its EDID the size alone decides, exactly as before, and
        // that identity is not the monitor's.
        let unknown = shown(uhd, 60_000, None);
        assert!(!unknown.from_monitor());
        assert_eq!(unknown, Advertised::sized(uhd, 60_000, size).unwrap());
        assert_ne!(unknown.uuid(), native.uuid());
        // What is not an EDID base block gives no identity.
        let mut broken = monitor_edid(7, b"AB01\n");
        broken[1] = 0;
        assert_eq!(MonitorId::from_edid(&broken), None);
        assert_eq!(MonitorId::from_edid(&monitor_edid(7, b"")[..127]), None);
        // A bad checksum is tolerated: the identity bytes are read as they are.
        let mut unsealed = monitor_edid(7, b"AB01\n");
        unsealed[127] ^= 1;
        assert_eq!(MonitorId::from_edid(&unsealed), Some(one));
        // Bytes outside the identity fields do not change it.
        let mut resized = monitor_edid(7, b"AB01\n");
        resized[21] = 70;
        assert_eq!(MonitorId::from_edid(&resized), Some(one));
        // Printing the identity shows none of its fields.
        assert_eq!(format!("{one:?}"), "MonitorId(..)");
        // The published EDID stays well formed.
        assert_eq!(native.edid.iter().fold(0u8, |s, b| s.wrapping_add(*b)), 0);
    }

    #[test]
    fn base64_pads_like_the_xml_data_element_expects() {
        let mut out = String::new();
        base64(&mut out, b"Man");
        base64(&mut out, b"Ma");
        base64(&mut out, b"M");
        assert_eq!(out, "TWFuTWE=TQ==");
    }
}
