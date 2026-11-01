use super::{Image, pixels::decode};
use anyhow::Context;
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        randr::ConnectionExt as _,
        xproto::{ConnectionExt as _, ImageFormat, ImageOrder, VisualClass},
    },
};

pub(super) fn capture(emit: &mut impl FnMut(Image) -> anyhow::Result<()>) -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var_os("WAYLAND_DISPLAY").is_none()
            && !std::env::var("XDG_SESSION_TYPE")
                .is_ok_and(|value| value.eq_ignore_ascii_case("wayland")),
        "Screenshot capture is unsupported in Wayland sessions"
    );
    let (connection, _) = x11rb::connect(None).context("Failed to connect to X11 display")?;
    let setup = connection.setup();
    let randr = connection.extension_information("RANDR")?.is_some();
    if randr {
        connection
            .randr_query_version(1, 2)?
            .reply()
            .context("Failed to query RandR version")?;
    }
    let mut number = 0;
    for screen in &setup.roots {
        let mut regions = Vec::new();
        if randr {
            let resources = connection
                .randr_get_screen_resources(screen.root)?
                .reply()
                .context("Failed to enumerate X11 display resources")?;
            for crtc in resources.crtcs {
                let info = connection
                    .randr_get_crtc_info(crtc, resources.config_timestamp)?
                    .reply()
                    .context("Failed to query X11 display geometry")?;
                anyhow::ensure!(
                    info.status == x11rb::protocol::randr::SetConfig::SUCCESS,
                    "X11 display configuration changed"
                );
                if info.mode != 0 && info.width > 0 && info.height > 0 {
                    let region = (info.x, info.y, info.width, info.height);
                    if !regions.contains(&region) {
                        regions.push(region);
                    }
                }
            }
        } else {
            regions.push((0, 0, screen.width_in_pixels, screen.height_in_pixels));
        }
        for (x, y, width, height) in regions {
            number += 1;
            let result = (|| {
                let reply = connection
                    .get_image(
                        ImageFormat::Z_PIXMAP,
                        screen.root,
                        x,
                        y,
                        width,
                        height,
                        u32::MAX,
                    )?
                    .reply()?;
                let visual = screen
                    .allowed_depths
                    .iter()
                    .flat_map(|depth| &depth.visuals)
                    .find(|visual| visual.visual_id == reply.visual)
                    .context("Missing X11 screenshot visual")?;
                anyhow::ensure!(
                    visual.class == VisualClass::TRUE_COLOR,
                    "Unsupported X11 visual class"
                );
                let format = setup
                    .pixmap_formats
                    .iter()
                    .find(|format| format.depth == reply.depth)
                    .context("Missing X11 pixel format")?;
                decode(
                    width.into(),
                    height.into(),
                    &reply.data,
                    format.bits_per_pixel,
                    format.scanline_pad,
                    setup.image_byte_order == ImageOrder::LSB_FIRST,
                    [visual.red_mask, visual.green_mask, visual.blue_mask],
                )
            })()
            .with_context(|| format!("Failed to capture X11 display {number}"))?;
            emit(result)?;
        }
    }
    Ok(())
}
