//! Render elements and texture helpers shared by the backends.

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::utils::{Logical, Physical, Size, Transform};

smithay::render_elements! {
    /// Everything wdm can put on screen.
    ///
    /// An enum rather than a bare surface element because the give-up screen is
    /// an image wdm rasterises itself, not a client surface — and both backends
    /// composite through the same element list, so they need one type.
    pub WdmElement<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Image = MemoryRenderBufferRenderElement<R>,
}

/// Build the give-up screen as a render element.
///
/// The buffer is handed in so it can be built once and reused: the error screen
/// never changes, and rasterising a full-screen image every frame would be
/// wasteful on the one code path that runs when everything else has failed.
pub fn error_buffer(reason: &str, size: Size<i32, Physical>) -> MemoryRenderBuffer {
    let image = crate::errscreen::render(reason, size.w, size.h);
    MemoryRenderBuffer::from_slice(
        &image.data,
        Fourcc::Abgr8888,
        (image.width, image.height),
        1,
        Transform::Normal,
        None,
    )
}

/// The logical size to hand `MemoryRenderBufferRenderElement::from_buffer` for a
/// give-up screen rasterised at `mode` physical pixels.
///
/// [`error_buffer`] rasterises at the output's *physical* mode size with a
/// buffer scale of 1, so passing `None` for `from_buffer`'s `size` makes smithay
/// derive `size = pixels / buffer_scale` — the mode size reinterpreted as
/// *logical* — and then multiply it by the output scale when it draws. On a
/// `scale = 2` output that composites the error screen at twice the framebuffer:
/// only the top-left quarter is on screen, and the centred "switch to tty1" line
/// is off it. The nested backend hardcodes scale 1, where the two agree, so this
/// is invisible in development. It is also the one path where nothing else can
/// help the user, hence a size computed here and passed explicitly.
pub fn error_element_size(mode: Size<i32, Physical>, scale: f64) -> Size<i32, Logical> {
    mode.to_f64().to_logical(scale).to_i32_round()
}

/// A small white arrow with a dark outline, for when no client has set a cursor.
///
/// Drawn in code rather than loaded from an xcursor theme: wdm runs before any
/// user session, so it cannot rely on a theme being installed or on the
/// per-user setting that selects one. Built once and reused.
pub fn pointer_buffer() -> &'static MemoryRenderBuffer {
    static POINTER: std::sync::OnceLock<MemoryRenderBuffer> = std::sync::OnceLock::new();

    POINTER.get_or_init(|| {
        const W: i32 = 12;
        const H: i32 = 19;
        // Classic arrow: each row is the run of solid pixels from the left edge.
        const RUN: [i32; H as usize] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 6, 5, 5, 4, 4, 3, 2, 1];

        let mut data = vec![0u8; (W * H * 4) as usize];
        for (row, len) in RUN.iter().enumerate() {
            for col in 0..*len {
                let offset = ((row as i32 * W + col) * 4) as usize;
                // Outline on the trailing pixel of each row so the arrow stays
                // visible against a light background.
                let edge = col == len - 1 || row as i32 == H - 1;
                let value = if edge { 0x20 } else { 0xff };
                data[offset..offset + 3].copy_from_slice(&[value, value, value]);
                data[offset + 3] = 0xff;
            }
        }

        MemoryRenderBuffer::from_slice(&data, Fourcc::Abgr8888, (W, H), 1, Transform::Normal, None)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What smithay does with the size at draw time: `physical_size` scales the
    /// element's logical size by the output scale, with the element at the
    /// origin. Reproduced here because building a real element needs a renderer,
    /// and the give-up screen is exactly the path with no GPU to spare.
    fn geometry(size: Size<i32, Logical>, scale: f64) -> Size<i32, Physical> {
        size.to_f64().to_physical(scale).to_i32_round()
    }

    #[test]
    fn the_give_up_screen_fills_the_mode_at_every_scale() {
        // The defect this guards: leaving `size` as None, which makes smithay
        // read the physical mode size as logical and draw it scale times too
        // big. At scale 2 the user sees the top-left quarter of an error message
        // and no way to act on it.
        let mode = Size::<i32, Physical>::from((3840, 2160));

        for scale in [1.0, 1.5, 2.0] {
            assert_eq!(
                geometry(error_element_size(mode, scale), scale),
                mode,
                "at scale {scale}"
            );
        }
    }

    #[test]
    fn the_logical_size_shrinks_as_the_scale_grows() {
        // The direction matters as much as the arithmetic: dividing where
        // smithay multiplies is the same bug the other way round.
        let mode = Size::<i32, Physical>::from((1920, 1080));
        assert_eq!(error_element_size(mode, 1.0), Size::from((1920, 1080)));
        assert_eq!(error_element_size(mode, 2.0), Size::from((960, 540)));
    }
}
