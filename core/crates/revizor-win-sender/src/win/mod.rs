//! Windows implementation: Windows.Graphics.Capture → D3D11 video processor
//! (BGRA→NV12 + scaling on the GPU) → Media Foundation hardware H.264 encoder
//! fed with GPU textures (no CPU readback on the hardware path).
//!
//! NOTE: this module has been type-checked for `x86_64-pc-windows-*` but has not
//! been exercised on real GPUs by the authors of this commit; see docs/TESTING.md.




mod capture;
mod convert;
mod encoder;
mod enumerate;
mod runner;


pub use enumerate::list_sources;
pub use encoder::probe_hardware_h264;
pub use runner::WinPipeline;


pub(crate) fn hr_err(ctx: &str, e: windows::core::Error) -> String {
    format!("{ctx}: {e}")
}
