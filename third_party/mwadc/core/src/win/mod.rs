//! Windows: LibMWCapture.dll plus the driver's hidden KS properties (docs/07).

pub mod audio;
pub mod calibrate;
pub mod device;
pub mod ks;
pub mod sdk;
pub mod stream;

pub use audio::{AudioBlock, AudioClock, AudioFormat, AudioSink, AudioStream, AudioSummary, LockInfo, LockReport, LockedSink};
pub use calibrate::{calibrate, CalibrateOptions};
pub use device::{list, Armed, Device, DeviceInfo, Selector};
pub use stream::{FrameRef, FrameSink, Geometry, Stream, StreamOptions, Summary};
