//! Media Foundation H.264 encoder wrapper.
//!
//! Hardware path (preferred): an asynchronous hardware MFT (NVENC / AMD AMF /
//! Intel Quick Sync, whichever the driver exposes) fed directly with D3D11
//! NV12 textures through a DXGI device manager — no CPU readback.
//! Fallback: Microsoft's software MFT with a one-copy staging readback. The
//! fallback is reported to the UI/diagnostics; it is never silent.

use super::capture::D3d;
use super::hr_err;
use std::collections::HashMap;
use std::time::Instant;
use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::Foundation::{BOOL, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

pub struct EncodedOut {
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// Time between handing the frame to the encoder and getting the bitstream back.
    pub encode_us: u32,
}

pub struct Encoder {
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    _mgr: Option<IMFDXGIDeviceManager>,
    pub name: String,
    pub hardware: bool,
    is_async: bool,
    need_input: u32,
    provides_samples: bool,
    out_size: u32,
    seq_header: Vec<u8>,
    fps: u32,
    w: u32,
    h: u32,
    inflight: HashMap<i64, Instant>,
    pending: Vec<EncodedOut>,
    staging: Option<ID3D11Texture2D>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

fn variant_u32(x: u32) -> VARIANT {
    VARIANT::from(x)
}

fn set_codec(codec: &Option<ICodecAPI>, key: &GUID, value: u32) {
    if let Some(c) = codec {
        let v = variant_u32(value);
        // Not every encoder implements every property; unsupported ones are fine to skip.
        let _ = unsafe { c.SetValue(key, &v) };
    }
}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

impl Encoder {
    pub fn new(d3d: &D3d, w: u32, h: u32, fps: u32, bitrate: u32, gop_secs: u32) -> Result<Self, String> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| hr_err("MFStartup", e))?;
            let in_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let out_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };

            let (activate, hardware) = match enum_encoder(&in_info, &out_info, true)? {
                Some(a) => (a, true),
                None => {
                    log::warn!("no hardware H.264 encoder found, falling back to the software MFT");
                    (enum_encoder(&in_info, &out_info, false)?.ok_or("no H.264 encoder available on this system")?, false)
                }
            };
            let name = {
                let mut buf = [0u16; 128];
                let mut len = 0u32;
                if activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)).is_ok() {
                    String::from_utf16_lossy(&buf[..len as usize])
                } else {
                    "H.264 encoder".to_string()
                }
            };
            let mft: IMFTransform = activate.ActivateObject().map_err(|e| hr_err("ActivateObject", e))?;

            let attrs = mft.GetAttributes().ok();
            let is_async = attrs.as_ref().is_some_and(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1);
            if is_async {
                if let Some(a) = &attrs {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| hr_err("async unlock", e))?;
                }
            }

            let mut mgr = None;
            if hardware {
                let mut token = 0u32;
                let mut m = None;
                MFCreateDXGIDeviceManager(&mut token, &mut m).map_err(|e| hr_err("MFCreateDXGIDeviceManager", e))?;
                let m = m.ok_or("no device manager")?;
                m.ResetDevice(&d3d.device, token).map_err(|e| hr_err("ResetDevice", e))?;
                if let Some(a) = &attrs {
                    let _ = a.SetUINT32(&MF_SA_D3D11_AWARE, 1);
                }
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize).map_err(|e| hr_err("SET_D3D_MANAGER", e))?;
                mgr = Some(m);
            }

            let codec: Option<ICodecAPI> = mft.cast().ok();
            set_codec(&codec, &CODECAPI_AVLowLatencyMode, 1);
            set_codec(&codec, &CODECAPI_AVEncCommonRealTime, 1);
            set_codec(&codec, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
            set_codec(&codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate);
            set_codec(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
            set_codec(&codec, &CODECAPI_AVEncMPVGOPSize, fps * gop_secs.max(1));

            // Output (compressed) type must be set before the input type for encoders.
            let out = MFCreateMediaType().map_err(|e| hr_err("MFCreateMediaType", e))?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| hr_err("major", e))?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| hr_err("subtype", e))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).map_err(|e| hr_err("bitrate", e))?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(|e| hr_err("frame size", e))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| hr_err("frame rate", e))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| hr_err("par", e))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| hr_err("interlace", e))?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).map_err(|e| hr_err("profile", e))?;
            mft.SetOutputType(0, &out, 0).map_err(|e| hr_err("SetOutputType", e))?;

            let inp = MFCreateMediaType().map_err(|e| hr_err("MFCreateMediaType", e))?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| hr_err("major", e))?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| hr_err("subtype", e))?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(|e| hr_err("frame size", e))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| hr_err("frame rate", e))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| hr_err("par", e))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| hr_err("interlace", e))?;
            // The video processor writes BT.709 limited range.
            let _ = inp.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
            let _ = inp.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            let _ = inp.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            mft.SetInputType(0, &inp, 0).map_err(|e| hr_err("SetInputType(NV12)", e))?;

            let info = mft.GetOutputStreamInfo(0).map_err(|e| hr_err("GetOutputStreamInfo", e))?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

            let seq_header = mft
                .GetOutputCurrentType(0)
                .ok()
                .and_then(|t| {
                    let mut size = 0u32;
                    t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok().and_then(|n| {
                        let mut buf = vec![0u8; n as usize];
                        t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, Some(&mut size)).ok().map(|_| buf)
                    })
                })
                .unwrap_or_default();

            mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).ok();
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| hr_err("BEGIN_STREAMING", e))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| hr_err("START_OF_STREAM", e))?;

            let events = if is_async { mft.cast::<IMFMediaEventGenerator>().ok() } else { None };
            Ok(Self {
                mft,
                events,
                codec,
                _mgr: mgr,
                name,
                hardware,
                is_async,
                need_input: 0,
                provides_samples,
                out_size: info.cbSize.max(w * h),
                seq_header,
                fps,
                w,
                h,
                inflight: HashMap::new(),
                pending: Vec::new(),
                staging: None,
                device: d3d.device.clone(),
                context: d3d.context.clone(),
            })
        }
    }

    pub fn set_bitrate(&self, bps: u32) {
        set_codec(&self.codec, &CODECAPI_AVEncCommonMeanBitRate, bps);
    }

    pub fn force_keyframe(&self) {
        set_codec(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame, 1);
    }

    /// True if the encoder can take a frame right now. An async hardware MFT tells
    /// us explicitly (NeedInput); when it does not, the new frame is dropped by the
    /// caller instead of queueing (queues are latency).
    pub fn can_accept(&mut self) -> bool {
        if self.is_async {
            let _ = self.pump_events();
            self.need_input > 0
        } else {
            true
        }
    }

    pub fn submit(&mut self, tex: &ID3D11Texture2D, pts_us: u64) -> Result<(), String> {
        unsafe {
            let sample = MFCreateSample().map_err(|e| hr_err("MFCreateSample", e))?;
            let buffer: IMFMediaBuffer = if self.hardware {
                let b = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, BOOL(0)).map_err(|e| hr_err("MFCreateDXGISurfaceBuffer", e))?;
                if let Ok(b2d) = b.cast::<IMF2DBuffer>() {
                    if let Ok(len) = b2d.GetContiguousLength() {
                        let _ = b.SetCurrentLength(len);
                    }
                }
                b
            } else {
                self.readback(tex)?
            };
            sample.AddBuffer(&buffer).map_err(|e| hr_err("AddBuffer", e))?;
            let t100 = (pts_us as i64) * 10;
            sample.SetSampleTime(t100).map_err(|e| hr_err("SetSampleTime", e))?;
            sample.SetSampleDuration(10_000_000 / self.fps.max(1) as i64).map_err(|e| hr_err("SetSampleDuration", e))?;
            self.inflight.insert(t100, Instant::now());
            // keep the map bounded if the encoder swallows a frame
            if self.inflight.len() > 64 {
                let min = *self.inflight.keys().min().unwrap();
                self.inflight.remove(&min);
            }
            self.mft.ProcessInput(0, &sample, 0).map_err(|e| hr_err("ProcessInput", e))?;
            if self.is_async {
                self.need_input = self.need_input.saturating_sub(1);
            } else {
                while let Some(o) = self.process_output()? {
                    self.pending.push(o);
                }
            }
            Ok(())
        }
    }

    /// Collect finished bitstreams.
    pub fn poll(&mut self, out: &mut Vec<EncodedOut>) -> Result<(), String> {
        if self.is_async {
            self.pump_events()?;
        }
        out.append(&mut self.pending);
        Ok(())
    }

    fn pump_events(&mut self) -> Result<(), String> {
        let Some(gen) = self.events.clone() else { return Ok(()) };
        loop {
            let ev = match unsafe { gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(hr_err("GetEvent", e)),
            };
            let kind = unsafe { ev.GetType() }.unwrap_or(0);
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                if let Some(o) = self.process_output()? {
                    self.pending.push(o);
                }
            }
        }
    }

    fn process_output(&mut self) -> Result<Option<EncodedOut>, String> {
        unsafe {
            let mut buf = MFT_OUTPUT_DATA_BUFFER::default();
            if !self.provides_samples {
                let s = MFCreateSample().map_err(|e| hr_err("MFCreateSample", e))?;
                let mb = MFCreateMemoryBuffer(self.out_size).map_err(|e| hr_err("MFCreateMemoryBuffer", e))?;
                s.AddBuffer(&mb).map_err(|e| hr_err("AddBuffer", e))?;
                buf.pSample = std::mem::ManuallyDrop::new(Some(s));
            }
            let mut arr = [buf];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut arr, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut arr[0].pSample);
            let _ = std::mem::ManuallyDrop::take(&mut arr[0].pEvents);
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if let Ok(t) = self.mft.GetOutputAvailableType(0, 0) {
                        let _ = self.mft.SetOutputType(0, &t, 0);
                    }
                    return Ok(None);
                }
                Err(e) => return Err(hr_err("ProcessOutput", e)),
            }
            let Some(sample) = sample else { return Ok(None) };
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
            let t100 = sample.GetSampleTime().unwrap_or(0);
            let contiguous = sample.ConvertToContiguousBuffer().map_err(|e| hr_err("ConvertToContiguousBuffer", e))?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            contiguous.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| hr_err("Lock", e))?;
            let mut data = Vec::with_capacity(len as usize + self.seq_header.len());
            if keyframe && !self.seq_header.is_empty() && !has_sps(std::slice::from_raw_parts(ptr, (len as usize).min(128))) {
                data.extend_from_slice(&self.seq_header);
            }
            data.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
            let _ = contiguous.Unlock();
            let encode_us = self.inflight.remove(&t100).map_or(0, |t| t.elapsed().as_micros() as u32);
            Ok(Some(EncodedOut { pts_us: (t100 / 10).max(0) as u64, keyframe, data, encode_us }))
        }
    }

    /// Software-MFT fallback: copy the NV12 texture into system memory.
    fn readback(&mut self, tex: &ID3D11Texture2D) -> Result<IMFMediaBuffer, String> {
        unsafe {
            if self.staging.is_none() {
                let td = D3D11_TEXTURE2D_DESC {
                    Width: self.w,
                    Height: self.h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.device.CreateTexture2D(&td, None, Some(&mut t)).map_err(|e| hr_err("staging texture", e))?;
                self.staging = t;
            }
            let st = self.staging.as_ref().unwrap();
            self.context.CopyResource(st, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m)).map_err(|e| hr_err("Map", e))?;
            let (w, h, pitch) = (self.w as usize, self.h as usize, m.RowPitch as usize);
            let total = w * h * 3 / 2;
            let mb = MFCreateMemoryBuffer(total as u32).map_err(|e| hr_err("MFCreateMemoryBuffer", e))?;
            let mut dst = std::ptr::null_mut();
            mb.Lock(&mut dst, None, None).map_err(|e| hr_err("Lock", e))?;
            let src = m.pData as *const u8;
            for row in 0..h {
                std::ptr::copy_nonoverlapping(src.add(row * pitch), dst.add(row * w), w);
            }
            // UV plane follows the Y plane in the mapped texture at pitch*height
            for row in 0..h / 2 {
                std::ptr::copy_nonoverlapping(src.add((h + row) * pitch), dst.add(w * h + row * w), w);
            }
            let _ = mb.Unlock();
            self.context.Unmap(st, 0);
            mb.SetCurrentLength(total as u32).map_err(|e| hr_err("SetCurrentLength", e))?;
            Ok(mb)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            let _ = MFShutdown();
        }
    }
}

unsafe fn enum_encoder(i: &MFT_REGISTER_TYPE_INFO, o: &MFT_REGISTER_TYPE_INFO, hw: bool) -> Result<Option<IMFActivate>, String> {
    let flags = if hw { MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER } else { MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER };
    let mut acts: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(i), Some(o), &mut acts, &mut n).map_err(|e| hr_err("MFTEnumEx", e))?;
    let mut first = None;
    if !acts.is_null() {
        for k in 0..n as usize {
            let a = std::ptr::read(acts.add(k));
            if first.is_none() {
                first = a;
            }
        }
        CoTaskMemFree(Some(acts as *const _));
    }
    Ok(first)
}

/// Does the Annex-B buffer start with (or contain early) an SPS NAL (type 7)?
pub fn has_sps(b: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < b.len() {
        if b[i] == 0 && b[i + 1] == 0 && (b[i + 2] == 1 || (b[i + 2] == 0 && b[i + 3] == 1)) {
            let nal = if b[i + 2] == 1 { b[i + 3] } else { b.get(i + 4).copied().unwrap_or(0) };
            if nal & 0x1f == 7 {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[allow(dead_code)]
const _TRUE: BOOL = TRUE;

/// Cheap capability probe (no encoder instance): is a hardware H.264 MFT registered?
pub fn probe_hardware_h264() -> Option<String> {
    unsafe {
        if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
            return None;
        }
        let i = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let o = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let r = enum_encoder(&i, &o, true).ok().flatten().map(|a| {
            let mut buf = [0u16; 128];
            let mut len = 0u32;
            if a.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)).is_ok() {
                String::from_utf16_lossy(&buf[..len as usize])
            } else {
                "Hardware H.264 encoder".to_string()
            }
        });
        let _ = MFShutdown();
        r
    }
}
