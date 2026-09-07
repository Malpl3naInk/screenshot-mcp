// Windows Graphics Capture (WGC) — window capture
//
// Uses the `windows` crate directly (instead of `kscreenshot`).
//
// Why a custom implementation?
//   `kscreenshot` initializes COM as MTA (COINIT_MULTITHREADED) and grabs a
//   single frame via `TryGetNextFrame`. On several GPUs / window types that
//   yields an all-black frame (or an E_INVALIDARG from CreateForWindow for
//   UWP/XAML content windows) because the WGC session requires a STA thread
//   and a little time to produce a composited frame.
//
// This implementation:
//   - Initializes COM as STA (COINIT_APARTMENTTHREADED)
//   - Creates the capture item with `IGraphicsCaptureItemInterop`
//   - Uses a free-threaded frame pool and polls `TryGetNextFrame`, discarding
//     black frames, so we return the first *real* frame
//
// Key properties (preserved from the previous implementation):
//   - Captures a specific HWND's GPU-rendered content directly
//   - Does NOT modify window state (no z-order, activation, minimize)
//   - Works for DirectX/OpenGL/Vulkan applications
//   - Requires Windows 10 1903+ (build 18362)

use image::RgbaImage;
use windows::core::{factory, Interface};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
};
use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

const RPC_E_CHANGED_MODE: i32 = -2147417850; // 0x80010106

/// Keeps the COM apartment initialized as STA for the duration of a capture.
struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self, String> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if hr.is_err() && hr.0 != RPC_E_CHANGED_MODE {
                return Err(format!("CoInitializeEx (STA) failed: {hr}"));
            }
        }
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// Capture one monitor using Windows Graphics Capture.
///
/// Kept for compatibility with the full-screen path.
pub fn capture_monitor(monitor_index: usize) -> Result<RgbaImage, String> {
    tracing::info!("WGC: capturing monitor {}", monitor_index);

    let manager = kscreenshot::ScreenCaptureManager::new()
        .map_err(|e| format!("WGC: create manager failed: {e}"))?;
    let capture_info = manager
        .get_screen_capture_info_by_index(monitor_index)
        .map_err(|e| format!("WGC: monitor {monitor_index} not found: {e}"))?;
    let result = manager
        .capture_screen_mat(capture_info)
        .map_err(|e| format!("WGC: monitor capture failed: {e}"))?;

    RgbaImage::from_raw(
        result.source.width,
        result.source.height,
        result.source.to_rgba(),
    )
    .ok_or_else(|| "WGC: RgbaImage creation failed".into())
}

/// Capture a single window using Windows Graphics Capture (STA-based).
pub fn capture_window(hwnd: usize) -> Result<RgbaImage, String> {
    tracing::info!("WGC: capturing hwnd={:#x} (custom STA implementation)", hwnd);

    let _com = ComGuard::new()?;
    let (device, context) = crate::dxgi::create_device()?;

    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|e| format!("WGC: cast to IDXGIDevice failed: {e}"))?;
    let d3d_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
        .map_err(|e| format!("WGC: CreateDirect3D11DeviceFromDXGIDevice failed: {e}"))?
        .cast()
        .map_err(|e| format!("WGC: cast d3d device failed: {e}"))?;

    let item = create_item_for_window(hwnd)?;
    let size = item
        .Size()
        .map_err(|e| format!("WGC: item.Size() failed: {e}"))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err("WGC: capture item has zero size".into());
    }

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &d3d_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|e| format!("WGC: CreateFreeThreaded failed: {e}"))?;

    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|e| format!("WGC: CreateCaptureSession failed: {e}"))?;
    // Ignore errors from these niceties: some builds reject the setters.
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetIsBorderRequired(false);
    session
        .StartCapture()
        .map_err(|e| format!("WGC: StartCapture failed: {e}"))?;

    // Poll for a *real* (non-black) frame. WGC needs a moment to compose a
    // frame; the very first one is frequently all-black.
    const MAX_ATTEMPTS: u32 = 100;
    const POLL_INTERVAL_MS: u64 = 16;
    let mut last_image: Option<RgbaImage> = None;

    for _ in 0..MAX_ATTEMPTS {
        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        if let Ok(frame) = pool.TryGetNextFrame() {
            if let Ok(image) = read_frame_to_image(&device, &context, &frame) {
                if !is_image_all_black(&image) {
                    let _ = frame.Close();
                    let _ = session.Close();
                    let _ = pool.Close();
                    tracing::info!(
                        "WGC: captured window {:#x} ({})x({})",
                        hwnd,
                        image.width(),
                        image.height()
                    );
                    return Ok(image);
                }
                last_image = Some(image);
            }
        }
    }

    let _ = session.Close();
    let _ = pool.Close();

    if let Some(image) = last_image {
        tracing::warn!("WGC: only black frames received for hwnd={:#x}; returning last", hwnd);
        Ok(image)
    } else {
        Err("WGC: no frame received after timeout".into())
    }
}

/// Create a `GraphicsCaptureItem` for a window handle via the interop factory.
fn create_item_for_window(hwnd: usize) -> Result<GraphicsCaptureItem, String> {
    // Primary: interop CreateForWindow (classic top-level Win32 windows).
    if let Ok(interop) = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>() {
        if let Ok(item) = unsafe { interop.CreateForWindow(HWND(hwnd as *mut core::ffi::c_void)) } {
            return Ok(item);
        }
    }

    // Fallback: TryCreateFromWindowId. This route supports some UWP /
    // compositor-hosted windows where CreateForWindow returns E_INVALIDARG.
    let window_id = windows::UI::WindowId {
        Value: hwnd as u64,
    };
    GraphicsCaptureItem::TryCreateFromWindowId(window_id)
        .map_err(|e| format!("WGC: CreateForWindow & TryCreateFromWindowId both failed: {e}"))
}

/// Copy the frame surface into a staging texture and convert it to RGBA.
fn read_frame_to_image(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &Direct3D11CaptureFrame,
) -> Result<RgbaImage, String> {
    let surface: IDirect3DSurface = frame
        .Surface()
        .map_err(|e| format!("WGC: frame.Surface() failed: {e}"))?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .map_err(|e| format!("WGC: surface cast failed: {e}"))?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
        .map_err(|e| format!("WGC: GetInterface failed: {e}"))?;

    let mut desc = unsafe { std::mem::zeroed::<D3D11_TEXTURE2D_DESC>() };
    unsafe { texture.GetDesc(&mut desc) };

    let content_size = frame
        .ContentSize()
        .map_err(|e| format!("WGC: frame.ContentSize() failed: {e}"))?;

    let width = content_size.Width.max(1) as u32;
    let height = content_size.Height.max(1) as u32;
    if width == 0 || height == 0 {
        return Err("WGC: frame content has zero size".into());
    }

    // Clamp content size to the actual texture dimensions.
    let width = width.min(desc.Width.max(1));
    let height = height.min(desc.Height.max(1));

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };

    let mut staging_texture: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging_texture))
            .map_err(|e| format!("WGC: CreateTexture2D staging failed: {e}"))?;
    }
    let staging_texture = staging_texture.ok_or("WGC: staging texture is null")?;

    // Copy only the content region.
    let region = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: width,
        bottom: height,
        back: 1,
    };
    unsafe {
        context.CopySubresourceRegion(
            &staging_texture,
            0,
            0,
            0,
            0,
            &texture,
            0,
            Some(&region),
        );
    }

    let mut mapped = unsafe { std::mem::zeroed::<D3D11_MAPPED_SUBRESOURCE>() };
    unsafe {
        context
            .Map(&staging_texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("WGC: Map staging texture failed: {e}"))?;
    }

    let row_pitch = mapped.RowPitch as usize;
    let pixels = unsafe {
        std::slice::from_raw_parts(mapped.pData as *const u8, row_pitch * height as usize)
    };

    let image = match desc.Format {
        DXGI_FORMAT_B8G8R8A8_UNORM => bgra_to_rgba(pixels, row_pitch, width, height),
        DXGI_FORMAT_R8G8B8A8_UNORM => rgba_to_rgba(pixels, row_pitch, width, height),
        _ => {
            unsafe { context.Unmap(&staging_texture, 0) };
            return Err(format!("WGC: unsupported frame format {:?}", desc.Format));
        }
    };

    unsafe { context.Unmap(&staging_texture, 0) };
    image
}

fn bgra_to_rgba(pixels: &[u8], row_pitch: usize, width: u32, height: u32) -> Result<RgbaImage, String> {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for y in 0..height as usize {
        let src = y * row_pitch;
        let dst = y * width as usize * 4;
        for x in 0..width as usize {
            let si = src + x * 4;
            let di = dst + x * 4;
            data[di] = pixels[si + 2]; // R
            data[di + 1] = pixels[si + 1]; // G
            data[di + 2] = pixels[si]; // B
            data[di + 3] = 255;
        }
    }
    RgbaImage::from_raw(width, height, data).ok_or_else(|| "WGC: RgbaImage creation failed".into())
}

fn rgba_to_rgba(pixels: &[u8], row_pitch: usize, width: u32, height: u32) -> Result<RgbaImage, String> {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for y in 0..height as usize {
        let src = y * row_pitch;
        let dst = y * width as usize * 4;
        for x in 0..width as usize {
            let si = src + x * 4;
            let di = dst + x * 4;
            data[di] = pixels[si]; // R
            data[di + 1] = pixels[si + 1]; // G
            data[di + 2] = pixels[si + 2]; // B
            data[di + 3] = 255;
        }
    }
    RgbaImage::from_raw(width, height, data).ok_or_else(|| "WGC: RgbaImage creation failed".into())
}

/// Cheap "is this essentially a black frame?" check used to skip bad frames.
fn is_image_all_black(img: &RgbaImage) -> bool {
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return true;
    }
    let step_x = (w as usize / 16).max(1) as u32;
    let step_y = (h as usize / 16).max(1) as u32;
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for y in (0..h).step_by(step_y as usize) {
        for x in (0..w).step_by(step_x as usize) {
            let p = img.get_pixel(x, y);
            sum += p[0] as u64 + p[1] as u64 + p[2] as u64;
            count += 1;
        }
    }
    if count == 0 {
        return true;
    }
    (sum / count) < 5
}
