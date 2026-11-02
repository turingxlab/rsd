use super::{Image, rgba_len};
use anyhow::Context;
use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{LPARAM, RECT},
    Graphics::Gdi::*,
    System::LibraryLoader::{GetModuleHandleA, GetProcAddress},
    UI::HiDpi::{
        DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    },
};

type SetThreadDpiAwarenessContextFn =
    unsafe extern "system" fn(DPI_AWARENESS_CONTEXT) -> DPI_AWARENESS_CONTEXT;

struct DpiGuard {
    function: Option<SetThreadDpiAwarenessContextFn>,
    previous: DPI_AWARENESS_CONTEXT,
}

impl Drop for DpiGuard {
    fn drop(&mut self) {
        if let Some(function) = self.function {
            unsafe {
                function(self.previous);
            }
        }
    }
}

fn set_dpi_awareness() -> anyhow::Result<DpiGuard> {
    unsafe {
        // GDI 代码已导入 user32.dll，借用模块句柄避免每次截图增加引用计数。
        let module = GetModuleHandleA(c"user32.dll".as_ptr().cast());
        if module.is_null() {
            return Ok(DpiGuard {
                function: None,
                previous: null_mut(),
            });
        }
        let Some(address) = GetProcAddress(module, c"SetThreadDpiAwarenessContext".as_ptr().cast())
        else {
            // 早期 Windows 10 没有线程 DPI API，继续使用进程默认行为。
            return Ok(DpiGuard {
                function: None,
                previous: null_mut(),
            });
        };
        let function: SetThreadDpiAwarenessContextFn = std::mem::transmute(address);
        let mut previous = function(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if previous.is_null() {
            // 部分版本有线程 DPI API，但尚不支持 V2 上下文。
            previous = function(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
        }
        anyhow::ensure!(
            !previous.is_null(),
            "Failed to set screenshot DPI awareness: {}",
            std::io::Error::last_os_error()
        );
        Ok(DpiGuard {
            function: Some(function),
            previous,
        })
    }
}

struct Surface {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}

impl Drop for Surface {
    fn drop(&mut self) {
        // 先恢复旧对象，再销毁位图和 DC；任一初始化步骤失败也可安全清理。
        unsafe {
            if !self.previous.is_null() && self.previous as isize != -1 {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.memory.is_null() {
                DeleteDC(self.memory);
            }
            if !self.screen.is_null() {
                ReleaseDC(null_mut(), self.screen);
            }
        }
    }
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    state: LPARAM,
) -> i32 {
    // state 只在同步枚举期间指向当前线程持有的 Vec。
    unsafe {
        (*(state as *mut Vec<HMONITOR>)).push(monitor);
    }
    1
}

pub(super) fn capture(emit: &mut impl FnMut(Image) -> anyhow::Result<()>) -> anyhow::Result<()> {
    unsafe {
        let _dpi = set_dpi_awareness()?;
        let mut monitors = Vec::new();
        anyhow::ensure!(
            EnumDisplayMonitors(
                null_mut(),
                null(),
                Some(collect_monitor),
                &mut monitors as *mut Vec<HMONITOR> as LPARAM
            ) != 0,
            "Failed to enumerate Windows displays"
        );
        for (index, monitor) in monitors.into_iter().enumerate() {
            let image = capture_monitor(monitor)
                .with_context(|| format!("Failed to capture Windows display {}", index + 1))?;
            emit(image)?;
        }
        Ok(())
    }
}

unsafe fn capture_monitor(monitor: HMONITOR) -> anyhow::Result<Image> {
    unsafe {
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        anyhow::ensure!(
            GetMonitorInfoW(monitor, &mut info) != 0,
            "Failed to query Windows display"
        );
        let rect = info.rcMonitor;
        let width = rect
            .right
            .checked_sub(rect.left)
            .filter(|v| *v > 0)
            .context("Invalid Windows display width")?;
        let height = rect
            .bottom
            .checked_sub(rect.top)
            .filter(|v| *v > 0)
            .context("Invalid Windows display height")?;
        let len = rgba_len(width as u32, height as u32)?;
        let mut surface = Surface {
            screen: GetDC(null_mut()),
            memory: null_mut(),
            bitmap: null_mut(),
            previous: null_mut(),
        };
        anyhow::ensure!(!surface.screen.is_null(), "Failed to acquire screen DC");
        surface.memory = CreateCompatibleDC(surface.screen);
        anyhow::ensure!(!surface.memory.is_null(), "Failed to create screenshot DC");
        let mut bitmap_info: BITMAPINFO = zeroed();
        bitmap_info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bitmap_info.bmiHeader.biWidth = width;
        bitmap_info.bmiHeader.biHeight = -height;
        bitmap_info.bmiHeader.biPlanes = 1;
        bitmap_info.bmiHeader.biBitCount = 32;
        bitmap_info.bmiHeader.biCompression = BI_RGB;
        let mut pixels = null_mut();
        surface.bitmap = CreateDIBSection(
            surface.screen,
            &bitmap_info,
            DIB_RGB_COLORS,
            &mut pixels,
            null_mut(),
            0,
        );
        anyhow::ensure!(
            !surface.bitmap.is_null() && !pixels.is_null(),
            "Failed to allocate screenshot bitmap"
        );
        surface.previous = SelectObject(surface.memory, surface.bitmap);
        anyhow::ensure!(
            !surface.previous.is_null() && surface.previous as isize != -1,
            "Failed to select screenshot bitmap"
        );
        anyhow::ensure!(
            BitBlt(
                surface.memory,
                0,
                0,
                width,
                height,
                surface.screen,
                rect.left,
                rect.top,
                SRCCOPY | CAPTUREBLT
            ) != 0,
            "Failed to copy Windows display pixels"
        );
        anyhow::ensure!(GdiFlush() != 0, "Failed to flush screenshot pixels");
        let mut data = std::slice::from_raw_parts(pixels.cast::<u8>(), len).to_vec();
        for pixel in data.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        Ok(Image {
            width: width as u32,
            height: height as u32,
            data,
        })
    }
}
