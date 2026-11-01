use super::{Image, rgba_len};
use anyhow::Context;

pub(super) fn decode(
    width: u32,
    height: u32,
    bytes: &[u8],
    bits: u8,
    pad: u8,
    little: bool,
    masks: [u32; 3],
) -> anyhow::Result<Image> {
    let len = rgba_len(width, height)?;
    anyhow::ensure!(
        matches!(bits, 16 | 24 | 32) && matches!(pad, 8 | 16 | 32),
        "Unsupported X11 pixel layout"
    );
    let row_bits = (width as usize)
        .checked_mul(bits as usize)
        .context("X11 row size overflow")?;
    let stride = row_bits
        .div_ceil(pad as usize)
        .checked_mul(pad as usize / 8)
        .context("X11 row size overflow")?;
    anyhow::ensure!(
        bytes.len()
            >= stride
                .checked_mul(height as usize)
                .context("X11 image size overflow")?,
        "Incomplete X11 pixel data"
    );
    let pixel_mask = u32::MAX >> (32 - bits);
    for mask in masks {
        anyhow::ensure!(
            mask != 0 && mask & !pixel_mask == 0,
            "Invalid X11 color mask"
        );
        let normalized = mask >> mask.trailing_zeros();
        anyhow::ensure!(
            normalized & normalized.wrapping_add(1) == 0,
            "Unsupported X11 color mask"
        );
    }
    anyhow::ensure!(
        masks[0] & masks[1] == 0 && masks[0] & masks[2] == 0 && masks[1] & masks[2] == 0,
        "Overlapping X11 color masks"
    );
    let mut data = Vec::with_capacity(len);
    for row in bytes.chunks_exact(stride).take(height as usize) {
        for pixel in row[..width as usize * (bits as usize / 8)].chunks_exact(bits as usize / 8) {
            let mut value = 0u32;
            for (index, byte) in pixel.iter().enumerate() {
                let shift = if little {
                    index
                } else {
                    pixel.len() - 1 - index
                } * 8;
                value |= u32::from(*byte) << shift;
            }
            for mask in masks {
                let shift = mask.trailing_zeros();
                let maximum = mask >> shift;
                data.push((((value & mask) >> shift) as u64 * 255 / maximum as u64) as u8);
            }
            data.push(255);
        }
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

    #[test]
    fn pixel_orders_and_padding() {
        let masks = [0xff0000, 0xff00, 0xff];
        assert_eq!(
            decode(1, 1, &[3, 2, 1, 0], 24, 32, true, masks)
                .unwrap()
                .data,
            [1, 2, 3, 255]
        );
        assert_eq!(
            decode(1, 1, &[0, 1, 2, 3], 32, 32, false, masks)
                .unwrap()
                .data,
            [1, 2, 3, 255]
        );
        assert_eq!(
            decode(1, 1, &[0, 248], 16, 16, true, [0xf800, 0x7e0, 0x1f])
                .unwrap()
                .data,
            [255, 0, 0, 255]
        );
    }

    #[test]
    fn rejects_invalid_buffers() {
        assert!(decode(1, 2, &[0; 4], 32, 32, true, [0xff0000, 0xff00, 0xff]).is_err());
        assert!(decode(1, 1, &[0; 4], 32, 32, true, [0, 0xff00, 0xff]).is_err());
    }
}
