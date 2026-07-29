//! Texture upload and drawing helpers shared by the backends.

use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Frame, ImportMem, Texture};
use smithay::utils::{Physical, Rectangle, Size, Transform};

/// Upload the give-up error screen as a texture.
pub fn error_texture(
    renderer: &mut GlesRenderer,
    reason: &str,
    size: Size<i32, Physical>,
) -> Result<GlesTexture, Box<dyn std::error::Error>> {
    let image = crate::errscreen::render(reason, size.w, size.h);

    Ok(renderer.import_memory(
        &image.data,
        smithay::backend::allocator::Fourcc::Abgr8888,
        (image.width, image.height).into(),
        false,
    )?)
}

/// Draw a texture across the whole output.
pub fn draw_fullscreen<F: Frame>(
    frame: &mut F,
    texture: &F::TextureId,
    size: Size<i32, Physical>,
    damage: &[Rectangle<i32, Physical>],
) -> Result<(), F::Error>
where
    F::TextureId: Texture,
{
    // The whole texture, in buffer coordinates.
    let src = Rectangle::from_size(texture.size()).to_f64();

    frame.render_texture_from_to(
        texture,
        src,
        Rectangle::from_size(size),
        damage,
        &[],
        Transform::Normal,
        1.0,
    )
}
