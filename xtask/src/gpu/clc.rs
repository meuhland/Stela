//! The build host's tools a Vulkan driver's build runs: `mesa_clc` and
//! `vtn_bindgen2`, built here for the build host from the same Mesa sources
//! as the image, which turn a driver's internal OpenCL kernels into SPIR-V
//! and that into the C++ that builds their NIR, and `glslangValidator`,
//! which compiles a driver's GLSL libraries to SPIR-V.
//!
//! What they need comes from the build host, as Meson finds it: LLVM with
//! Clang's development files, SPIRV-LLVM-Translator for that LLVM and
//! SPIRV-Tools for `mesa_clc`, and glslang. A host without them builds the
//! drivers that do not need them; [`tools`] says why once.
use super::support::{build_directory, capture, third_party, tool};
use super::toolchain::GENERATORS;
use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Mesa for the build host with nothing but its OpenCL C compiler and the
/// SPIR-V binding generator. The OpenCL headers are compiled in, so the tool
/// does not depend on where the host keeps Clang's resource directory.
const OPTIONS: [&str; 26] = [
    "-Dbuildtype=release",
    "-Dmesa-clc=enabled",
    "-Dinstall-mesa-clc=true",
    "-Dmesa-clc-bundle-headers=enabled",
    "-Dprecomp-compiler=auto",
    "-Dllvm=enabled",
    "-Dshared-llvm=enabled",
    "-Dvulkan-drivers=",
    "-Dgallium-drivers=",
    "-Dplatforms=",
    "-Dopengl=false",
    "-Dgles1=disabled",
    "-Dgles2=disabled",
    "-Dglx=disabled",
    "-Degl=disabled",
    "-Dgbm=disabled",
    "-Dzlib=disabled",
    "-Dzstd=disabled",
    "-Dshader-cache=disabled",
    "-Dvulkan-layers=",
    "-Dtools=",
    "-Dvideo-codecs=",
    "-Dexpat=disabled",
    "-Dlibunwind=disabled",
    "-Dvalgrind=disabled",
    "-Dbuild-tests=false",
];
/// The two programs, as Meson's targets.
const PROGRAMS: [&str; 2] = [
    "src/compiler/clc/mesa_clc",
    "src/compiler/spirv/vtn_bindgen2",
];
/// The oldest glslang Mesa's configuration accepts for the drivers that build
/// acceleration structures (`_glslang_preamble_version`).
const GLSLANG_MINIMUM: (u32, u32) = (12, 2);

/// The host tools, found or built.
pub(super) struct Tools {
    /// `mesa_clc` and `vtn_bindgen2`.
    pub(super) programs: Vec<PathBuf>,
    /// `glslangValidator`, with the version it reports.
    pub(super) glslang: (PathBuf, String),
}

impl Tools {
    /// What the tools are, for Mesa's configuration record: a different
    /// glslang or a rebuilt tool configures Mesa again.
    pub(super) fn identity(&self) -> String {
        let mut identity = format!("glslang {} {}\n", self.glslang.0.display(), self.glslang.1);
        for program in &self.programs {
            let length = fs::metadata(program).map_or(0, |file| file.len());
            identity.push_str(&format!("{} {length}\n", program.display()));
        }
        identity
    }
}

/// The host tools, or why this host cannot have them. `path` is the PATH the
/// GPU builds run with.
pub(super) fn tools(path: &Path, jobs: u16) -> Result<std::result::Result<Tools, String>> {
    let glslang = match glslang(path) {
        Ok(glslang) => glslang,
        Err(reason) => return Ok(Err(reason)),
    };
    let programs = match programs(path, jobs)? {
        Ok(programs) => programs,
        Err(reason) => return Ok(Err(reason)),
    };
    Ok(Ok(Tools { programs, glslang }))
}

/// The first `glslangValidator` on `path` that Mesa accepts.
fn glslang(path: &Path) -> std::result::Result<(PathBuf, String), String> {
    let found = std::env::split_paths(path)
        .map(|directory| {
            directory.join(format!("glslangValidator{}", std::env::consts::EXE_SUFFIX))
        })
        .find(|candidate| candidate.is_file())
        .ok_or("no glslangValidator on PATH")?;
    let reported = capture(Command::new(&found).arg("--version"))
        .map_err(|error| format!("{} --version failed: {error}", found.display()))?;
    let version = glslang_version(&reported)
        .ok_or_else(|| format!("{} reports no version", found.display()))?;
    let (major, minor) = GLSLANG_MINIMUM;
    if numbers(&version) < (major, minor) {
        return Err(format!(
            "{} is glslang {version}; Mesa needs {major}.{minor} or newer",
            found.display()
        ));
    }
    Ok((found, version))
}

/// The version in `glslangValidator --version`'s first line, as Mesa's
/// configuration reads it: "Glslang Version: 11:16.2.0" is 16.2.0.
fn glslang_version(reported: &str) -> Option<String> {
    let line = reported.lines().next()?;
    let version = line.split(':').nth(2)?.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

fn numbers(version: &str) -> (u32, u32) {
    let mut parts = version.split('.').map(|part| part.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// `mesa_clc` and `vtn_bindgen2`, built under `target/gpu/clc`, or why
/// Meson cannot configure them on this host.
fn programs(path: &Path, jobs: u16) -> Result<std::result::Result<Vec<PathBuf>, String>> {
    let directory = build_directory("clc");
    let build = directory.join("build");
    let record = directory.join("configuration");
    let arguments = OPTIONS.join("\n") + "\n";
    fs::create_dir_all(&directory)?;
    let current = build.join("build.ninja").is_file()
        && fs::read_to_string(&record).is_ok_and(|recorded| recorded == arguments);
    if !current {
        if build.is_dir() {
            fs::remove_dir_all(&build)?;
        }
        let _ = fs::remove_file(&record);
        // The host's own pkg-config, which knows where its libraries are,
        // else the program the pkg-config wheel wraps: the wheel's launcher,
        // first on `path`, loses its output to Meson.
        let pkg_config = match crate::tools::which("pkg-config") {
            Some(host) => super::support::slashed(&host),
            None => super::toolchain::pkg_config()?,
        };
        let native = directory.join("native.ini");
        fs::write(
            &native,
            format!("[binaries]\npkg-config = '{pkg_config}'\n"),
        )?;
        let output = Command::new(tool("meson", GENERATORS)?)
            .arg("setup")
            .arg(&build)
            .arg(third_party("mesa"))
            .arg("--native-file")
            .arg(&native)
            .args(OPTIONS)
            .env("PATH", path)
            .output()?;
        if !output.status.success() {
            let log = String::from_utf8_lossy(&output.stdout);
            let kept = directory.join("meson-log.txt");
            let _ = fs::copy(build.join("meson-logs/meson-log.txt"), &kept);
            return Ok(Err(format!(
                "mesa_clc cannot be built here ({}; Meson's log is {})",
                meson_error(&log).unwrap_or("Meson's configuration failed"),
                kept.display()
            )));
        }
        fs::write(&record, &arguments)?;
    }
    // A host whose libraries Meson found but cannot compile against (a
    // pkg-config file naming headers that are not there) fails here, not in
    // the configuration, and builds the drivers that do not need the tools.
    let built = Command::new(tool("ninja", GENERATORS)?)
        .arg(format!("-j{jobs}"))
        .args(PROGRAMS)
        .current_dir(&build)
        .env("PATH", path)
        .output()?;
    if !built.status.success() {
        // Configured again next time, as the host may have changed.
        let _ = fs::remove_file(&record);
        let kept = directory.join("ninja-log.txt");
        let _ = fs::write(&kept, [&built.stdout[..], &built.stderr[..]].concat());
        let output = String::from_utf8_lossy(&built.stdout);
        return Ok(Err(format!(
            "mesa_clc cannot be built here ({}; the build's output is {})",
            compile_error(&output).unwrap_or("its build failed"),
            kept.display()
        )));
    }
    Ok(Ok(PROGRAMS
        .iter()
        .map(|program| build.join(format!("{program}{}", std::env::consts::EXE_SUFFIX)))
        .collect()))
}

/// The error Meson stopped at, from its output: the text after "ERROR: ".
fn meson_error(output: &str) -> Option<&str> {
    output
        .lines()
        .find_map(|line| line.split_once("ERROR: ").map(|(_, error)| error.trim()))
}

/// The first compiler error in a build's output, as the compiler printed it.
fn compile_error(output: &str) -> Option<&str> {
    output
        .lines()
        .find(|line| line.contains("error: "))
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glslangs_version_is_read_as_mesa_reads_it() {
        assert_eq!(
            glslang_version("Glslang Version: 11:16.2.0\nESSL Version: x\n").as_deref(),
            Some("16.2.0")
        );
        assert_eq!(glslang_version("Glslang Version: 11"), None);
        assert_eq!(glslang_version(""), None);
        assert_eq!(numbers("16.2.0"), (16, 2));
        assert!(numbers("12.1.0") < GLSLANG_MINIMUM);
        assert!(numbers("12.2") >= GLSLANG_MINIMUM);
    }

    #[test]
    fn mesons_stopping_error_is_named() {
        let output = "Run-time dependency SPIRV-Tools found: YES\n\
                      meson.build:2092:22: ERROR: Dependency \"LLVMSPIRVLib\" not found, tried pkgconfig\n\
                      \nA full log can be found at x\n";
        assert_eq!(
            meson_error(output),
            Some("Dependency \"LLVMSPIRVLib\" not found, tried pkgconfig")
        );
        assert_eq!(meson_error("fine\n"), None);
    }

    #[test]
    fn a_builds_first_compiler_error_is_named() {
        let output = "[12/80] Compiling C object src/util/libmesa_util.a.p/u_sync_provider.c.o\n\
                      FAILED: src/util/libmesa_util.a.p/u_sync_provider.c.o\n\
                      ../src/util/u_sync_provider.c:8:10: fatal error: xf86drm.h: No such file or directory\n\
                      compilation terminated.\n";
        assert_eq!(
            compile_error(output),
            Some(
                "../src/util/u_sync_provider.c:8:10: fatal error: xf86drm.h: No such file or directory"
            )
        );
        assert_eq!(compile_error("[1/1] Linking target mesa_clc\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn an_old_or_missing_glslang_is_named() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        assert!(
            glslang(directory.path())
                .unwrap_err()
                .contains("no glslangValidator")
        );
        let program = directory.path().join("glslangValidator");
        let write = |version: &str| {
            fs::write(
                &program,
                format!("#!/bin/sh\necho 'Glslang Version: 11:{version}'\n"),
            )
            .unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        };
        write("11.15.1");
        assert!(
            glslang(directory.path())
                .unwrap_err()
                .contains("needs 12.2")
        );
        write("16.2.0");
        assert_eq!(
            glslang(directory.path()).unwrap(),
            (program.clone(), "16.2.0".to_owned())
        );
    }
}
