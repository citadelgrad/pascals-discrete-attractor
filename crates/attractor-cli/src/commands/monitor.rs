//! `pas monitor [--port 7777] [--open]`: run the loopback-only Monitor (spec File Change 12/13).

use attractor_monitor::MonitorOpts;

pub async fn cmd_monitor(port: u16, open: bool) -> anyhow::Result<()> {
    attractor_monitor::serve(MonitorOpts { port, open }).await
}
