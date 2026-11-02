use crate::network::http::{Api, UploadFile};
use anyhow::Context;
use std::time::Instant;

#[cfg(any(target_os = "linux", test))]
mod pixels;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

struct Image {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

fn rgba_len(width: u32, height: u32) -> anyhow::Result<usize> {
    anyhow::ensure!(width > 0 && height > 0, "Invalid screenshot dimensions");
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|size| size.checked_mul(4))
        .filter(|size| *size <= isize::MAX as usize)
        .context("Screenshot dimensions overflow")
}

fn encode_png(image: Image) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        image.data.len() == rgba_len(image.width, image.height)?,
        "Invalid screenshot buffer length"
    );
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("Failed to encode PNG header")?;
        writer
            .write_image_data(&image.data)
            .context("Failed to encode PNG pixels")?;
        writer.finish().context("Failed to finish PNG")?;
    }
    Ok(bytes)
}

pub async fn screenshot() -> anyhow::Result<()> {
    let start = Instant::now();
    let files = tokio::task::spawn_blocking(capture_files)
        .await
        .context("Screenshot task failed")??;
    let count = files.len();
    Api::upload_screenshots(files).await?;
    tracing::info!("Uploaded {count} screenshots in {:?}", start.elapsed());
    Ok(())
}

fn capture_files() -> anyhow::Result<Vec<UploadFile>> {
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    {
        let mut files = Vec::new();
        platform::capture(&mut |image| {
            let number = files.len() + 1;
            files.push(UploadFile {
                filename: format!("screenshot-{number}.png"),
                content_type: "image/png".to_string(),
                data: encode_png(image)
                    .with_context(|| format!("Failed to encode display {number}"))?,
            });
            Ok(())
        })?;
        anyhow::ensure!(!files.is_empty(), "No displays available for screenshot");
        Ok(files)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    anyhow::bail!("Screenshot capture is unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let data = vec![12, 34, 56, 255, 78, 90, 123, 255];
        let bytes = encode_png(Image {
            width: 1,
            height: 2,
            data: data.clone(),
        })
        .unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!((info.width, info.height), (1, 2));
        assert_eq!(&decoded[..info.buffer_size()], &data);
    }

    #[test]
    fn rejects_invalid_images() {
        assert!(rgba_len(0, 1).is_err());
        assert!(rgba_len(u32::MAX, u32::MAX).is_err());
        assert!(
            encode_png(Image {
                width: 2,
                height: 2,
                data: vec![0; 4]
            })
            .is_err()
        );
    }
}
