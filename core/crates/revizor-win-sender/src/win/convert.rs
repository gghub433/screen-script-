//! GPU colour conversion + scaling: BGRA capture texture → NV12 texture sized
//! for the encoder, using the D3D11 video processor (fixed-function hardware
//! on all GPUs that can encode). No CPU involvement.

use super::capture::D3d;
use super::hr_err;
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};

const RING: usize = 6;

pub struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    ring: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    pub out_w: u32,
    pub out_h: u32,
    in_w: u32,
    in_h: u32,
}

impl Converter {
    pub fn new(d3d: &D3d, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self, String> {
        unsafe {
            let vdev: ID3D11VideoDevice = d3d.device.cast().map_err(|e| hr_err("ID3D11VideoDevice (GPU without video support?)", e))?;
            let vctx: ID3D11VideoContext = d3d.context.cast().map_err(|e| hr_err("ID3D11VideoContext", e))?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc).map_err(|e| hr_err("CreateVideoProcessorEnumerator", e))?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0).map_err(|e| hr_err("CreateVideoProcessor", e))?;

            // BGRA full-range sRGB in → BT.709 limited-range NV12 out (what players/decoders assume for HD).
            let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &in_cs);
            vctx.VideoProcessorSetOutputColorSpace(&processor, &out_cs);
            let out_rect = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            vctx.VideoProcessorSetOutputTargetRect(&processor, TRUE, Some(&out_rect));
            vctx.VideoProcessorSetStreamDestRect(&processor, 0, TRUE, Some(&out_rect));

            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                let td = D3D11_TEXTURE2D_DESC {
                    Width: out_w,
                    Height: out_h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut tex = None;
                d3d.device.CreateTexture2D(&td, None, Some(&mut tex)).map_err(|e| hr_err("CreateTexture2D(NV12)", e))?;
                let tex = tex.ok_or("no texture")?;
                let vd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut view = None;
                vdev.CreateVideoProcessorOutputView(&tex, &enumerator, &vd, Some(&mut view)).map_err(|e| hr_err("CreateVideoProcessorOutputView", e))?;
                ring.push((tex, view.ok_or("no output view")?));
            }
            Ok(Self { vdev, vctx, enumerator, processor, ring, next: 0, out_w, out_h, in_w, in_h })
        }
    }

    pub fn input_size(&self) -> (u32, u32) {
        (self.in_w, self.in_h)
    }

    /// Converts `src` (BGRA) into the next NV12 ring texture and returns it.
    pub fn convert(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D, String> {
        unsafe {
            let id = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut in_view = None;
            self.vdev
                .CreateVideoProcessorInputView(src, &self.enumerator, &id, Some(&mut in_view))
                .map_err(|e| hr_err("CreateVideoProcessorInputView", e))?;
            let (tex, out_view) = &self.ring[self.next];
            self.next = (self.next + 1) % self.ring.len();
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: TRUE,
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: ManuallyDrop::new(in_view),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&self.processor, out_view, 0, &streams);
            // reclaim the view reference we handed to the stream
            for s in streams {
                ManuallyDrop::into_inner(s.pInputSurface);
            }
            r.map_err(|e| hr_err("VideoProcessorBlt", e))?;
            Ok(tex.clone())
        }
    }
}
