use crate::DesktopData;
use crate::DesktopDataKey;
use anyhow::{Result, anyhow, bail};
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{Data, DataCondition, RealmDatabase, ResidentVec};
use tracing::trace;

/// Whether listing the displays failed because there is no display server to
/// list them from. The capture backends report a missing X11 or Wayland server
/// as "connection refused", which is the normal state of a headless host
/// rather than a fault.
fn no_display_server(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ConnectionRefused
}

/// Locate a display by name, falling back to the primary (then first) display.
pub(crate) fn find_display(desktop_uuid: &str) -> Result<crate::capture::Display> {
    let mut displays = crate::capture::Display::all().map_err(|e| {
        if no_display_server(&e) {
            anyhow!("No display server (X11 or Wayland) is running to capture from")
        } else {
            anyhow::Error::new(e).context("Failed to list the displays to capture")
        }
    })?;
    if let Some(idx) = displays.iter().position(|d| d.name() == desktop_uuid) {
        return Ok(displays.swap_remove(idx));
    }
    if displays.is_empty() {
        bail!("No displays available");
    }
    if let Some(idx) = displays.iter().position(|d| d.is_primary()) {
        return Ok(displays.swap_remove(idx));
    }
    Ok(displays.swap_remove(0))
}

/// Invoke `f` with the `(r, g, b)` bytes of each pixel in a captured frame,
/// handling row stride and the source pixel format (BGRA vs RGBA).
pub(crate) fn for_each_rgb(
    data: &[u8],
    width: usize,
    height: usize,
    stride: &[usize],
    pixfmt: crate::capture::Pixfmt,
    mut f: impl FnMut(u8, u8, u8),
) {
    let row_stride = stride.first().copied().unwrap_or(width * 4);
    // Byte offsets of the red and blue channels within a 4-byte pixel.
    let (r_off, b_off) = match pixfmt {
        crate::capture::Pixfmt::RGBA => (0usize, 2usize),
        // Default to BGRA layout for everything else.
        _ => (2usize, 0usize),
    };

    for y in 0..height {
        let row = &data[y * row_stride..];
        for x in 0..width {
            let i = x * 4;
            if i + 3 >= row.len() {
                break;
            }
            f(row[i + r_off], row[i + 1], row[i + b_off]);
        }
    }
}

/// Enumerates capturable desktops (displays) into the database.
pub struct DesktopDisplayCollector {
    data: ResidentVec<DesktopData>,
    instance_id: InstanceId,
}

impl DesktopDisplayCollector {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            data: db.resident_vec(DataCondition::equal(
                DesktopDataKey::_instance_id,
                instance_id,
            ))?,
            instance_id,
        })
    }

    /// Make the rows match `displays` exactly: update the ones still attached,
    /// add new ones, and drop the ones that have gone away.
    fn reconcile(&mut self, displays: Vec<DesktopData>) -> Result<()> {
        'next_display: for display in &displays {
            // Update an existing row if we already know this display.
            for resident in self.data.iter() {
                if resident.read().name == display.name {
                    resident.update(|d| {
                        d.width = display.width;
                        d.height = display.height;
                        d.primary = display.primary;
                        d.scale_factor = display.scale_factor;
                        Ok(())
                    })?;
                    continue 'next_display;
                }
            }

            self.data.push(display.clone())?;
        }

        // A display that was unplugged, or a display server that went away,
        // can no longer be captured, so it shouldn't be offered to clients.
        let stale: Vec<_> = self
            .data
            .iter()
            .filter(|r| !displays.iter().any(|d| d.name == r.read().name))
            .map(|r| r.read().id())
            .collect();
        for id in stale {
            self.data.remove(id)?;
        }

        Ok(())
    }
}

impl Collector for DesktopDisplayCollector {
    async fn refresh(&mut self) -> Result<()> {
        let displays = match crate::capture::Display::all() {
            Ok(displays) => displays,
            // A headless host simply has no displays to offer.
            Err(e) if no_display_server(&e) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        trace!(count = displays.len(), "Enumerated desktops");

        self.reconcile(
            displays
                .iter()
                .map(|display| DesktopData {
                    name: display.name(),
                    width: display.width() as u32,
                    height: display.height() as u32,
                    primary: display.is_primary(),
                    scale_factor: display.scale(),
                    ..DesktopData::scoped(self.instance_id)
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandpolis_instance::database::DatabaseManager;
    use sandpolis_instance::realm::RealmName;
    use sandpolis_instance::test_db;

    fn display(instance: InstanceId, name: &str, width: u32) -> DesktopData {
        DesktopData {
            name: name.to_string(),
            width,
            height: 1080,
            primary: name == "DP-1",
            scale_factor: 1.0,
            ..DesktopData::scoped(instance)
        }
    }

    /// The collector's display names, once its rows have settled: a removal
    /// only leaves the vec when the database watcher delivers it.
    async fn names(collector: &DesktopDisplayCollector, expected: &[&str]) -> Vec<String> {
        let read = || {
            let mut names: Vec<_> = collector.data.iter().map(|r| r.read().name.clone()).collect();
            names.sort();
            names
        };
        for _ in 0..50 {
            if read() == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        read()
    }

    /// Displays come and go: a resized display is updated in place, an
    /// unplugged one is dropped, and losing the display server entirely
    /// leaves nothing to capture.
    #[tokio::test]
    async fn rows_follow_the_attached_displays() -> Result<()> {
        let database: DatabaseManager = test_db!(DesktopData);
        let instance: InstanceId = sandpolis_instance::AgentId::random().into();
        let mut collector =
            DesktopDisplayCollector::new(database.realm(RealmName::default())?, instance)?;

        collector.reconcile(vec![
            display(instance, "DP-1", 1920),
            display(instance, "HDMI-1", 1280),
        ])?;
        assert_eq!(names(&collector, &["DP-1", "HDMI-1"]).await, ["DP-1", "HDMI-1"]);

        collector.reconcile(vec![display(instance, "DP-1", 2560)])?;
        assert_eq!(names(&collector, &["DP-1"]).await, ["DP-1"]);
        assert_eq!(collector.data.iter().next().unwrap().read().width, 2560);

        collector.reconcile(Vec::new())?;
        assert!(names(&collector, &[]).await.is_empty());
        Ok(())
    }

    /// Another instance's displays (replicated into the same database) are
    /// neither claimed nor removed by this collector.
    #[tokio::test]
    async fn other_instances_displays_are_left_alone() -> Result<()> {
        let database: DatabaseManager = test_db!(DesktopData);
        let realm = database.realm(RealmName::default())?;
        let instance: InstanceId = sandpolis_instance::AgentId::random().into();
        let other: InstanceId = sandpolis_instance::AgentId::random().into();

        let mut theirs = DesktopDisplayCollector::new(realm.clone(), other)?;
        theirs.reconcile(vec![display(other, "DP-1", 1920)])?;

        let mut ours = DesktopDisplayCollector::new(realm, instance)?;
        assert!(names(&ours, &[]).await.is_empty());

        ours.reconcile(vec![display(instance, "HDMI-1", 1280)])?;
        assert_eq!(names(&ours, &["HDMI-1"]).await, ["HDMI-1"]);

        ours.reconcile(Vec::new())?;
        assert!(names(&ours, &[]).await.is_empty());
        assert_eq!(names(&theirs, &["DP-1"]).await, ["DP-1"]);
        Ok(())
    }

    #[test]
    fn a_refused_connection_means_headless() {
        use std::io::{Error, ErrorKind};
        assert!(no_display_server(&Error::from(ErrorKind::ConnectionRefused)));
        assert!(!no_display_server(&Error::from(ErrorKind::PermissionDenied)));
    }
}
