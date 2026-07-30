//! Render elements and texture helpers shared by the backends.

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::utils::{Physical, Size, Transform};

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
