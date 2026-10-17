use crate::bitmaps::BitmapImage;
use core_graphics::base::{kCGBitmapByteOrder32Big, kCGImageAlphaLast, kCGRenderingIntentDefault};
use core_graphics::color_space::CGColorSpace;
use core_graphics::data_provider::CGDataProvider;
use core_graphics::image::CGImage;
use std::marker::PhantomData;
use std::sync::Arc;

/// Allows referencing a BitmapImage as a CGImage.
/// The CGImage holds a copy of the BitmapImage's pixels: whoever it is
/// handed to may keep it after the BitmapImage is gone, as AppKit keeps
/// the application icon.
pub struct BitmapRef<'a> {
    image: CGImage,
    _phantom: PhantomData<&'a ()>,
}

impl<'a> std::ops::Deref for BitmapRef<'a> {
    type Target = CGImage;
    fn deref(&self) -> &CGImage {
        &self.image
    }
}

impl<'a> BitmapRef<'a> {
    pub fn with_image(image: &'a dyn BitmapImage) -> Self {
        let (width, height) = image.image_dimensions();
        let byte_size = width * height * 4;

        // This is safe because BitmapRef<'a> borrows the
        // data from BitmapImage and the compiler will ensure
        // that the lifetime is maintained
        let slice = unsafe {
            let data = image.pixel_data();
            std::slice::from_raw_parts(data, byte_size)
        };
        let provider = CGDataProvider::from_buffer(Arc::new(slice.to_vec()));

        let should_interpolate = true;
        let bytes_per_row = width * 4;
        let image = CGImage::new(
            width,
            height,
            8,
            32,
            bytes_per_row,
            &CGColorSpace::create_device_rgb(),
            // RGBA, unpremultiplied: what Image::with_rgba32 keeps of a
            // decoded PNG, and what the X11 icon reads it as. Read as BGRA,
            // the blue icon came out red.
            kCGImageAlphaLast | kCGBitmapByteOrder32Big,
            &provider,
            should_interpolate,
            kCGRenderingIntentDefault,
        );
        BitmapRef {
            image,
            _phantom: PhantomData,
        }
    }
}
