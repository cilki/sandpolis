//! The systemd unit a deployment installs on its target.
//!
//! These run on the *target* over SSH, which is why this is gated on the server
//! feature rather than the agent one — an agent manages its own host's units
//! through `sandpolis-health`, not through here.

use crate::deploy::{DATA_PATH, INSTALL_PATH, REALM_FILE};

/// Where the unit file goes on the target.
pub const UNIT_PATH: &str = "/etc/systemd/system/sandpolis-agent.service";

/// The unit's name, as `systemctl` wants it.
pub const UNIT_NAME: &str = "sandpolis-agent.service";

/// Render the unit file.
pub fn unit_file() -> String {
    format!(
        "[Unit]\n\
         Description=Sandpolis Agent\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={INSTALL_PATH} agent --realm {REALM_FILE} --data {DATA_PATH}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

#[cfg(test)]
mod test {
    use super::*;

    /// The unit that shipped with the old deploy crate had a literal `{}` where
    /// `ExecStart` should have named the binary, so it could never have started
    /// anything.
    #[test]
    fn unit_names_the_installed_binary() {
        let unit = unit_file();
        // The subcommand is what makes this an agent; a bare invocation prints
        // help and exits.
        assert!(unit.contains(&format!("ExecStart={INSTALL_PATH} agent ")));
        assert!(unit.contains(REALM_FILE));
        assert!(!unit.contains("{}"));
    }
}
