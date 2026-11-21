use crate::error::{IpcError, Result};
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRole {
    Content,
    Network,
    Gpu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    pub role: SandboxRole,
    pub address_space_limit_bytes: Option<u64>,
    pub allow_network: bool,
    pub read_only_paths: Vec<PathBuf>,
}

impl SandboxConfig {
    pub fn for_content() -> Self {
        Self {
            role: SandboxRole::Content,
            address_space_limit_bytes: Some(2 * 1024 * 1024 * 1024),
            allow_network: false,
            read_only_paths: Vec::new(),
        }
    }

    pub fn for_network() -> Self {
        Self {
            role: SandboxRole::Network,
            address_space_limit_bytes: Some(1024 * 1024 * 1024),
            allow_network: true,
            read_only_paths: Vec::new(),
        }
    }

    pub fn for_gpu() -> Self {
        Self {
            role: SandboxRole::Gpu,
            address_space_limit_bytes: None,
            allow_network: false,
            read_only_paths: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.role == SandboxRole::Content && self.allow_network {
            return Err(IpcError::Process(
                "Content sandbox configuration must not permit direct network access".to_string(),
            ));
        }
        if self.role == SandboxRole::Gpu && self.allow_network {
            return Err(IpcError::Process(
                "GPU sandbox configuration must not permit direct network access".to_string(),
            ));
        }
        for path in &self.read_only_paths {
            if !path.is_absolute() {
                return Err(IpcError::InvalidEndpoint(format!(
                    "Sandbox read-only path must be absolute: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SandboxStatus {
    pub no_new_privs: bool,
    pub rlimit_applied: bool,
    pub landlock_abi_version: Option<u32>,
    pub landlock_applied: bool,
}

#[cfg(target_os = "linux")]
pub fn probe_landlock_abi() -> Option<u32> {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    // SAFETY: Invoking `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` is a
    // read-only kernel ABI capability query with no side effects and no dereferenced pointers.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi >= 0 {
        u32::try_from(abi).ok()
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_landlock_abi() -> Option<u32> {
    None
}

fn apply_address_space_limit(limit: u64) -> Result<bool> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `current` is a valid writable `libc::rlimit` struct for `getrlimit(RLIMIT_AS)`.
    if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut current) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let desired = limit as libc::rlim_t;
    // Never attempt to raise a hard ceiling (which requires privileges and returns EPERM).
    if current.rlim_max != libc::RLIM_INFINITY && desired > current.rlim_max {
        return Ok(false);
    }
    if current.rlim_cur != libc::RLIM_INFINITY && current.rlim_cur <= desired {
        return Ok(true);
    }
    let next = libc::rlimit {
        rlim_cur: desired,
        rlim_max: current.rlim_max,
    };
    // SAFETY: `next` only lowers or preserves the caller's soft virtual-address limit within `rlim_max`.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &next) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(true)
}

pub fn apply_sandbox(config: &SandboxConfig) -> Result<SandboxStatus> {
    config.validate()?;

    let mut status = SandboxStatus {
        landlock_abi_version: probe_landlock_abi(),
        ..SandboxStatus::default()
    };

    #[cfg(target_os = "linux")]
    {
        // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` takes no pointers and irreversibly
        // prevents privilege escalation across `execve` for the calling process.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        status.no_new_privs = true;
    }

    if let Some(limit) = config.address_space_limit_bytes {
        status.rlimit_applied = apply_address_space_limit(limit)?;
    }

    // Full path-allowlisted Landlock + seccomp-bpf enforcement lands in the Phase 4 hardening pass;
    // keep `landlock_applied = false` here so callers always receive accurate status.
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_role_configs_validate() {
        let content = SandboxConfig::for_content();
        assert!(!content.allow_network);
        assert!(content.validate().is_ok());

        let network = SandboxConfig::for_network();
        assert!(network.allow_network);
        assert!(network.validate().is_ok());

        let gpu = SandboxConfig::for_gpu();
        assert!(!gpu.allow_network);
        assert!(gpu.validate().is_ok());
    }

    #[test]
    fn content_cannot_request_direct_network_and_paths_must_be_absolute() {
        let mut bad_net = SandboxConfig::for_content();
        bad_net.allow_network = true;
        assert!(matches!(bad_net.validate(), Err(IpcError::Process(_))));

        let mut bad_path = SandboxConfig::for_content();
        bad_path
            .read_only_paths
            .push(PathBuf::from("relative/font"));
        assert!(matches!(
            bad_path.validate(),
            Err(IpcError::InvalidEndpoint(_))
        ));
    }

    #[test]
    fn landlock_probe_does_not_panic() {
        let _ = probe_landlock_abi();
    }
}
