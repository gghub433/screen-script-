//! Windows.Graphics.Capture session producing GPU textures.

use super::hr_err;
use crate::sources::parse_id;
use std::sync::{Arc, Condvar, Mutex};
use windows::core::{factory, IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

pub struct D3d {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub winrt: IDirect3DDevice,
}

pub fn create_device() -> Result<D3d, String> {
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| hr_err("D3D11CreateDevice", e))?;
        let device = device.ok_or("no D3D11 device")?;
        let context = context.ok_or("no D3D11 context")?;
        // Media Foundation and the capture thread both touch the device.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let dxgi: IDXGIDevice = device.cast().map_err(|e| hr_err("IDXGIDevice", e))?;
        let winrt: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
            .map_err(|e| hr_err("CreateDirect3D11DeviceFromDXGIDevice", e))?
            .cast()
            .map_err(|e| hr_err("IDirect3DDevice", e))?;
        Ok(D3d { device, context, winrt })
    }
}

pub fn create_item(source_id: &str) -> Result<GraphicsCaptureItem, String> {
    let (kind, handle) = parse_id(source_id).ok_or("bad source id")?;
    unsafe {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|e| hr_err("capture interop", e))?;
        let item: GraphicsCaptureItem = match kind {
            'm' => interop.CreateForMonitor(HMONITOR(handle as *mut _)),
            _ => interop.CreateForWindow(HWND(handle as *mut _)),
        }
        .map_err(|e| hr_err("CreateCaptureItem (is Windows 10 1903+ with a capturable source?)", e))?;
        Ok(item)
    }
}

/// A captured frame together with the texture holding it. Dropping it returns
/// the buffer to the pool, so consume it promptly.
pub struct CapturedFrame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// Session-clock time (µs) when the frame arrived from the compositor.
    pub captured_us: u64,
    _frame: Direct3D11CaptureFrame,
}

/// Single-slot mailbox: a newer frame replaces an unconsumed older one, which is
/// exactly the "never queue stale frames" behaviour a low-latency stream needs.
#[derive(Default)]
pub struct Mailbox {
    slot: Mutex<Option<CapturedFrame>>,
    cv: Condvar,
    pub replaced: std::sync::atomic::AtomicU64,
}

impl Mailbox {
    fn put(&self, f: CapturedFrame) {
        let mut g = self.slot.lock().unwrap();
        if g.replace(f).is_some() {
            self.replaced.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.cv.notify_one();
    }
    pub fn take(&self, timeout: std::time::Duration) -> Option<CapturedFrame> {
        let mut g = self.slot.lock().unwrap();
        if g.is_none() {
            g = self.cv.wait_timeout(g, timeout).unwrap().0;
        }
        g.take()
    }
}

// SAFETY: the WinRT frame and D3D texture are free-threaded agile objects.
unsafe impl Send for CapturedFrame {}

pub struct Capture {
    pub mailbox: Arc<Mailbox>,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
    winrt: IDirect3DDevice,
    pub size: SizeInt32,
}

impl Capture {
    pub fn start(d3d: &D3d, item: GraphicsCaptureItem, clock: Arc<dyn revizor_session::Clock>) -> Result<Self, String> {
        let size = item.Size().map_err(|e| hr_err("item size", e))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d.winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 3, size)
            .map_err(|e| hr_err("frame pool", e))?;
        let mailbox = Arc::new(Mailbox::default());
        let mb = mailbox.clone();
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |p, _| {
            if let Some(p) = p.as_ref() {
                if let Ok(frame) = p.TryGetNextFrame() {
                    let arrived = clock.now_us();
                    if let Ok(surface) = frame.Surface() {
                        if let Ok(access) = surface.cast::<IDirect3DDxgiInterfaceAccess>() {
                            if let Ok(tex) = unsafe { access.GetInterface::<ID3D11Texture2D>() } {
                                let cs = frame.ContentSize().unwrap_or_default();
                                mb.put(CapturedFrame { texture: tex, width: cs.Width.max(0) as u32, height: cs.Height.max(0) as u32, captured_us: arrived, _frame: frame });
                            }
                        }
                    }
                }
            }
            Ok(())
        }))
        .map_err(|e| hr_err("FrameArrived", e))?;
        let session = pool.CreateCaptureSession(&item).map_err(|e| hr_err("capture session", e))?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        // Windows 11 only: hide the yellow capture border. Ignored where unsupported.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().map_err(|e| hr_err("StartCapture", e))?;
        Ok(Self { mailbox, pool, session, item, winrt: d3d.winrt.clone(), size })
    }

    /// If the source changed size (window resized, display mode change) the pool must be recreated.
    pub fn resize_if_needed(&mut self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 || (self.size.Width as u32 == w && self.size.Height as u32 == h) {
            return false;
        }
        let new = SizeInt32 { Width: w as i32, Height: h as i32 };
        if self.pool.Recreate(&self.winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 3, new).is_ok() {
            self.size = new;
            return true;
        }
        false
    }

    pub fn source_size(&self) -> (u32, u32) {
        self.item.Size().map(|s| (s.Width.max(0) as u32, s.Height.max(0) as u32)).unwrap_or((0, 0))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
