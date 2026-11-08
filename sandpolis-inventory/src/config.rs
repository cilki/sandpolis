use serde::Deserialize;
use serde::Serialize;
use strum::Display;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct InventoryManagerConfig {
    /// The CVE matching service.
    pub cve: CveConfig,
}

/// Settings for the CVE matching service.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
pub struct CveConfig {
    /// Whether the service runs at all. It downloads a sizeable feed from a
    /// third party, so it's worth being able to switch off wholesale.
    pub enabled: bool,

    /// Seconds between matching passes. A pass only re-downloads feed files
    /// that have gone stale, so this can be frequent without generating much
    /// traffic.
    pub interval: u64,

    /// Seconds a downloaded feed file stays fresh before it's fetched again.
    pub refresh_after: u64,

    /// Findings below this severity are ignored entirely: no row, no alert.
    /// Raising it also removes previously stored findings that fall under the
    /// new threshold on the next pass.
    pub min_severity: CveSeverity,

    /// Base URL the feed files are downloaded from. Overridable mainly so
    /// tests and air-gapped deployments can point at a mirror.
    pub feed_url: String,
}

impl Default for CveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: 6 * 60 * 60,
            refresh_after: 7 * 24 * 60 * 60,
            min_severity: CveSeverity::Critical,
            feed_url: "https://github.com/fkie-cad/nvd-json-data-feeds/releases/latest/download"
                .to_string(),
        }
    }
}

/// CVSS base severity, ordered so a threshold can be a simple comparison.
#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Display,
)]
pub enum CveSeverity {
    #[default]
    Low,
    Medium,
    High,
    Critical,
}
