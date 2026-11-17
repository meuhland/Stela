//! RADV on amdgpu without a GPU, on the build host: Mesa built for this
//! machine with the image's RADV options (`nvk::RADV`), its objects linked
//! into a test program as Mesa links its driver, with Stela's libdrm
//! stand-in (`compat/mesa/drm.c`) in libdrm's place, libdrm_amdgpu built from
//! the same unchanged sources as in the image, and Stela's NIR options hook,
//! run over Mesa's own amdgpu no-op DRM shim (`src/amd/drm-shim`), which
//! answers amdgpu's queries from real devices' records and accepts buffers,
//! binds and submissions without running them. For each device the test
//! names, the program creates RADV's device, builds a compute pipeline
//! through ACO and submits one dispatch, then builds one from NIR given as
//! the Metal device gives it (`amd/tests/radv_device.c`); every amdgpu ioctl
//! RADV made must be one Stela's amdgpu registers.
//!
//! Needs what RADV's image build needs (`clc.rs`: glslang, and mesa_clc
//! for Mesa's configuration) and the host's `libdrm.so.2`.
use super::hosted;
use super::nvk::{LIBDRM_VERSION, RADV};
use super::support::{build_directory, gpu, slashed, third_party, tool};
use super::toolchain::GENERATORS;
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Each device the test runs, by the shim's name for it (`AMDGPU_GPU_ID`,
/// `amdgpu_devices.c`), what it is, and whether Vulkan must report it
/// discrete (`true`) or integrated. The first is the owner's PC's graphics
/// core (GC 10.3.6); the others cover each ACO generation from GFX8.
const DEVICES: [(&str, &str, bool); 9] = [
    (
        "raphael_mendocino",
        "Ryzen 7000/9000 graphics, GFX10.3",
        false,
    ),
    ("rembrandt", "Ryzen 6000 graphics, GFX10.3", false),
    ("phoenix", "Ryzen 7040 graphics, GFX11", false),
    ("strix1", "Ryzen AI 300 graphics, GFX11.5", false),
    ("renoir", "Ryzen 4000 graphics, GFX9", false),
    ("navi21", "Radeon RX 6800/6900, GFX10.3", true),
    ("navi31", "Radeon RX 7900, GFX11", true),
    ("gfx1201", "Radeon RX 9070, GFX12", true),
    ("polaris10", "Radeon RX 480/580, GFX8", true),
];
/// The program's exit line naming the amdgpu ioctls RADV made.
const USED: &str = "amdgpu ioctls:";
/// Mesa's amdgpu no-op shim library, whose link the test runs itself: the
/// shim's libdrm calls are the program's, which Meson's link refuses as
/// undefined.
const SHIM: &str = "src/amd/drm-shim/libamdgpu_noop_drm_shim.so";

pub fn test(jobs: u16) -> Result<()> {
    let directory = build_directory("radv-host");
    let build = directory.join("build");
    let output = directory.join("test");
    let bin = directory.join("bin");
    let packages = directory.join("packages");
    let local = crate::tools::root().join(".tools/gpu");
    let host = std::env::join_paths(
        [local.join(GENERATORS), local.join("rust/bin")]
            .into_iter()
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )),
    )?;
    let tools = match super::clc::tools(Path::new(&host), jobs)? {
        Ok(tools) => tools,
        Err(reason) => bail!("RADV cannot be built on this host: {reason}"),
    };
    for path in [&output, &bin, &packages] {
        fs::create_dir_all(path)?;
    }
    for program in &tools.programs {
        fs::copy(program, bin.join(program.file_name().context("host tool")?))?;
    }
    let path =
        std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(&host)))?;
    let libdrm = third_party("libdrm");
    ensure!(
        libdrm.join("xf86drm.h").is_file() && libdrm.join("amdgpu/amdgpu_device.c").is_file(),
        "third_party/libdrm is missing or lacks libdrm_amdgpu; run `cargo xtask gpu fetch`"
    );
    // libdrm's headers as the image has them; its calls are Stela's, and
    // libdrm_amdgpu is built here from its sources, as for the image.
    fs::write(
        packages.join("libdrm.pc"),
        format!(
            "Name: libdrm\nDescription: vendored headers\nVersion: {LIBDRM_VERSION}\n\
             Cflags: -I{0} -I{0}/include/drm\n",
            slashed(&libdrm)
        ),
    )?;
    fs::write(
        packages.join("libdrm_amdgpu.pc"),
        format!(
            "Name: libdrm_amdgpu\nDescription: built from its sources\nVersion: {LIBDRM_VERSION}\n\
             Cflags: -I{0} -I{0}/amdgpu -I{0}/include/drm\n",
            slashed(&libdrm)
        ),
    )?;
    let arguments = hosted::arguments(RADV.name, RADV.options);
    hosted::configure(&directory, &build, &packages, &path, &arguments)?;
    let link = hosted::link_command(&build, RADV.target)?;
    let shim_link = hosted::link_command(&build, SHIM)?;
    let mut targets = hosted::built_inputs(&link);
    targets.extend(hosted::built_inputs(&shim_link));
    crate::tools::exec(
        Command::new(tool("ninja", GENERATORS)?)
            .arg("-C")
            .arg(&build)
            .arg(format!("-j{jobs}"))
            .args(&targets)
            .env("PATH", &path),
    )?;
    let program = link_test(&build, &output, &link, &tools.glslang.0)?;
    let registered = hosted::registered_ioctls(
        &fs::read_to_string(third_party("linux").join("drivers/gpu/drm/amd/amdgpu/amdgpu_drv.c"))?,
        "AMDGPU_",
    );
    ensure!(!registered.is_empty(), "amdgpu_drv.c registers no ioctl");
    let shim = output.join("libamdgpu_noop_drm_shim.so");
    let words = hosted::preloaded_link(&shim_link, &shim);
    crate::tools::exec(
        Command::new(&words[0])
            .current_dir(&build)
            .args(&words[1..]),
    )?;
    let mut failures = Vec::new();
    for (device, name, discrete) in DEVICES {
        let run = Command::new("timeout")
            .arg("120")
            .arg(&program)
            .arg(if discrete { "discrete" } else { "integrated" })
            .env("LD_PRELOAD", &shim)
            .env("AMDGPU_GPU_ID", device)
            .stdin(Stdio::null())
            .output()?;
        let stdout = String::from_utf8_lossy(&run.stdout);
        let stderr = String::from_utf8_lossy(&run.stderr);
        fs::write(
            output.join(format!("{device}.log")),
            format!("{stdout}{stderr}"),
        )?;
        let used = hosted::used_ioctls(&stdout, USED);
        let unregistered: Vec<_> = used
            .keys()
            .filter(|ioctl| !registered.contains(ioctl))
            .cloned()
            .collect();
        let summary = stdout
            .lines()
            .find(|line| line.starts_with("RADV on amdgpu:"));
        match (run.status.success(), summary, unregistered.is_empty()) {
            (true, Some(summary), true) if !used.is_empty() => {
                println!("{device} ({name}): {summary}");
                println!(
                    "  amdgpu ioctls: {}",
                    used.iter()
                        .map(|(ioctl, count)| format!("{ioctl} {count}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            (true, _, false) => failures.push(format!(
                "{device} ({name}): RADV used ioctls Stela's amdgpu does not register: {}",
                unregistered.join(", ")
            )),
            _ => failures.push(format!(
                "{device} ({name}) failed with {}: {}",
                run.status,
                stderr.trim()
            )),
        }
    }
    ensure!(
        failures.is_empty(),
        "RADV on amdgpu failed on the host (logs in {}):\n{}",
        output.display(),
        failures.join("\n")
    );
    println!(
        "RADV ran on amdgpu's ioctls for {} devices, with no GPU",
        DEVICES.len()
    );
    Ok(())
}

/// Builds the test program; its path.
fn link_test(build: &Path, output: &Path, link: &[String], glslang: &Path) -> Result<PathBuf> {
    let sources = gpu();
    crate::tools::exec(
        Command::new(glslang)
            .args([
                "-V",
                "--target-env",
                "vulkan1.1",
                "--vn",
                "stela_radv_device_spirv",
            ])
            .arg("-o")
            .arg(output.join("radv_device_shader.h"))
            .arg(sources.join("amd/tests/radv_device.comp"))
            .stdout(Stdio::null()),
    )?;
    let libc = format!("-idirafter{}", sources.join("compat/libc").display());
    let mut units = vec![
        (
            sources.join("compat/mesa/radv_nir_options.c"),
            vec!["-Wno-missing-prototypes".to_owned()],
        ),
        // Exported, as in Intel's test, so the shim's libdrm calls reach
        // them. RADV's units refuse variable-length arrays, which libdrm's
        // command macros take the size of; NVK's flags, which the image
        // compiles this unit with, allow them.
        (
            sources.join("compat/mesa/drm.c"),
            vec![
                libc.clone(),
                "-fvisibility=default".to_owned(),
                "-Wno-vla".to_owned(),
            ],
        ),
        (
            sources.join("amd/tests/radv_device.c"),
            vec![libc, format!("-I{}", output.display())],
        ),
    ];
    for library in RADV.libraries {
        let defines: Vec<String> = library
            .defines
            .iter()
            .map(|define| format!("-D{define}"))
            .collect();
        units.extend(
            library
                .sources
                .iter()
                .map(|source| (third_party(source), defines.clone())),
        );
    }
    let mut objects = Vec::new();
    for (source, flags) in units {
        let object = output.join(
            source
                .with_extension("o")
                .file_name()
                .context("unit name")?,
        );
        crate::tools::exec(
            hosted::compile_command(build, RADV.model, &source, &object)?.args(flags),
        )?;
        objects.push(object);
    }
    let program = output.join("radv_device");
    // Every ioctl the linked code makes passes the program's count first.
    let extra = [
        "-Wl,--wrap=ioctl".to_owned(),
        hosted::host_libdrm()?.display().to_string(),
    ];
    let words = hosted::program_link(link, &program, &objects, &extra);
    crate::tools::exec(Command::new(&words[0]).current_dir(build).args(&words[1..]))?;
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_devices_are_distinct_and_the_owners_core_comes_first() {
        assert_eq!(DEVICES[0].0, "raphael_mendocino");
        for (index, (device, _, _)) in DEVICES.iter().enumerate() {
            assert!(DEVICES[..index].iter().all(|(other, _, _)| other != device));
        }
        assert!(DEVICES.iter().any(|&(_, _, discrete)| discrete));
        assert!(DEVICES.iter().any(|&(_, _, discrete)| !discrete));
    }

    #[test]
    fn radvs_test_build_is_the_images_radv() {
        let arguments = hosted::arguments(RADV.name, RADV.options);
        assert!(arguments.contains(&"-Dvulkan-drivers=amd".to_owned()));
        assert!(arguments.contains(&"-Damd-use-llvm=false".to_owned()));
        assert_eq!(RADV.libraries[0].name, "libdrm_amdgpu");
    }
}
