use crate::DesktopData;
use anyhow::{Context, Result, bail};
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{RealmDatabase, ResidentVec};
use tracing::trace;

/// Locate a display by name, falling back to the primary (then first) display.
pub(crate) fn find_display(desktop_uuid: &str) -> Result<crate::capture::Display> {
    // On Linux, "connection refused" here means there is no X11 or Wayland
    // server to talk to, which is meaningless without saying what was refused.
    let mut displays =
        crate::capture::Display::all().context("Failed to list the displays to capture")?;
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
            data: db.resident_vec(())?,
            instance_id,
        })
    }
}

impl Collector for DesktopDisplayCollector {
    async fn refresh(&mut self) -> Result<()> {
        let displays = crate::capture::Display::all()?;
        trace!(count = displays.len(), "Enumerated desktops");

        'next_display: for display in displays.iter() {
            let name = display.name();
            // Update an existing row if we already know this display.
            for resident in self.data.iter() {
                if resident.read().name == name {
                    resident.update(|d| {
                        d.width = display.width() as u32;
                        d.height = display.height() as u32;
                        d.primary = display.is_primary();
                        d.scale_factor = display.scale();
                        Ok(())
                    })?;
                    continue 'next_display;
                }
            }

            self.data.push(DesktopData {
                name,
                width: display.width() as u32,
                height: display.height() as u32,
                primary: display.is_primary(),
                scale_factor: display.scale(),
                ..DesktopData::scoped(self.instance_id)
            })?;
        }

        Ok(())
    }
}
