//! `org.stela.StelaNet`, the launchd job that configures `stn0`, StelaNet's
//! interface (`--guest-network-stelanet`). `stn0` is a plain ifnet, not an
//! IONetworkInterface, so configd offers it as no network service and
//! nothing configures it unless asked. The kext installs the job's plist
//! beside the sandbox patcher's (`kext/sandbox_cache.cpp`); launchd loads it
//! from the next boot on.
//!
//! The job is `/bin/sh` and the tools macOS ships, started by launchd. A
//! process of ours that is not in the trust cache, such as the sandbox
//! patcher, may not start them: AMFI refuses the launch (Launch Constraint
//! Violation), so only launchd starts the script.
use anyhow::Result;

/// Where the kext installs the job, on the Data volume.
pub const JOB_PATH: &str = "/System/Volumes/Data/Library/LaunchDaemons/org.stela.StelaNet.plist";
const LABEL: &str = "org.stela.StelaNet";
const LOG: &str = "/Library/Logs/org.stela.StelaNet.log";
/// How long the script waits for `stn0` after launchd starts it, in seconds:
/// without it (the option off) it then exits for the rest of the boot.
const WAIT_SECONDS: u32 = 60;
/// How long without a DHCP address before DHCP is asked for again, in
/// seconds; IPConfiguration retries on its own meanwhile.
const RETRY_SECONDS: u32 = 300;
/// How often the script looks at `stn0`'s address, in seconds.
const POLL_SECONDS: u32 = 10;

/// The job's script, for `/bin/sh` as root from boot. It waits a minute
/// for `stn0` and exits when there is none. While `stn0` exists it asks
/// IPConfiguration for DHCP and IPv6 autoconfiguration once, and again after
/// [`RETRY_SECONDS`] without a DHCP address, and logs every address `stn0`
/// gets with its router and DNS servers.
pub fn script() -> String {
    format!(
        r#"log() {{ echo "$(/bin/date '+%Y-%m-%d %H:%M:%S') $*"; }}
waited=0
until /sbin/ifconfig stn0 >/dev/null 2>&1; do
  [ "$waited" -ge {WAIT_SECONDS} ] && exit 0
  /bin/sleep 2
  waited=$((waited + 2))
done
log "stn0 is present"
applied=0
last=none
while /sbin/ifconfig stn0 >/dev/null 2>&1; do
  now=$(/bin/date +%s)
  address=$(/usr/sbin/ipconfig getifaddr stn0 2>/dev/null)
  case "$address" in
  ""|169.254.*)
    if [ "$applied" = 0 ] || [ $((now - applied)) -ge {RETRY_SECONDS} ]; then
      log "stn0 has no DHCP address${{address:+ ($address)}}: ipconfig set stn0 DHCP"
      /sbin/ifconfig stn0 up
      if /usr/sbin/ipconfig set stn0 DHCP; then
        applied=$now
        /usr/sbin/ipconfig set stn0 AUTOMATIC-V6 || log "ipconfig set stn0 AUTOMATIC-V6 failed"
      else
        log "ipconfig set stn0 DHCP failed: $?"
      fi
    fi
    address=none
    ;;
  esac
  if [ "$address" != "$last" ]; then
    if [ "$address" = none ]; then
      log "stn0 lost its address $last"
    else
      log "stn0 $address router $(/usr/sbin/ipconfig getoption stn0 router 2>/dev/null) dns $(/usr/sbin/ipconfig getoption stn0 domain_name_server 2>/dev/null)"
    fi
    last=$address
  fi
  /bin/sleep {POLL_SECONDS}
done
log "stn0 is gone"
"#
    )
}

/// The job's launchd plist: started at load, started again only if the
/// script fails, its output in [`LOG`].
pub fn job() -> Result<Vec<u8>> {
    let mut job = plist::Dictionary::new();
    job.insert("Label".into(), LABEL.into());
    job.insert(
        "ProgramArguments".into(),
        plist::Value::Array(vec!["/bin/sh".into(), "-c".into(), script().into()]),
    );
    job.insert("RunAtLoad".into(), true.into());
    let mut keep_alive = plist::Dictionary::new();
    keep_alive.insert("SuccessfulExit".into(), false.into());
    job.insert("KeepAlive".into(), keep_alive.into());
    job.insert("StandardOutPath".into(), LOG.into());
    job.insert("StandardErrorPath".into(), LOG.into());
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &plist::Value::Dictionary(job))?;
    Ok(bytes)
}

/// `stela/stelanet_job.h`: the job's plist as the kext installs it, with
/// its path and SHA-256.
pub fn header() -> Result<String> {
    use std::fmt::Write as _;
    let bytes = job()?;
    let mut header = String::from("#pragma once\n");
    writeln!(header, "#define STELANET_JOB_PATH {JOB_PATH:?}")?;
    super::c_bytes(&mut header, "stelanet_job", &bytes)?;
    writeln!(
        header,
        "static const unsigned char stelanet_job_sha256[32] = {{{}}};",
        super::c_digest(&bytes)
    )?;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    #[test]
    fn the_job_runs_the_script_with_sh_from_launchd() {
        let job: plist::Value = plist::from_bytes(&job().unwrap()).unwrap();
        let job = job.as_dictionary().unwrap();
        assert_eq!(job["Label"].as_string(), Some(LABEL));
        let arguments = job["ProgramArguments"].as_array().unwrap();
        assert_eq!(arguments[0].as_string(), Some("/bin/sh"));
        assert_eq!(arguments[1].as_string(), Some("-c"));
        assert_eq!(arguments[2].as_string(), Some(script().as_str()));
        assert_eq!(job["RunAtLoad"].as_boolean(), Some(true));
        let keep_alive = job["KeepAlive"].as_dictionary().unwrap();
        assert_eq!(keep_alive["SuccessfulExit"].as_boolean(), Some(false));
        assert!(JOB_PATH.len() < 256, "the kext's paths are char[256]");
        let header = header().unwrap();
        assert!(header.contains(&format!("#define STELANET_JOB_PATH \"{JOB_PATH}\"")));
    }

    #[test]
    fn the_script_parses_and_names_every_tool_by_path() {
        let script = script();
        // A minute's wait for stn0, then nothing without it.
        assert!(script.contains("[ \"$waited\" -ge 60 ] && exit 0"));
        // Outside the quoted log messages, every tool goes by its path:
        // launchd gives the job a minimal PATH.
        for line in script.lines() {
            let commands: String = line.split('"').step_by(2).collect();
            for word in commands.split([' ', '(', '$', ';']) {
                assert!(
                    !["date", "sleep", "ifconfig", "ipconfig"].contains(&word),
                    "{word} without its path: {line}"
                );
            }
        }
        assert!(script.contains("/usr/sbin/ipconfig set stn0 DHCP"));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stelanet.sh");
        fs::write(&path, &script).unwrap();
        let status = Command::new("sh").arg("-n").arg(&path).status().unwrap();
        assert!(status.success());
    }

    /// The sandbox patcher runs outside the trust cache: AMFI refuses any
    /// platform tool it starts (Launch Constraint Violation), which is why
    /// this job exists. Its libSystem stubs must not grow a way to start one.
    #[test]
    fn the_sandbox_patcher_starts_no_process() {
        let stubs = include_str!("../../../guest/stela-gpu/sandbox/libSystem.tbd");
        for call in [
            "_posix_spawn",
            "_posix_spawnp",
            "_fork",
            "_vfork",
            "_execve",
            "_execv",
            "_execvp",
            "_execl",
            "_system",
            "_popen",
        ] {
            assert!(
                !stubs
                    .split([' ', ',', '[', ']', '\n'])
                    .any(|word| word == call),
                "the patcher links {call}"
            );
        }
    }
}
