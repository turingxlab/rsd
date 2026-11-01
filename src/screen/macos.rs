use super::{Image, rgba_len};
use anyhow::Context;
use block2::RcBlock;
use objc2::{AllocAnyThread, rc::Retained};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGPreflightScreenCaptureAccess,
};
use objc2_foundation::{NSArray, NSError, NSOperatingSystemVersion, NSProcessInfo};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};
use std::{sync::mpsc, time::Duration};

const TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn capture(emit: &mut impl FnMut(Image) -> anyhow::Result<()>) -> anyhow::Result<()> {
    objc2::rc::autoreleasepool(|_| {
        let version = NSOperatingSystemVersion {
            majorVersion: 14,
            minorVersion: 0,
            patchVersion: 0,
        };
        anyhow::ensure!(
            NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(version),
            "Screenshot capture requires macOS 14 or later"
        );
        anyhow::ensure!(
            CGPreflightScreenCaptureAccess(),
            "Screen recording permission required: enable rsd in System Settings > Privacy & Security > Screen Recording"
        );
        let (tx, rx) = mpsc::channel();
        let callback = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // 回调参数仅在调用期间有效，保留对象后再交给等待线程。
                let result = unsafe {
                    if let Some(error) = error.as_ref() {
                        Err(anyhow::anyhow!(
                            "Failed to enumerate displays: {}",
                            error.localizedDescription()
                        ))
                    } else {
                        Retained::retain(content).context("Missing shareable screen content")
                    }
                };
                let _ = tx.send(result);
            },
        );
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&callback) };
        let content = rx
            .recv_timeout(TIMEOUT)
            .context("Timed out waiting for display enumeration")??;
        let displays = unsafe { content.displays() };
        for (index, display) in displays.iter().enumerate() {
            let image = capture_display(&display)
                .with_context(|| format!("Failed to capture macOS display {}", index + 1))?;
            emit(image)?;
        }
        Ok(())
    })
}

fn capture_display(display: &objc2_screen_capture_kit::SCDisplay) -> anyhow::Result<Image> {
    // 所有方法均已通过外层 macOS 14 版本检查，且对象在异步请求期间保持存活。
    unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::new(),
        );
        let rect = filter.contentRect();
        let scale = f64::from(filter.pointPixelScale());
        let width = rect.size.width * scale;
        let height = rect.size.height * scale;
        anyhow::ensure!(
            width.is_finite()
                && height.is_finite()
                && width > 0.0
                && height > 0.0
                && width <= u32::MAX as f64
                && height <= u32::MAX as f64,
            "Invalid macOS display dimensions"
        );
        let config = SCStreamConfiguration::new();
        config.setWidth(width.round() as usize);
        config.setHeight(height.round() as usize);
        config.setShowsCursor(false);
        let (tx, rx) = mpsc::channel();
        let callback = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
            // 在回调内复制像素，不在线程之间传递未保留的 CoreGraphics 指针。
            let result = if let Some(error) = error.as_ref() {
                Err(anyhow::anyhow!(
                    "ScreenCaptureKit failed: {}",
                    error.localizedDescription()
                ))
            } else {
                image
                    .as_ref()
                    .context("Missing screenshot image")
                    .and_then(convert_image)
            };
            let _ = tx.send(result);
        });
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&callback),
        );
        rx.recv_timeout(TIMEOUT)
            .context("Timed out waiting for macOS screenshot")?
    }
}

fn convert_image(image: &CGImage) -> anyhow::Result<Image> {
    let width = u32::try_from(CGImage::width(Some(image)))?;
    let height = u32::try_from(CGImage::height(Some(image)))?;
    let mut data = vec![0; rgba_len(width, height)?];
    let space = CGColorSpace::new_device_rgb().context("Failed to create RGB color space")?;
    // 显式绘制到大端 RGBX 缓冲区，避免猜测输入图片的颜色空间和通道顺序。
    let context = unsafe {
        CGBitmapContextCreate(
            data.as_mut_ptr().cast(),
            width as usize,
            height as usize,
            8,
            width as usize * 4,
            Some(&space),
            (1 << 14) | 5,
        )
    }
    .context("Failed to create screenshot bitmap context")?;
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width.into(), height.into()),
    );
    CGContext::draw_image(Some(&context), rect, Some(image));
    drop(context);
    for pixel in data.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Ok(Image {
        width,
        height,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_graphics::CGBitmapContextCreateImage;

    #[test]
    fn normalizes_channels_and_preserves_row_order() {
        let mut pixels = [255u8, 0, 0, 0, 0, 0, 255, 0];
        let space = CGColorSpace::new_device_rgb().unwrap();
        let context = unsafe {
            CGBitmapContextCreate(
                pixels.as_mut_ptr().cast(),
                1,
                2,
                8,
                4,
                Some(&space),
                (1 << 14) | 5,
            )
        }
        .unwrap();
        let image = CGBitmapContextCreateImage(Some(&context)).unwrap();
        let converted = convert_image(&image).unwrap();
        assert_eq!((converted.width, converted.height), (1, 2));
        assert_eq!(converted.data, [255, 0, 0, 255, 0, 0, 255, 255]);
    }
}
