//! IOAudioFamily, for K5's sound: the device and engine classes
//! StelaVirtIOSound derives from, the stream its engine adds and the
//! controls it makes. Its header and manifest are
//! `family/audio/IOAudioFamily.{h,slots}`, kept apart from the other
//! families' so each device's family merges on its own.
//!
//! IOAudioEngine has two pure virtual slots, 333 and 389, which the
//! collection cannot name. Apple's own USB audio driver in the same
//! collection, `AppleUSBAudioEngine : IOAudioEngine`
//! (`com.apple.driver.AppleUSBAudio`), overrides both, and its exported
//! overrides name them.
use super::{Family, Pure};

/// The fileset entry of the driver whose engine names IOAudioEngine's pure
/// slots.
const USB_AUDIO: &str = "com.apple.driver.AppleUSBAudio";

pub const FAMILY: Family = Family {
    header: "audio/IOAudioFamily.h",
    manifest: "audio/IOAudioFamily.slots",
    entry: "com.apple.iokit.IOAudioFamily",
    classes: &[
        "IOAudioDevice",
        "IOAudioEngine",
        "IOAudioStream",
        "IOAudioControl",
        "IOAudioLevelControl",
        "IOAudioToggleControl",
    ],
    pure: &[
        Pure {
            class: "IOAudioEngine",
            from: "AppleUSBAudioEngine",
            entry: Some(USB_AUDIO),
            first: 333,
            count: 1,
            trailing: "",
        },
        Pure {
            class: "IOAudioEngine",
            from: "AppleUSBAudioEngine",
            entry: Some(USB_AUDIO),
            first: 389,
            count: 1,
            trailing: "",
        },
    ],
};

#[cfg(test)]
mod tests {
    use super::super::{check_probe, directory, object_vtables};
    use super::*;
    use std::{fs, path::Path, process::Command};
    use stela_loader::aux_kc::manifest::Manifest;

    /// The probes of `audio/tests/audio_probe.cpp`, each with the family
    /// class it derives from.
    const PROBES: [(&str, &str); 6] = [
        ("StelaAudioProbeDevice", "IOAudioDevice"),
        ("StelaAudioProbeEngine", "IOAudioEngine"),
        ("StelaAudioProbeStream", "IOAudioStream"),
        ("StelaAudioProbeControl", "IOAudioControl"),
        ("StelaAudioProbeLevel", "IOAudioLevelControl"),
        ("StelaAudioProbeToggle", "IOAudioToggleControl"),
    ];

    fn committed() -> Manifest {
        let text = fs::read_to_string(directory().join(FAMILY.manifest)).unwrap();
        Manifest::parse(&text).unwrap()
    }

    /// The command that compiles `source` into `built/object.o` as the
    /// kext's sources are compiled, with the family headers of `headers`
    /// (which has `audio/IOAudioFamily.h`).
    fn compile_command(source: &Path, headers: &Path, built: &Path) -> Command {
        let root = crate::tools::root();
        let xnu = root.join("third_party/xnu");
        let include = built.join("include");
        crate::guest::sdk::generate(&xnu, &include).unwrap();
        let clang = &crate::clang::toolset().unwrap().clangxx;
        let mut command = Command::new(clang);
        command
            .args(crate::guest::KEXT_FLAGS)
            .arg(format!("--target={}", crate::guest::TARGET))
            .arg("-I")
            .arg(headers);
        for headers in crate::guest::XNU_HEADERS {
            command.arg("-isystem").arg(xnu.join(headers));
        }
        command.arg("-isystem").arg(&include);
        command
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(built.join("object.o"));
        command
    }

    fn compile(source: &Path, headers: &Path) -> Vec<u8> {
        let built = tempfile::tempdir().unwrap();
        crate::tools::exec(&mut compile_command(source, headers, built.path())).unwrap();
        fs::read(built.path().join("object.o")).unwrap()
    }

    fn probe_object(headers: &Path) -> Vec<u8> {
        compile(&directory().join("audio/tests/audio_probe.cpp"), headers)
    }

    #[test]
    fn the_audio_header_puts_every_method_in_the_kernels_slot() {
        let vtables = object_vtables(&probe_object(&directory())).unwrap();
        let manifest = committed();
        for (probe, class) in PROBES {
            check_probe(&vtables, &manifest, probe, class).unwrap();
        }
        // The engine's pure slots are the probe's own methods.
        let engine = &vtables["__ZTV21StelaAudioProbeEngine"];
        assert_eq!(
            engine[333].as_deref(),
            Some("__ZN21StelaAudioProbeEngine21getCurrentSampleFrameEv")
        );
        assert_eq!(
            engine[389].as_deref(),
            Some("__ZN21StelaAudioProbeEngine33driverDesiresHiResSampleIntervalsEv")
        );
    }

    /// The kext's sound driver: the device's and the engine's vtables, slot
    /// by slot, and its overrides where the kernel calls them.
    #[test]
    fn the_sound_driver_overrides_the_kernels_slots() {
        let source = crate::tools::root().join("guest/stela-virtio/kext/Sound.cpp");
        let vtables = object_vtables(&compile(&source, &directory())).unwrap();
        let manifest = committed();
        check_probe(&vtables, &manifest, "StelaVirtIOSound", "IOAudioDevice").unwrap();
        check_probe(
            &vtables,
            &manifest,
            "StelaVirtIOSoundEngine",
            "IOAudioEngine",
        )
        .unwrap();
        let engine = &vtables["__ZTV22StelaVirtIOSoundEngine"];
        let own = |slot: usize, method: &str| {
            assert_eq!(
                engine[slot].as_deref(),
                Some(format!("__ZN22StelaVirtIOSoundEngine{method}").as_str()),
                "slot {slot}"
            );
        };
        own(322, "12initHardwareEP9IOService");
        own(333, "21getCurrentSampleFrameEv");
        own(338, "23performAudioEngineStartEv");
        own(339, "22performAudioEngineStopEv");
        own(
            351,
            "17clipOutputSamplesEPKvPvjjPK20_IOAudioStreamFormatP13IOAudioStream",
        );
        own(389, "33driverDesiresHiResSampleIntervalsEv");
        let device = &vtables["__ZTV16StelaVirtIOSound"];
        assert_eq!(
            device[304].as_deref(),
            Some("__ZN16StelaVirtIOSound12initHardwareEP9IOService")
        );
        assert_eq!(
            device[77].as_deref(),
            Some("__ZN16StelaVirtIOSound4initEP12OSDictionary")
        );
    }

    /// K5's silence: a stream given a format by the one-function
    /// `addAvailableFormat` with no function keeps a list of one null IO
    /// function, which IOAudioStream::clipOutputSamples calls instead of
    /// the engine's clipOutputSamples, so nothing is ever clipped [kc]. The
    /// header has no default for that function, so the three-argument call
    /// does not compile, and the list form with no list does.
    #[test]
    fn a_format_without_an_io_function_list_is_the_only_form_that_compiles() {
        let built = tempfile::tempdir().unwrap();
        let source = |body: &str| {
            format!(
                "#include <audio/IOAudioFamily.h>\n\
                 void addFormat(IOAudioStream *stream, const IOAudioStreamFormat *format,\n\
                 \x20   const IOAudioSampleRate *rate)\n{{\n\t{body}\n}}\n"
            )
        };
        let silent = built.path().join("silent.cpp");
        fs::write(
            &silent,
            source("stream->addAvailableFormat(format, rate, rate);"),
        )
        .unwrap();
        let output = compile_command(&silent, &directory(), built.path())
            .output()
            .unwrap();
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{errors}");
        assert!(errors.contains("addAvailableFormat"), "{errors}");

        let listless = built.path().join("listless.cpp");
        fs::write(
            &listless,
            source(
                "stream->addAvailableFormat(format, rate, rate, \
                 static_cast<const AudioIOFunction *>(nullptr), 0);",
            ),
        )
        .unwrap();
        let output = compile_command(&listless, &directory(), built.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Two of the engine's latency setters swapped: the probe's vtable
    /// shows it at both slots.
    #[test]
    fn an_audio_header_with_two_methods_swapped_is_caught() {
        let header = fs::read_to_string(directory().join(FAMILY.header)).unwrap();
        let output = "\tvirtual void setOutputSampleLatency(UInt32 numSamples);\n";
        let input = "\tvirtual void setInputSampleLatency(UInt32 numSamples);\n";
        assert!(header.contains(&format!("{output}{input}")));
        let swapped = header.replace(&format!("{output}{input}"), &format!("{input}{output}"));
        let headers = tempfile::tempdir().unwrap();
        fs::create_dir(headers.path().join("audio")).unwrap();
        fs::write(headers.path().join(FAMILY.header), swapped).unwrap();
        let vtables = object_vtables(&probe_object(headers.path())).unwrap();
        let error = check_probe(
            &vtables,
            &committed(),
            "StelaAudioProbeEngine",
            "IOAudioEngine",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("slot 371: the header has __ZN13IOAudioEngine21setInputSampleLatencyEj")
                && error.contains(
                    "slot 372: the header has __ZN13IOAudioEngine22setOutputSampleLatencyEj"
                ),
            "{error}"
        );
    }
}
