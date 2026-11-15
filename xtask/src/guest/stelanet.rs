//! `org.stela.StelaNet`, the launchd job that configures `stn0`, StelaNet's
//! interface (`--guest-network-stelanet`). `stn0` is a plain ifnet, not an
//! IONetworkInterface, so configd's preferences hold no network service for
//! it and nothing configures it unless asked. The kext installs the job's
//! plist beside the sandbox patcher's (`kext/sandbox_cache.cpp`); launchd
//! loads it from the next boot on.
//!
//! The job publishes a DHCP service for `stn0` in configd's dynamic store,
//! where PreferencesMonitor publishes the preferences' services
//! (`Setup:/Network/Service/<id>/{Interface,IPv4,IPv6}`). IPConfiguration
//! runs the services it finds there and keeps them across configuration
//! changes; an `ipconfig set` service, which the first version used, is
//! dropped at the next configuration change for an interface without a
//! service of its own (seen 60 s after the first lease), and the new one got
//! another address. The job publishes the service again if the store loses
//! it, falls back to `ipconfig set` if the service gets no address, and logs
//! IPConfiguration's state whenever `stn0` has no address.
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
/// The service's ID in the store: fixed, so a restarted job finds its own.
const SERVICE: &str = "5354454C-414E-4554-0000-73746E300000";
/// How long the script waits for `stn0` after launchd starts it, in seconds:
/// without it (the option off) it then exits for the rest of the boot.
const WAIT_SECONDS: u32 = 60;
/// How long the store's service may go without an address before the job
/// falls back to `ipconfig set`, in seconds.
const STORE_GRACE_SECONDS: u32 = 90;
/// How long an `ipconfig set` service may go without an address before it
/// is set again, in seconds.
const ADHOC_GRACE_SECONDS: u32 = 20;
/// How often the script looks at `stn0`, in seconds.
const POLL_SECONDS: u32 = 5;

const SCRIPT: &str = r#"ID=@SERVICE@
log() { echo "$(/bin/date '+%Y-%m-%d %H:%M:%S') $*"; }
publish() {
  /usr/sbin/scutil <<EOF
d.init
d.add UserDefinedName StelaNet
set Setup:/Network/Service/$ID
d.init
d.add DeviceName stn0
d.add Type Ethernet
d.add Hardware Ethernet
d.add UserDefinedName StelaNet
set Setup:/Network/Service/$ID/Interface
d.init
d.add ConfigMethod DHCP
set Setup:/Network/Service/$ID/IPv4
d.init
d.add ConfigMethod Automatic
set Setup:/Network/Service/$ID/IPv6
quit
EOF
}
withdraw() {
  /usr/sbin/scutil <<EOF
remove Setup:/Network/Service/$ID/IPv4
remove Setup:/Network/Service/$ID/IPv6
remove Setup:/Network/Service/$ID/Interface
remove Setup:/Network/Service/$ID
quit
EOF
}
published() {
  printf 'show Setup:/Network/Service/%s/IPv4\nquit\n' "$ID" | /usr/sbin/scutil | /usr/bin/grep -q 'ConfigMethod : DHCP'
}
explain() {
  echo "--- ipconfig getsummary stn0"
  /usr/sbin/ipconfig getsummary stn0 2>&1
  echo "--- store"
  printf 'show Setup:/Network/Service/%s/IPv4\nshow State:/Network/Service/%s/IPv4\nshow State:/Network/Interface/stn0/Link\nshow State:/Network/Global/IPv4\nquit\n' "$ID" "$ID" | /usr/sbin/scutil 2>&1
  echo "--- ifconfig stn0"
  /sbin/ifconfig stn0 2>&1
  echo "---"
}
waited=0
until /sbin/ifconfig stn0 >/dev/null 2>&1; do
  [ "$waited" -ge @WAIT@ ] && exit 0
  /bin/sleep 2
  waited=$((waited + 2))
done
log "stn0 is present"
/sbin/ifconfig stn0 up
mode=store
grace=@STORE_GRACE@
publish
log "published DHCP service $ID for stn0 in the dynamic store"
since=$(/bin/date +%s)
last=none
while /sbin/ifconfig stn0 >/dev/null 2>&1; do
  now=$(/bin/date +%s)
  if [ "$mode" = store ] && ! published; then
    log "the dynamic store lost service $ID: publishing it again"
    explain
    publish
  fi
  address=$(/usr/sbin/ipconfig getifaddr stn0 2>/dev/null)
  case "$address" in
  ""|169.254.*) address=none ;;
  esac
  if [ "$address" != "$last" ]; then
    if [ "$address" = none ]; then
      log "stn0 lost its address $last ($mode)"
      explain
    else
      log "stn0 $address router $(/usr/sbin/ipconfig getoption stn0 router 2>/dev/null) dns $(/usr/sbin/ipconfig getoption stn0 domain_name_server 2>/dev/null) ($mode)"
    fi
    last=$address
    since=$now
  fi
  if [ "$address" = none ] && [ $((now - since)) -ge "$grace" ]; then
    if [ "$mode" = store ]; then
      log "service $ID got no address in $grace s: falling back to ipconfig set stn0 DHCP"
      explain
      withdraw
      mode=adhoc
      grace=@ADHOC_GRACE@
    else
      log "stn0 has no address: ipconfig set stn0 DHCP again"
      explain
    fi
    if /usr/sbin/ipconfig set stn0 DHCP; then
      /usr/sbin/ipconfig set stn0 AUTOMATIC-V6 || log "ipconfig set stn0 AUTOMATIC-V6 failed"
    else
      log "ipconfig set stn0 DHCP failed: $?"
    fi
    since=$now
  fi
  /bin/sleep @POLL@
done
log "stn0 is gone"
if [ "$mode" = store ]; then
  withdraw
fi
"#;

/// The job's script, for `/bin/sh` as root from boot. It waits a minute
/// for `stn0` and exits when there is none. While `stn0` exists it keeps a
/// DHCP and IPv6-automatic service for it in configd's dynamic store, falls
/// back to `ipconfig set` when that service gets no address, and logs every
/// address `stn0` gets, with its router and DNS servers, and
/// IPConfiguration's state whenever `stn0` has none.
pub fn script() -> String {
    SCRIPT
        .replace("@SERVICE@", SERVICE)
        .replace("@WAIT@", &WAIT_SECONDS.to_string())
        .replace("@STORE_GRACE@", &STORE_GRACE_SECONDS.to_string())
        .replace("@ADHOC_GRACE@", &ADHOC_GRACE_SECONDS.to_string())
        .replace("@POLL@", &POLL_SECONDS.to_string())
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
        assert!(!script.contains('@'), "every placeholder is filled");
        // A minute's wait for stn0, then nothing without it.
        assert!(script.contains("[ \"$waited\" -ge 60 ] && exit 0"));
        // Outside the quoted log messages, every tool goes by its path:
        // launchd gives the job a minimal PATH.
        for line in script.lines() {
            let commands: String = line.split('"').step_by(2).collect();
            for word in commands.split([' ', '(', '$', ';', '|']) {
                assert!(
                    !["date", "sleep", "ifconfig", "ipconfig", "scutil", "grep"].contains(&word),
                    "{word} without its path: {line}"
                );
            }
        }
        assert!(script.contains("set Setup:/Network/Service/$ID/IPv4"));
        assert!(script.contains("d.add ConfigMethod DHCP"));
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
