//! frame_tap.rs — VizLab driver-tap (virtual-display sub-method B).
//!
//! Copies each composited frame the IddCx swap chain hands us straight into the
//! `Global\VizLabFrame` shared-memory triple-buffer — the SAME transport the lab's
//! present-hook worker writes (`src/shm_frame.h`). No DXGI re-capture, no grabber, no
//! on-screen window: DWM delivers the frame to the driver and the driver publishes it.
//! This is the method's ceiling (`docs/virtual-display.md` "Building the driver-tap").
//!
//! Layout — MUST stay byte-identical to `src/shm_frame.h`:
//!   `[VizLabShmHeader: 8 x u32][buffer0][buffer1][buffer2]`
//!   header  = magic('VLFB') / version(1) / width / height / stride / format(0=BGRA8) / gen / reserved
//!   buffers = 1920 x 1088 x 4 BGRA8, top-down, tight (stride = width*4)
//!   protocol = fill slot (gen+1)%3, memory-fence, bump gen; a reader copies slot gen%3 (never torn).
//!
//! The IDD runs in **Session 0**, so the mapping is `Global\`-namespaced with a NULL-DACL
//! so the user-session consumer (lab / worker / `vizlab_shmread` / Resonance) can open it.
//!
//! This is an FFI- and raw-framebuffer-heavy module; it batches unsafe ops per block
//! (each is a single Win32/D3D11 call or a tight pixel copy), so the workspace's
//! `multiple_unsafe_ops_per_block` deny is relaxed here.
#![allow(clippy::multiple_unsafe_ops_per_block)]

use core::ffi::c_void;
use core::sync::atomic::{fence, Ordering};

use log::{debug, error};
use windows::{
    core::{w, Interface},
    Win32::{
        Foundation::{CloseHandle, BOOL, HANDLE, INVALID_HANDLE_VALUE},
        Graphics::{
            Direct3D11::{
                ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC},
                IDXGIResource,
            },
        },
        Security::{
            InitializeSecurityDescriptor, SetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR,
            SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        },
        System::Memory::{
            CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
            MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
        },
    },
};

// --- shm_frame.h constants (keep in lock-step) ---
const SHM_MAGIC: u32 = 0x4246_4C56; // 'VLFB'
const SHM_MAXW: u32 = 1920;
const SHM_MAXH: u32 = 1088; // 1088 not 1080: a D3D9 backbuffer rounds height up to a multiple of 16
const SHM_HEADER: usize = 32; // 8 x u32
const SHM_BUF: usize = (SHM_MAXW as usize) * (SHM_MAXH as usize) * 4;
const SHM_TOTAL: usize = SHM_HEADER + 3 * SHM_BUF;

// header u32 indices
const H_WIDTH: usize = 2;
const H_HEIGHT: usize = 3;
const H_STRIDE: usize = 4;
const H_GEN: usize = 6;

/// Publishes IddCx-composited frames into `Global\VizLabFrame`. Created on the swap-chain
/// processing thread and used only there (the device's immediate context is single-threaded).
pub struct FrameTap {
    map: HANDLE,
    base: *mut u8,
    ctx: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    sw: u32,
    sh: u32,
    sfmt: DXGI_FORMAT,
    logged_first: bool,
}

impl FrameTap {
    /// Create the `Global\VizLabFrame` mapping (NULL-DACL so the user session can open it) and
    /// cache the device's immediate context. Returns `None` on any failure — the driver then
    /// runs as a plain virtual display (the tap is purely additive).
    pub fn new(device: &ID3D11Device) -> Option<Self> {
        // NULL-DACL security descriptor: grant every session access to the Global\ object.
        let mut sd = SECURITY_DESCRIPTOR::default();
        let psd = PSECURITY_DESCRIPTOR(core::ptr::addr_of_mut!(sd).cast());
        if unsafe {
            InitializeSecurityDescriptor(psd, 1 /* SECURITY_DESCRIPTOR_REVISION */)
        }
        .is_err()
        {
            error!("frame_tap: InitializeSecurityDescriptor failed");
            return None;
        }
        // bDaclPresent=TRUE, pDacl=None => a NULL DACL (everyone), bDaclDefaulted=FALSE.
        if unsafe { SetSecurityDescriptorDacl(psd, BOOL(1), None, BOOL(0)) }.is_err() {
            error!("frame_tap: SetSecurityDescriptorDacl failed");
            return None;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: BOOL(0),
        };

        let total = SHM_TOTAL as u64;
        let map = match unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                (total >> 32) as u32,
                (total & 0xFFFF_FFFF) as u32,
                w!("Global\\VizLabFrame"),
            )
        } {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                error!("frame_tap: CreateFileMapping(Global\\VizLabFrame) failed");
                return None;
            }
        };

        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, SHM_TOTAL) };
        if view.Value.is_null() {
            error!("frame_tap: MapViewOfFile failed");
            unsafe { CloseHandle(map) }.ok();
            return None;
        }
        let base: *mut u8 = view.Value.cast();

        // Initialise the header: zero everything, then magic + version (format/reserved stay 0).
        let header: *mut u32 = base.cast();
        for i in 0..8usize {
            unsafe { header.add(i).write_volatile(0) };
        }
        unsafe { header.write_volatile(SHM_MAGIC) }; // index 0 = magic
        unsafe { header.add(1).write_volatile(1) }; // index 1 = version

        let ctx = match unsafe { device.GetImmediateContext() } {
            Ok(c) => c,
            Err(_) => {
                error!("frame_tap: GetImmediateContext failed");
                unsafe { UnmapViewOfFile(view) }.ok();
                unsafe { CloseHandle(map) }.ok();
                return None;
            }
        };

        debug!("frame_tap: armed — Global\\VizLabFrame mapped ({SHM_TOTAL} bytes)");
        Some(Self {
            map,
            base,
            ctx,
            staging: None,
            sw: 0,
            sh: 0,
            sfmt: DXGI_FORMAT(0),
            logged_first: false,
        })
    }

    /// Copy one composited frame (the IddCx-acquired surface) into the shmem triple-buffer.
    /// Never panics; any failure skips this frame. `surface_raw` is `IDDCX_METADATA.pSurface`.
    pub fn publish(&mut self, device: &ID3D11Device, surface_raw: *mut c_void) {
        if surface_raw.is_null() {
            return;
        }
        // Borrow the acquired surface (IddCx owns it — do NOT release), then QI to a 2D texture.
        let Some(res) = (unsafe { IDXGIResource::from_raw_borrowed(&surface_raw) }) else {
            return;
        };
        let Ok(tex) = res.cast::<ID3D11Texture2D>() else {
            return;
        };

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (w, h, fmt) = (desc.Width, desc.Height, desc.Format);
        if w == 0 || h == 0 {
            return;
        }

        // (Re)create the CPU-readable staging texture when geometry/format changes.
        if self.staging.is_none() || self.sw != w || self.sh != h || self.sfmt != fmt {
            let sdesc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            if unsafe { device.CreateTexture2D(&sdesc, None, Some(&mut staging)) }.is_err() {
                error!("frame_tap: CreateTexture2D(staging) failed");
                return;
            }
            self.staging = staging;
            self.sw = w;
            self.sh = h;
            self.sfmt = fmt;
        }
        let Some(staging) = self.staging.clone() else {
            return;
        };

        // GPU -> CPU: copy the composited surface into staging, then map it for read.
        unsafe { self.ctx.CopyResource(&staging, &tex) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if unsafe {
            self.ctx
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .is_err()
        {
            error!("frame_tap: Map(staging) failed");
            return;
        }

        self.write_frame(mapped.pData.cast::<u8>(), w, h, mapped.RowPitch);
        unsafe { self.ctx.Unmap(&staging, 0) };

        if !self.logged_first {
            self.logged_first = true;
            debug!("frame_tap: first frame published {w}x{h}");
        }
    }

    /// Write BGRA pixels into slot (gen+1)%3, fence, bump gen. `src` is top-down BGRA,
    /// `pitch` source bytes/row. Oversized frames are nearest-neighbour downscaled to fit
    /// (mirrors `shm_publish`); alpha is forced opaque.
    fn write_frame(&self, src: *const u8, w: u32, h: u32, pitch: u32) {
        let header: *mut u32 = self.base.cast();
        let gen = unsafe { header.add(H_GEN).read_volatile() };
        let slot = ((gen.wrapping_add(1)) % 3) as usize;
        let buf = unsafe { self.base.add(SHM_HEADER + slot * SHM_BUF) };

        let (out_w, out_h, out_stride): (u32, u32, usize);
        if w <= SHM_MAXW && h <= SHM_MAXH {
            out_w = w;
            out_h = h;
            out_stride = (w * 4) as usize;
            for y in 0..h as usize {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src.add(y * pitch as usize),
                        buf.add(y * out_stride),
                        out_stride,
                    );
                }
            }
            // Force opaque (a composited surface may carry A<255).
            let end = out_stride * out_h as usize;
            let mut i = 3usize;
            while i < end {
                unsafe { *buf.add(i) = 255 };
                i += 4;
            }
        } else {
            let scale =
                (f64::from(SHM_MAXW) / f64::from(w)).min(f64::from(SHM_MAXH) / f64::from(h));
            out_w = ((f64::from(w) * scale) as u32).max(1);
            out_h = ((f64::from(h) * scale) as u32).max(1);
            out_stride = (out_w * 4) as usize;
            for y in 0..out_h as usize {
                let sy = (((y as f64) / scale) as usize).min(h as usize - 1);
                for x in 0..out_w as usize {
                    let sx = (((x as f64) / scale) as usize).min(w as usize - 1);
                    unsafe {
                        let sp = src.add(sy * pitch as usize + sx * 4);
                        let dp = buf.add(y * out_stride + x * 4);
                        *dp = *sp;
                        *dp.add(1) = *sp.add(1);
                        *dp.add(2) = *sp.add(2);
                        *dp.add(3) = 255;
                    }
                }
            }
        }

        unsafe {
            header.add(H_WIDTH).write_volatile(out_w);
            header.add(H_HEIGHT).write_volatile(out_h);
            header.add(H_STRIDE).write_volatile(out_stride as u32);
        }
        fence(Ordering::SeqCst); // pixels + dims globally visible before the generation bump
        unsafe { header.add(H_GEN).write_volatile(gen.wrapping_add(1)) };
    }
}

impl Drop for FrameTap {
    fn drop(&mut self) {
        if !self.base.is_null() {
            let view = MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base.cast(),
            };
            unsafe { UnmapViewOfFile(view) }.ok();
        }
        if !self.map.is_invalid() {
            unsafe { CloseHandle(self.map) }.ok();
        }
    }
}
