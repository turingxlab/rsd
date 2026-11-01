use crate::network::http::{Api, UploadFile};
use anyhow::Context;
use std::io::Cursor;
use std::time::Instant;
use xcap::{
    Monitor,
    image::{ImageFormat, RgbaImage},
};

pub async fn screenshot() -> anyhow::Result<()> {
    let start = Instant::now();
    let files = tokio::task::spawn_blocking(capture_files)
        .await
        .context("Screenshot task failed")??;
    let count = files.len();
    Api::upload_files(files).await?;
    tracing::info!("Uploaded {count} screenshots in {:?}", start.elapsed());
    Ok(())
}

fn capture_files() -> anyhow::Result<Vec<UploadFile>> {
    let monitors = Monitor::all().context("Failed to enumerate monitors")?;
    anyhow::ensure!(!monitors.is_empty(), "No monitors available for screenshot");
    monitors
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| {
            let number = index + 1;
            let image = monitor
                .capture_image()
                .with_context(|| format!("Failed to capture monitor {number}"))?;
            Ok(UploadFile {
                filename: format!("screenshot-{number}.png"),
                content_type: "image/png".to_string(),
                data: encode_png(&image)
                    .with_context(|| format!("Failed to encode monitor {number}"))?,
            })
        })
        .collect()
}

fn encode_png(image: &RgbaImage) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Cursor::new(Vec::new());
    image
        .write_to(&mut buffer, ImageFormat::Png)
        .context("Failed to encode screenshot as PNG")?;
    Ok(buffer.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let image = RgbaImage::from_pixel(2, 3, xcap::image::Rgba([12, 34, 56, 255]));
        let bytes = encode_png(&image).expect("PNG encoding should succeed");
        let decoded = xcap::image::load_from_memory_with_format(&bytes, ImageFormat::Png)
            .expect("PNG should decode")
            .to_rgba8();
        assert_eq!(decoded, image);
    }
}
