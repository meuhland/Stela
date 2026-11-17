//! The kernel SDK the guest kexts compile against: xnu's headers from
//! `third_party/xnu`, plus what Apple's SDK build generates beside them.
//!
//! - `DriverKit/<Class>.h` for each `iokit/DriverKit/<Class>.iig`. In a kernel
//!   build iig keeps an interface's declarations outside its classes and
//!   turns each class it compiles into `<Class>_Methods` and
//!   `<Class>_KernelMethods`, which xnu's own class declarations expand.
//!   Of those methods only the `Dispatch` override is virtual, so it is the
//!   only one these headers declare: a kext never calls the rest, and every
//!   vtable keeps the kernel's layout.
//! - The Mach interface headers MIG generates, which `mach/mach_interface.h`
//!   includes for user-side RPC stubs a kext does not call: empty.
//! - `TargetConditionals.h`, which Libc supplies, for macOS on arm64.
//! - `string.h`, which the kernel SDK takes from xnu's `libsa`.
//! - `stela/gate.h`: the gate window's layout from [`window`].
//! - `stela/net.h` and `stela/virtio.h`: StelaNet's and the virtio-mmio
//!   transport's layouts, from `stela_contracts::net` and `::virtio`.
//! - `stela/virtio_snd.h`: the virtio sound device's messages, from
//!   `stela_contracts::virtio_snd`.
use anyhow::{Context, Result, bail};
use std::{fmt::Write as _, fs, path::Path};
use stela_contracts::gate::window;
use stela_contracts::net;

/// The MIG interfaces `mach/mach_interface.h` includes.
const MIG_INTERFACES: [&str; 16] = [
    "clock",
    "clock_priv",
    "clock_reply_server",
    "exc_server",
    "host_priv",
    "host_security",
    "mach_exc_server",
    "mach_host",
    "mach_port",
    "notify_server",
    "processor",
    "processor_set",
    "task",
    "thread_act",
    "vm_map",
    "upl",
];

const TARGET_CONDITIONALS: &str = "\
#ifndef __TARGETCONDITIONALS__
#define __TARGETCONDITIONALS__
#define TARGET_OS_MAC 1
#define TARGET_OS_OSX 1
#define TARGET_OS_IPHONE 0
#define TARGET_OS_IOS 0
#define TARGET_OS_WATCH 0
#define TARGET_OS_TV 0
#define TARGET_OS_BRIDGE 0
#define TARGET_OS_VISION 0
#define TARGET_OS_MACCATALYST 0
#define TARGET_OS_DRIVERKIT 0
#define TARGET_OS_SIMULATOR 0
#define TARGET_OS_EMBEDDED 0
#define TARGET_OS_EXCLAVECORE 0
#define TARGET_OS_EXCLAVEKIT 0
#define TARGET_CPU_ARM64 1
#define TARGET_CPU_X86_64 0
#define TARGET_RT_LITTLE_ENDIAN 1
#define TARGET_RT_64_BIT 1
#endif
";

/// Writes the generated headers for `xnu` into `include`.
pub fn generate(xnu: &Path, include: &Path) -> Result<()> {
    if include.exists() {
        fs::remove_dir_all(include)?;
    }
    let driverkit = include.join("DriverKit");
    fs::create_dir_all(&driverkit)?;
    let interfaces = xnu.join("iokit/DriverKit");
    for entry in fs::read_dir(&interfaces)
        .with_context(|| format!("{} (run `cargo xtask gpu fetch`)", interfaces.display()))?
    {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "iig") {
            let source = fs::read_to_string(&path)?;
            let header = kernel_header(&source)
                .with_context(|| format!("cannot generate from {}", path.display()))?;
            let stem = path.file_stem().unwrap();
            fs::write(driverkit.join(stem).with_extension("h"), header)?;
        }
    }
    let mach = include.join("mach");
    fs::create_dir_all(&mach)?;
    for interface in MIG_INTERFACES {
        let header = format!(
            "/* The MIG interface `{interface}` has no kernel-side callers in a kext. */\n"
        );
        fs::write(mach.join(interface).with_extension("h"), header)?;
    }
    fs::write(include.join("TargetConditionals.h"), TARGET_CONDITIONALS)?;
    fs::copy(xnu.join("osfmk/libsa/string.h"), include.join("string.h"))?;
    let stela = include.join("stela");
    fs::create_dir_all(&stela)?;
    fs::write(stela.join("gate.h"), gate_header())?;
    fs::write(stela.join("net.h"), net_header())?;
    fs::write(stela.join("virtio.h"), virtio_header())?;
    fs::write(stela.join("virtio_snd.h"), virtio_snd_header())?;
    Ok(())
}

/// The kernel header iig generates from an interface: its source with the
/// classes it compiles replaced by their method macros, and its `.iig`
/// includes by their headers.
pub fn kernel_header(source: &str) -> Result<String> {
    let mut header = String::new();
    let mut classes = Vec::new();
    let mut rest = source;
    while let Some(start) = class_definition(rest) {
        header.push_str(&rest[..start]);
        let (name, length) = class_extent(&rest[start..])?;
        classes.push(name);
        rest = &rest[start + length..];
    }
    header.push_str(rest);
    let mut header = header.replace(".iig>", ".h>");
    for class in classes {
        writeln!(
            header,
            "#define {class}_Methods public: \
             virtual kern_return_t Dispatch(const IORPC rpc) APPLE_KEXT_OVERRIDE; \
             static kern_return_t _Dispatch({class} * self, const IORPC rpc);\n\
             #define {class}_KernelMethods"
        )?;
    }
    Ok(header)
}

/// The offset of the next line-leading `class` that opens a body, skipping
/// forward declarations.
fn class_definition(source: &str) -> Option<usize> {
    let mut offset = 0;
    loop {
        let found = source[offset..].find("class ")? + offset;
        let at_line_start = found == 0 || source.as_bytes()[found - 1] == b'\n';
        let rest = &source[found..];
        let opens = rest.find('{');
        if at_line_start
            && opens.is_some_and(|opens| rest.find(';').is_none_or(|ends| opens < ends))
        {
            return Some(found);
        }
        offset = found + "class ".len();
    }
}

/// A class definition's name and length through its closing `};`, skipping
/// comments and literals while matching braces.
fn class_extent(definition: &str) -> Result<(String, usize)> {
    let opens = definition.find('{').unwrap();
    let head = &definition["class ".len()..opens];
    let head = head.split(':').next().unwrap();
    // Annotations precede the name; a parenthesized one takes arguments.
    let name = without_parentheses(head)
        .split_whitespace()
        .last()
        .context("a class without a name")?
        .to_owned();
    let bytes = definition.as_bytes();
    let (mut depth, mut index) = (0usize, opens);
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = definition[index..]
                    .find('\n')
                    .map_or(bytes.len(), |end| index + end);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = definition[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| index + 2 + end + 1);
            }
            quote @ (b'"' | b'\'') => {
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    index += usize::from(bytes[index] == b'\\') + 1;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let ends = definition[index..]
                        .find(';')
                        .context("a class body without its `;`")?;
                    return Ok((name, index + ends + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    bail!("class {name} never closes")
}

fn without_parentheses(text: &str) -> String {
    let mut depth = 0usize;
    text.chars()
        .filter(|&character| {
            match character {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => return depth == 0,
            }
            false
        })
        .collect()
}

fn gate_header() -> String {
    let mut header = String::from(
        "/* The StelaGPU gate window's layout, from stela_contracts::gate::window. */\n\
         #pragma once\n",
    );
    for (name, value) in window::LAYOUT {
        writeln!(header, "#define STELA_GATE_{name} {value:#x}ULL").unwrap();
    }
    header
}

/// `stela/net.h`: the StelaNet register page, its feature bits and its
/// virtqueue layout, from `stela_contracts::net`.
fn net_header() -> String {
    use net::{features, ring};
    let mut header = String::from(
        "/* The StelaNet adapter's layout, from stela_contracts::net. */\n\
         #pragma once\n",
    );
    for (name, value) in net::window::LAYOUT {
        writeln!(header, "#define STELA_NET_{name} {value:#x}ULL").unwrap();
    }
    for (name, value) in [
        ("CSUM", features::CSUM),
        ("GUEST_CSUM", features::GUEST_CSUM),
        ("GUEST_TSO4", features::GUEST_TSO4),
        ("GUEST_TSO6", features::GUEST_TSO6),
        ("HOST_TSO4", features::HOST_TSO4),
        ("HOST_TSO6", features::HOST_TSO6),
        ("MRG_RXBUF", features::MRG_RXBUF),
        ("MAC", features::MAC),
        ("EVENT_IDX", features::EVENT_IDX),
        ("VERSION_1", features::VERSION_1),
        ("OFFERED", features::OFFERED),
        ("REQUIRED", features::REQUIRED),
    ] {
        writeln!(header, "#define STELA_NET_F_{name} {value:#x}ULL").unwrap();
    }
    for (name, value) in [
        ("DESC_F_NEXT", ring::DESC_F_NEXT),
        ("DESC_F_WRITE", ring::DESC_F_WRITE),
        ("DESC_F_INDIRECT", ring::DESC_F_INDIRECT),
        ("AVAIL_F_NO_INTERRUPT", ring::AVAIL_F_NO_INTERRUPT),
        ("USED_F_NO_NOTIFY", ring::USED_F_NO_NOTIFY),
    ] {
        writeln!(header, "#define STELA_NET_{name} {value:#x}").unwrap();
    }
    writeln!(
        header,
        "#define STELA_NET_DESCRIPTOR_BYTES {}ULL",
        ring::DESCRIPTOR_BYTES
    )
    .unwrap();
    writeln!(
        header,
        "#define STELA_NET_USED_ALIGN {}ULL",
        ring::USED_ALIGN
    )
    .unwrap();
    // The same arithmetic as ring::avail, ring::used and ring::bytes.
    header.push_str(
        "#define STELA_NET_RING_AVAIL(size) (STELA_NET_DESCRIPTOR_BYTES * (size))\n\
         #define STELA_NET_RING_USED(size) \\\n\
         \t(((STELA_NET_RING_AVAIL(size) + 4 + 2 * (size) + 2 + STELA_NET_USED_ALIGN - 1) \\\n\
         \t/ STELA_NET_USED_ALIGN) * STELA_NET_USED_ALIGN)\n\
         #define STELA_NET_RING_BYTES(size) (STELA_NET_RING_USED(size) + 4 + 8 * (size) + 2)\n",
    );
    header
}

/// `stela/virtio.h`: the virtio-mmio register page, status and feature bits,
/// device IDs and split-ring sizes, from `stela_contracts::virtio`. Plain C,
/// so the kext's ring code compiles against it in the host's tests too.
pub fn virtio_header() -> String {
    use stela_contracts::virtio::{features, ids, mmio, net, ring, status};
    let mut header = String::from(
        "/* The virtio-mmio transport's layout, from stela_contracts::virtio. */\n\
         #pragma once\n",
    );
    for (prefix, layout) in [
        ("MMIO", mmio::LAYOUT),
        ("STATUS", status::LAYOUT),
        ("F", features::LAYOUT),
        ("ID", ids::LAYOUT),
        ("NET_F", net::features::LAYOUT),
        ("NET", net::LAYOUT),
    ] {
        for (name, value) in layout {
            writeln!(header, "#define STELA_VIRTIO_{prefix}_{name} {value:#x}ULL").unwrap();
        }
    }
    for (name, value) in [
        ("DESC_F_NEXT", ring::DESC_F_NEXT),
        ("DESC_F_WRITE", ring::DESC_F_WRITE),
        ("DESC_F_INDIRECT", ring::DESC_F_INDIRECT),
        ("AVAIL_F_NO_INTERRUPT", ring::AVAIL_F_NO_INTERRUPT),
        ("USED_F_NO_NOTIFY", ring::USED_F_NO_NOTIFY),
    ] {
        writeln!(header, "#define STELA_VIRTIO_{name} {value:#x}").unwrap();
    }
    for (name, value) in [
        ("DESCRIPTOR_BYTES", ring::DESCRIPTOR_BYTES),
        ("DESC_ALIGN", ring::DESC_ALIGN),
        ("AVAIL_ALIGN", ring::AVAIL_ALIGN),
        ("USED_ALIGN", ring::USED_ALIGN),
        ("QUEUE_MAX", ring::QUEUE_MAX),
    ] {
        writeln!(header, "#define STELA_VIRTIO_{name} {value}ULL").unwrap();
    }
    // The same arithmetic as ring::desc_bytes, avail_bytes and used_bytes.
    header.push_str(
        "#define STELA_VIRTIO_DESC_BYTES(size) (STELA_VIRTIO_DESCRIPTOR_BYTES * (size))\n\
         #define STELA_VIRTIO_AVAIL_BYTES(size) (4 + 2 * (size) + 2)\n\
         #define STELA_VIRTIO_USED_BYTES(size) (4 + 8 * (size) + 2)\n",
    );
    header
}

/// `stela/virtio_snd.h`: the sound device's queues, codes and message
/// layouts, from `stela_contracts::virtio_snd`.
pub fn virtio_snd_header() -> String {
    use stela_contracts::virtio_snd::{codes, messages};
    let mut header = String::from(
        "/* The virtio sound device's messages, from stela_contracts::virtio_snd. */\n\
         #pragma once\n",
    );
    for layout in [codes::LAYOUT, messages::LAYOUT] {
        for (name, value) in layout {
            writeln!(header, "#define STELA_VIRTIO_SND_{name} {value:#x}ULL").unwrap();
        }
    }
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_virtio_snd_header_names_codes_and_message_sizes() {
        let header = virtio_snd_header();
        for line in [
            "#define STELA_VIRTIO_SND_VQ_TX 0x2ULL\n",
            "#define STELA_VIRTIO_SND_R_PCM_SET_PARAMS 0x101ULL\n",
            "#define STELA_VIRTIO_SND_S_OK 0x8000ULL\n",
            "#define STELA_VIRTIO_SND_FMT_S16 0x5ULL\n",
            "#define STELA_VIRTIO_SND_RATE_48000 0x7ULL\n",
            "#define STELA_VIRTIO_SND_SET_PARAMS_BYTES 0x18ULL\n",
            "#define STELA_VIRTIO_SND_PCM_STATUS_BYTES 0x8ULL\n",
        ] {
            assert!(header.contains(line), "{line}");
        }
    }

    #[test]
    fn the_virtio_header_names_registers_bits_and_ring_sizes() {
        let header = virtio_header();
        for line in [
            "#define STELA_VIRTIO_MMIO_MAGIC 0x74726976ULL\n",
            "#define STELA_VIRTIO_MMIO_QUEUE_READY 0x44ULL\n",
            "#define STELA_VIRTIO_MMIO_CONFIG 0x100ULL\n",
            "#define STELA_VIRTIO_MMIO_INT_VRING 0x1ULL\n",
            "#define STELA_VIRTIO_STATUS_FEATURES_OK 0x8ULL\n",
            "#define STELA_VIRTIO_F_VERSION_1 0x100000000ULL\n",
            "#define STELA_VIRTIO_F_EVENT_IDX 0x20000000ULL\n",
            "#define STELA_VIRTIO_ID_RNG 0x4ULL\n",
            "#define STELA_VIRTIO_ID_NET 0x1ULL\n",
            "#define STELA_VIRTIO_NET_F_MRG_RXBUF 0x8000ULL\n",
            "#define STELA_VIRTIO_NET_F_STATUS 0x10000ULL\n",
            "#define STELA_VIRTIO_NET_CONFIG_STATUS 0x6ULL\n",
            "#define STELA_VIRTIO_NET_HEADER_BYTES 0xcULL\n",
            "#define STELA_VIRTIO_NET_HDR_F_NEEDS_CSUM 0x1ULL\n",
            "#define STELA_VIRTIO_DESC_F_WRITE 0x2\n",
            "#define STELA_VIRTIO_USED_ALIGN 4ULL\n",
            "#define STELA_VIRTIO_USED_BYTES(size) (4 + 8 * (size) + 2)\n",
        ] {
            assert!(header.contains(line), "{line}");
        }
    }

    const INTERFACE: &str = r#"#include <DriverKit/OSAction.iig>
class OSAction;
class IIG_SERIALIZABLE OSData;
enum { kCount = 16 };
/* class Hidden { }; */
class KERNEL Widget : public OSObject
{
public:
    /* braces in comments: { */
    virtual kern_return_t Start(IOService * provider) LOCAL; // }
    char brace = '}';
};
class EXTENDS (Widget) WidgetPrivate
{
    void Local() LOCALONLY;
};
typedef int After;
"#;

    #[test]
    fn classes_become_their_method_macros() {
        let header = kernel_header(INTERFACE).unwrap();
        assert!(header.starts_with("#include <DriverKit/OSAction.h>\nclass OSAction;\n"));
        assert!(header.contains("class IIG_SERIALIZABLE OSData;\nenum { kCount = 16 };\n"));
        assert!(header.contains("/* class Hidden { }; */\n\n\ntypedef int After;\n"));
        assert!(!header.contains("Start("));
        assert!(header.contains(
            "#define Widget_Methods public: virtual kern_return_t Dispatch(const IORPC rpc) \
             APPLE_KEXT_OVERRIDE; static kern_return_t _Dispatch(Widget * self, const IORPC rpc);\n\
             #define Widget_KernelMethods\n"
        ));
        assert!(header.contains("#define WidgetPrivate_KernelMethods\n"));
    }

    #[test]
    fn an_unclosed_class_is_refused() {
        assert!(kernel_header("class KERNEL Widget : public OSObject\n{\n").is_err());
    }

    #[test]
    fn the_net_header_names_registers_features_and_ring_layout() {
        let header = net_header();
        assert!(header.contains("#define STELA_NET_KICK 0x58ULL\n"));
        assert!(header.contains("#define STELA_NET_MAGIC 0x534e4554ULL\n"));
        assert!(header.contains("#define STELA_NET_F_VERSION_1 0x100000000ULL\n"));
        assert!(header.contains("#define STELA_NET_DESC_F_WRITE 0x2\n"));
        assert!(header.contains("#define STELA_NET_RING_BYTES(size)"));
    }

    #[test]
    fn the_gate_header_names_every_register() {
        let header = gate_header();
        assert!(header.contains("#define STELA_GATE_PAGE 0x4000ULL\n"));
        assert!(header.contains("#define STELA_GATE_ACK 0x28ULL\n"));
    }
}
