pub mod manager;
pub mod speed_meter;
pub mod state;

pub use manager::{DownloadMonitorError, DownloadMonitorHandle, DownloadMonitorResult};
pub use speed_meter::SpeedMeter;
pub use state::{DownloadView, MonitorState, Projection};
