// SPDX-License-Identifier: GPL-3.0-or-later
//! The planes of a picture in system memory, checked to be complete before a
//! byte of them goes to a texture.
//!
//! The rows go to `write_texture` with their stride as `bytes_per_row`, so a
//! padded plane is uploaded as it is, without repacking.

use std::fmt;

use takt_core::{PixelFormat, VideoFrame};

/// Why a set of planes is not a picture a layer takes.
#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// Width or height is zero.
    Empty,
    /// The stride of a plane is shorter than its rows.
    Stride {
        /// The index of the plane.
        plane: usize,
    },
    /// A plane is missing or holds fewer bytes than its rows need.
    Short {
        /// The index of the plane.
        plane: usize,
    },
    /// A pixel format no layer takes; it is not converted to one that is.
    Format,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty picture"),
            Self::Stride { plane } => write!(f, "plane {plane}: stride shorter than a row"),
            Self::Short { plane } => write!(f, "plane {plane}: fewer bytes than its rows"),
            Self::Format => write!(f, "a pixel format no layer takes"),
        }
    }
}

impl std::error::Error for LayoutError {}

/// What one plane of a format is as a texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneShape {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// The format of the texture the plane is uploaded to.
    pub format: wgpu::TextureFormat,
}

impl PlaneShape {
    /// Bytes of one texel.
    #[must_use]
    pub const fn texel_bytes(&self) -> u32 {
        match self.format {
            wgpu::TextureFormat::R8Unorm => 1,
            wgpu::TextureFormat::Rg8Unorm | wgpu::TextureFormat::R16Unorm => 2,
            _ => 4,
        }
    }
}

/// The shape of plane `plane` of a `width` by `height` picture of `format`;
/// `None` for a plane the format does not have, or a format no layer takes.
#[must_use]
pub fn plane_shape(
    format: PixelFormat,
    plane: usize,
    width: u32,
    height: u32,
) -> Option<PlaneShape> {
    use wgpu::TextureFormat as F;
    let half = (width.div_ceil(2), height.div_ceil(2));
    let full = (width, height);
    let (size, texture) = match (format, plane) {
        (PixelFormat::Nv12, 0) | (PixelFormat::Yuv420P | PixelFormat::Yuv444P, 0) => {
            (full, F::R8Unorm)
        }
        (PixelFormat::Nv12, 1) => (half, F::Rg8Unorm),
        (PixelFormat::P010, 0) | (PixelFormat::Yuv420P10, 0) => (full, F::R16Unorm),
        (PixelFormat::P010, 1) => (half, F::Rg16Unorm),
        (PixelFormat::Yuv420P, 1 | 2) => (half, F::R8Unorm),
        (PixelFormat::Yuv420P10, 1 | 2) => (half, F::R16Unorm),
        (PixelFormat::Yuv444P, 1 | 2) => (full, F::R8Unorm),
        (PixelFormat::Rgb0 | PixelFormat::Rgba8, 0) => (full, F::Rgba8Unorm),
        _ => return None,
    };
    Some(PlaneShape {
        width: size.0,
        height: size.1,
        format: texture,
    })
}

/// One plane of a [`PlanesView`].
#[derive(Clone, Copy, Debug)]
pub struct PlaneView<'a> {
    /// The rows, `stride` bytes apart.
    pub data: &'a [u8],
    /// Bytes from one row to the next.
    pub stride: u32,
    /// What the plane is as a texture.
    pub shape: PlaneShape,
}

/// The planes of one picture in system memory, checked to be complete.
#[derive(Clone, Copy, Debug)]
pub struct PlanesView<'a> {
    format: PixelFormat,
    width: u32,
    height: u32,
    planes: [Option<PlaneView<'a>>; 3],
}

/// Bytes a plane of `rows` rows of `row` bytes at `stride` needs: the last
/// row needs no padding after it. `None` when that does not fit a `usize`.
fn needed(stride: usize, row: usize, rows: usize) -> Option<usize> {
    stride.checked_mul(rows.checked_sub(1)?)?.checked_add(row)
}

impl<'a> PlanesView<'a> {
    /// The planes of a `width` by `height` picture of `format`, each with
    /// its stride in bytes: for NV12 the luma and the interleaved chroma of
    /// half width and height (rounded up), for `rgb0` one plane of four bytes
    /// a pixel.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] when the planes do not hold the picture.
    pub fn new(
        format: PixelFormat,
        width: u32,
        height: u32,
        planes: &[(&'a [u8], u32)],
    ) -> Result<Self, LayoutError> {
        if plane_shape(format, 0, 1, 1).is_none() {
            return Err(LayoutError::Format);
        }
        if width == 0 || height == 0 {
            return Err(LayoutError::Empty);
        }
        let mut out = [None; 3];
        for (index, slot) in out.iter_mut().enumerate() {
            let Some(shape) = plane_shape(format, index, width, height) else {
                break;
            };
            let &(data, stride) = planes
                .get(index)
                .ok_or(LayoutError::Short { plane: index })?;
            let texel = shape.texel_bytes();
            let row = (shape.width as usize)
                .checked_mul(texel as usize)
                .ok_or(LayoutError::Stride { plane: index })?;
            if (stride as usize) < row {
                return Err(LayoutError::Stride { plane: index });
            }
            let need = needed(stride as usize, row, shape.height as usize)
                .ok_or(LayoutError::Short { plane: index })?;
            if data.len() < need {
                return Err(LayoutError::Short { plane: index });
            }
            *slot = Some(PlaneView {
                data,
                stride,
                shape,
            });
        }
        Ok(Self {
            format,
            width,
            height,
            planes: out,
        })
    }

    /// The planes of a frame, in place.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for a format no layer takes or incomplete planes.
    pub fn from_frame(frame: &'a VideoFrame) -> Result<Self, LayoutError> {
        let meta = frame.meta();
        let mut planes = [(&[][..], 0_u32); 3];
        let mut count = 0;
        for (index, slot) in planes.iter_mut().enumerate() {
            if plane_shape(meta.format, index, 1, 1).is_none() {
                break;
            }
            let data = frame
                .plane(index)
                .ok_or(LayoutError::Short { plane: index })?;
            let stride = frame
                .stride(index)
                .and_then(|s| u32::try_from(s).ok())
                .ok_or(LayoutError::Short { plane: index })?;
            *slot = (data, stride);
            count = index + 1;
        }
        Self::new(meta.format, meta.width, meta.height, &planes[..count])
    }

    /// The pixel format.
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// Width and height in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The planes the format has, in order.
    pub fn planes(&self) -> impl Iterator<Item = &PlaneView<'a>> {
        self.planes.iter().flatten()
    }

    /// Plane `index`, `None` when the format has fewer.
    #[must_use]
    pub fn plane(&self, index: usize) -> Option<&PlaneView<'a>> {
        self.planes.get(index).and_then(Option::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use takt_core::{ColorPrimaries, ColorRange, FrameMeta, Timestamp, TransferFunction};

    #[test]
    fn an_nv12_view_checks_strides_and_lengths() {
        let y = vec![0_u8; 16 * 4];
        let uv = vec![0_u8; 8 * 2];
        let nv12 = |w, h, y: &[u8], ys, uv: &[u8], uvs| {
            PlanesView::new(PixelFormat::Nv12, w, h, &[(y, ys), (uv, uvs)]).map(|v| v.size())
        };
        assert_eq!(
            nv12(4, 4, &y, 16, &uv, 8),
            Ok((4, 4)),
            "padded rows are fine"
        );
        assert_eq!(nv12(0, 4, &y, 4, &uv, 4), Err(LayoutError::Empty));
        assert_eq!(
            nv12(4, 4, &y, 3, &uv, 4),
            Err(LayoutError::Stride { plane: 0 })
        );
        assert_eq!(
            nv12(4, 4, &y, 4, &uv, 3),
            Err(LayoutError::Stride { plane: 1 })
        );
        assert_eq!(
            nv12(4, 4, &y[..10], 4, &uv, 4),
            Err(LayoutError::Short { plane: 0 })
        );
        // Odd sizes round the chroma up: 5x3 needs 3x2 UV pairs.
        let uv_small = vec![0_u8; 6 + 5];
        assert_eq!(
            nv12(5, 3, &y, 5, &uv_small, 6),
            Err(LayoutError::Short { plane: 1 })
        );
        // The last row needs no padding after it.
        let y_tight = vec![0_u8; 16 * 3 + 4];
        assert!(nv12(4, 4, &y_tight, 16, &uv, 4).is_ok());
        // A plane that is not handed in at all.
        assert_eq!(
            PlanesView::new(PixelFormat::Nv12, 4, 4, &[(&y, 16)]).map(|v| v.size()),
            Err(LayoutError::Short { plane: 1 })
        );
    }

    #[test]
    fn an_rgb0_view_checks_stride_and_length() {
        let pixels = vec![0_u8; 64 * 3];
        let rgb0 = |w, h, p: &[u8], stride| {
            PlanesView::new(PixelFormat::Rgb0, w, h, &[(p, stride)]).map(|v| v.size())
        };
        assert!(rgb0(4, 3, &pixels, 64).is_ok(), "padded rows are fine");
        assert_eq!(rgb0(0, 3, &pixels, 16), Err(LayoutError::Empty));
        assert_eq!(
            rgb0(4, 3, &pixels, 15),
            Err(LayoutError::Stride { plane: 0 })
        );
        assert_eq!(
            rgb0(4, 3, &pixels[..64 * 2 + 15], 64),
            Err(LayoutError::Short { plane: 0 })
        );
        // The last row needs no padding after it.
        assert!(rgb0(4, 3, &pixels[..64 * 2 + 16], 64).is_ok());
    }

    #[test]
    fn sixteen_bit_planes_count_two_bytes_a_sample() {
        let y = vec![0_u8; 8 * 2];
        let uv = vec![0_u8; 8];
        let p010 = |y: &[u8], ys, uv: &[u8], uvs| {
            PlanesView::new(PixelFormat::P010, 4, 2, &[(y, ys), (uv, uvs)]).map(|v| v.size())
        };
        assert!(p010(&y, 8, &uv, 8).is_ok());
        assert_eq!(p010(&y, 7, &uv, 8), Err(LayoutError::Stride { plane: 0 }));
        assert_eq!(
            p010(&y, 8, &uv[..7], 8),
            Err(LayoutError::Short { plane: 1 })
        );
    }

    #[test]
    fn the_shapes_of_the_planes() {
        use wgpu::TextureFormat as F;
        let shape =
            |format, plane| plane_shape(format, plane, 5, 3).map(|s| (s.width, s.height, s.format));
        assert_eq!(shape(PixelFormat::Nv12, 0), Some((5, 3, F::R8Unorm)));
        assert_eq!(shape(PixelFormat::Nv12, 1), Some((3, 2, F::Rg8Unorm)));
        assert_eq!(shape(PixelFormat::Nv12, 2), None);
        assert_eq!(shape(PixelFormat::P010, 1), Some((3, 2, F::Rg16Unorm)));
        assert_eq!(shape(PixelFormat::Yuv420P, 2), Some((3, 2, F::R8Unorm)));
        assert_eq!(shape(PixelFormat::Yuv420P10, 1), Some((3, 2, F::R16Unorm)));
        assert_eq!(shape(PixelFormat::Yuv444P, 2), Some((5, 3, F::R8Unorm)));
        assert_eq!(shape(PixelFormat::Rgba8, 0), Some((5, 3, F::Rgba8Unorm)));
        assert_eq!(shape(PixelFormat::Rgb0, 1), None);
        assert_eq!(shape(PixelFormat::Rgba16F, 0), None);
    }

    fn frame(format: PixelFormat) -> VideoFrame {
        VideoFrame::alloc(FrameMeta {
            width: 6,
            height: 2,
            format,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            range: ColorRange::Full,
            pts: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    #[test]
    fn a_view_reads_a_frame_in_place_and_refuses_a_format_without_a_path() {
        let mut rgb = frame(PixelFormat::Rgb0);
        rgb.plane_mut(0).unwrap()[..4].copy_from_slice(&[1, 2, 3, 0]);
        let view = PlanesView::from_frame(&rgb).unwrap();
        assert_eq!((view.size(), view.format()), ((6, 2), PixelFormat::Rgb0));
        let plane = view.plane(0).unwrap();
        assert_eq!(&plane.data[..4], &[1, 2, 3, 0]);
        // The rows of the frame itself, at its aligned stride: 24 bytes of
        // picture in rows of 64.
        assert_eq!(plane.data.as_ptr(), rgb.plane(0).unwrap().as_ptr());
        assert_eq!(plane.stride, 64);
        assert!(view.plane(1).is_none());
        // Every format with a path reads from a frame of takt-core.
        for (format, planes) in [
            (PixelFormat::Nv12, 2),
            (PixelFormat::P010, 2),
            (PixelFormat::Yuv420P, 3),
            (PixelFormat::Yuv420P10, 3),
            (PixelFormat::Yuv444P, 3),
            (PixelFormat::Rgba8, 1),
        ] {
            let f = frame(format);
            let view = PlanesView::from_frame(&f).unwrap();
            assert_eq!(view.planes().count(), planes, "{format:?}");
        }
        assert_eq!(
            PlanesView::from_frame(&frame(PixelFormat::Rgba16F)).map(|v| v.size()),
            Err(LayoutError::Format)
        );
    }
}
