//! Checked CPU pixel transforms used by Desktop Duplication.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Rotation {
    #[default]
    Identity,
    Clockwise,
    HalfTurn,
    Counterclockwise,
}

impl Rotation {
    pub(super) const fn portrait(self) -> bool {
        matches!(self, Self::Clockwise | Self::Counterclockwise)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum CursorKind {
    Monochrome,
    MaskedColor,
    #[default]
    Color,
}

#[derive(Default)]
pub(super) struct Pointer {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub kind: CursorKind,
    pub width: usize,
    /// Monochrome shapes contain equally sized AND and XOR planes.
    pub height: usize,
    pub pitch: usize,
    pub shape: Vec<u8>,
}

pub(super) fn image_len(width: usize, height: usize) -> Result<usize, String> {
    if width == 0 || height == 0 {
        return Err("Invalid image dimensions.".into());
    }
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "The image dimensions are too large.".into())
}

fn resize_image(destination: &mut Vec<u8>, width: usize, height: usize) -> Result<(), String> {
    let len = image_len(width, height)?;
    destination
        .try_reserve(len.saturating_sub(destination.len()))
        .map_err(|error| format!("Unable to allocate the captured image: {error}"))?;
    destination.resize(len, 0);
    Ok(())
}

pub(super) fn rotate_desktop(
    source: &[u8],
    stride: usize,
    source_width: usize,
    source_height: usize,
    rotation: Rotation,
    destination: &mut Vec<u8>,
) -> Result<(), String> {
    let row_bytes = image_len(source_width, 1)?;
    let required = source_height
        .checked_sub(1)
        .and_then(|rows| rows.checked_mul(stride))
        .and_then(|bytes| bytes.checked_add(row_bytes))
        .ok_or_else(|| "Invalid captured image dimensions.".to_owned())?;
    if stride < row_bytes || source.len() < required {
        return Err("The captured image has invalid row data.".into());
    }
    let (width, height) = if rotation.portrait() {
        (source_height, source_width)
    } else {
        (source_width, source_height)
    };
    resize_image(destination, width, height)?;
    if rotation == Rotation::Identity {
        for (output, input) in destination
            .chunks_exact_mut(row_bytes)
            .zip(source.chunks(stride))
        {
            output.copy_from_slice(&input[..row_bytes]);
        }
        return Ok(());
    }
    for y in 0..height {
        for x in 0..width {
            let (sx, sy) = match rotation {
                Rotation::Clockwise => (y, source_height - 1 - x),
                Rotation::HalfTurn => (source_width - 1 - x, source_height - 1 - y),
                Rotation::Counterclockwise => (source_width - 1 - y, x),
                Rotation::Identity => (x, y),
            };
            let input = sy * stride + sx * 4;
            let output = (y * width + x) * 4;
            destination[output..output + 4].copy_from_slice(&source[input..input + 4]);
        }
    }
    Ok(())
}

pub(super) fn validate_pointer(pointer: &Pointer) -> Result<(), String> {
    let minimum_pitch = match pointer.kind {
        CursorKind::Monochrome => pointer.width.div_ceil(8),
        CursorKind::Color | CursorKind::MaskedColor => pointer
            .width
            .checked_mul(4)
            .ok_or_else(|| "Invalid cursor dimensions.".to_owned())?,
    };
    if pointer.width == 0
        || pointer.height == 0
        || pointer.pitch < minimum_pitch
        || (pointer.kind == CursorKind::Monochrome && !pointer.height.is_multiple_of(2))
        || pointer
            .pitch
            .checked_mul(pointer.height)
            .is_none_or(|bytes| bytes > pointer.shape.len())
    {
        return Err("Invalid cursor image data.".into());
    }
    Ok(())
}

pub(super) fn composite_pointer(
    desktop: &mut [u8],
    width: usize,
    height: usize,
    pointer: &Pointer,
) -> Result<(), String> {
    if !pointer.visible || pointer.shape.is_empty() {
        return Ok(());
    }
    validate_pointer(pointer)?;
    if desktop.len() != image_len(width, height)? {
        return Err("Invalid desktop image data.".into());
    }
    let pointer_height = if pointer.kind == CursorKind::Monochrome {
        pointer.height / 2
    } else {
        pointer.height
    };
    let left = i64::from(pointer.x);
    let top = i64::from(pointer.y);
    let right = left
        .checked_add(i64::try_from(pointer.width).map_err(|_| "Invalid cursor width.")?)
        .ok_or("Invalid cursor width.")?;
    let bottom = top
        .checked_add(i64::try_from(pointer_height).map_err(|_| "Invalid cursor height.")?)
        .ok_or("Invalid cursor height.")?;
    let x_begin = usize::try_from(left.max(0)).map_err(|_| "Invalid cursor position.")?;
    let y_begin = usize::try_from(top.max(0)).map_err(|_| "Invalid cursor position.")?;
    let x_end = usize::try_from(right.max(0))
        .map_err(|_| "Invalid cursor position.")?
        .min(width);
    let y_end = usize::try_from(bottom.max(0))
        .map_err(|_| "Invalid cursor position.")?
        .min(height);
    for y in y_begin..y_end {
        let py = usize::try_from(i64::try_from(y).map_err(|_| "Invalid image height.")? - top)
            .map_err(|_| "Invalid cursor position.")?;
        let shape_row = py * pointer.pitch;
        for x in x_begin..x_end {
            let px = usize::try_from(i64::try_from(x).map_err(|_| "Invalid image width.")? - left)
                .map_err(|_| "Invalid cursor position.")?;
            let pixel = &mut desktop[(y * width + x) * 4..(y * width + x + 1) * 4];
            match pointer.kind {
                CursorKind::Monochrome => {
                    let bit = 0x80_u8 >> (px % 8);
                    let and_mask = if pointer.shape[shape_row + px / 8] & bit == 0 {
                        0
                    } else {
                        255
                    };
                    let xor_mask =
                        if pointer.shape[shape_row + pointer_height * pointer.pitch + px / 8] & bit
                            == 0
                        {
                            0
                        } else {
                            255
                        };
                    for channel in &mut pixel[..3] {
                        *channel = (*channel & and_mask) ^ xor_mask;
                    }
                }
                CursorKind::MaskedColor | CursorKind::Color => {
                    let offset = shape_row + px * 4;
                    let cursor = &pointer.shape[offset..offset + 4];
                    for (channel, cursor_channel) in pixel[..3].iter_mut().zip(&cursor[..3]) {
                        *channel = if pointer.kind == CursorKind::MaskedColor {
                            if cursor[3] == 0 {
                                *cursor_channel
                            } else {
                                *channel ^ cursor_channel
                            }
                        } else {
                            let alpha = u32::from(cursor[3]);
                            let blended = (u32::from(*cursor_channel) * alpha
                                + u32::from(*channel) * (255 - alpha)
                                + 127)
                                / 255;
                            u8::try_from(blended).map_err(|_| "Invalid cursor blend.")?
                        };
                    }
                }
            }
            pixel[3] = 255;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct AxisSample {
    first: usize,
    second: usize,
    fraction: u32,
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "Validated display dimensions are at most 32768; clamped positions and 8-bit weights fit exactly."
)]
pub(super) fn scale_axis(source: usize, destination: usize) -> Result<Vec<AxisSample>, String> {
    if source == 0 || destination == 0 || source > 32768 || destination > 32768 {
        return Err("Invalid image scaling dimensions.".into());
    }
    Ok((0..destination)
        .map(|index| {
            let position = ((index as f64 + 0.5) * source as f64 / destination as f64 - 0.5)
                .clamp(0.0, (source - 1) as f64);
            let first = position as usize;
            AxisSample {
                first,
                second: (first + 1).min(source - 1),
                fraction: (position - first as f64).mul_add(256.0, 0.5) as u32,
            }
        })
        .collect())
}

pub(super) fn scale_desktop(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    xs: &[AxisSample],
    ys: &[AxisSample],
    destination: &mut Vec<u8>,
) -> Result<(), String> {
    scale_desktop_impl(
        source,
        source_width,
        source_height,
        xs,
        ys,
        destination,
        true,
    )
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn blend_sse2(
    a: [u8; 4],
    b: [u8; 4],
    c: [u8; 4],
    d: [u8; 4],
    horizontal: u32,
    vertical: u32,
) -> [u8; 4] {
    use std::arch::x86_64::{
        _mm_add_epi16, _mm_cvtsi128_si32, _mm_cvtsi32_si128, _mm_mullo_epi16, _mm_packus_epi16,
        _mm_set1_epi16, _mm_setzero_si128, _mm_srli_epi16, _mm_unpacklo_epi8,
    };
    let weight = |value: u32| {
        let bytes = value.to_le_bytes();
        i16::from_le_bytes([bytes[0], bytes[1]])
    };
    // SAFETY: SSE2 is baseline on x86_64. All intrinsics operate only on local
    // register values. Validated axis weights are 0..=256; weighted 8-bit
    // channels plus rounding stay below 65536, preserving the original math.
    let mut result = unsafe {
        let zero = _mm_setzero_si128();
        let rounding = _mm_set1_epi16(128);
        let va = _mm_unpacklo_epi8(_mm_cvtsi32_si128(i32::from_le_bytes(a)), zero);
        let vb = _mm_unpacklo_epi8(_mm_cvtsi32_si128(i32::from_le_bytes(b)), zero);
        let vc = _mm_unpacklo_epi8(_mm_cvtsi32_si128(i32::from_le_bytes(c)), zero);
        let vd = _mm_unpacklo_epi8(_mm_cvtsi32_si128(i32::from_le_bytes(d)), zero);
        let left = _mm_set1_epi16(weight(256 - horizontal));
        let right = _mm_set1_epi16(weight(horizontal));
        let upper = _mm_srli_epi16(
            _mm_add_epi16(
                rounding,
                _mm_add_epi16(_mm_mullo_epi16(va, left), _mm_mullo_epi16(vb, right)),
            ),
            8,
        );
        let lower = _mm_srli_epi16(
            _mm_add_epi16(
                rounding,
                _mm_add_epi16(_mm_mullo_epi16(vc, left), _mm_mullo_epi16(vd, right)),
            ),
            8,
        );
        let blended = _mm_srli_epi16(
            _mm_add_epi16(
                rounding,
                _mm_add_epi16(
                    _mm_mullo_epi16(upper, _mm_set1_epi16(weight(256 - vertical))),
                    _mm_mullo_epi16(lower, _mm_set1_epi16(weight(vertical))),
                ),
            ),
            8,
        );
        _mm_cvtsi128_si32(_mm_packus_epi16(blended, zero)).to_le_bytes()
    };
    result[3] = 255;
    result
}

fn scale_desktop_impl(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    xs: &[AxisSample],
    ys: &[AxisSample],
    destination: &mut Vec<u8>,
    use_simd: bool,
) -> Result<(), String> {
    if source.len() != image_len(source_width, source_height)? {
        return Err("Invalid image scaling data.".into());
    }
    if xs.iter().any(|sample| {
        sample.first >= source_width || sample.second >= source_width || sample.fraction > 256
    }) || ys.iter().any(|sample| {
        sample.first >= source_height || sample.second >= source_height || sample.fraction > 256
    }) {
        return Err("Invalid image scaling coordinates.".into());
    }
    let width = xs.len();
    let height = ys.len();
    resize_image(destination, width, height)?;
    if source_width == width && source_height == height {
        destination.copy_from_slice(source);
        return Ok(());
    }
    for (sample_y, output) in ys.iter().zip(destination.chunks_exact_mut(width * 4)) {
        let row0 = source
            [sample_y.first * source_width * 4..(sample_y.first + 1) * source_width * 4]
            .as_chunks::<4>()
            .0;
        let row1 = source
            [sample_y.second * source_width * 4..(sample_y.second + 1) * source_width * 4]
            .as_chunks::<4>()
            .0;
        for (sample_x, pixel) in xs.iter().zip(output.as_chunks_mut::<4>().0) {
            let a = row0[sample_x.first];
            let b = row0[sample_x.second];
            let c = row1[sample_x.first];
            let d = row1[sample_x.second];
            #[cfg(target_arch = "x86_64")]
            if use_simd {
                pixel.copy_from_slice(&blend_sse2(
                    a,
                    b,
                    c,
                    d,
                    sample_x.fraction,
                    sample_y.fraction,
                ));
                continue;
            }
            #[cfg(not(target_arch = "x86_64"))]
            let _ = use_simd;
            for (channel, output) in pixel[..3].iter_mut().enumerate() {
                let upper = (128
                    + u32::from(a[channel]) * (256 - sample_x.fraction)
                    + u32::from(b[channel]) * sample_x.fraction)
                    >> 8;
                let lower = (128
                    + u32::from(c[channel]) * (256 - sample_x.fraction)
                    + u32::from(d[channel]) * sample_x.fraction)
                    >> 8;
                let blended =
                    (128 + upper * (256 - sample_y.fraction) + lower * sample_y.fraction) >> 8;
                // Convex combinations of 8-bit channels remain in 0..=255.
                *output = blended.to_le_bytes()[0];
            }
            pixel[3] = 255;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_and_padded_rows() -> Result<(), String> {
        let image = [
            1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255, 0, 0, 0, 0, 4, 4, 4, 255, 5, 5, 5, 255, 6, 6,
            6, 255,
        ];
        let mut rotated = Vec::new();
        for (rotation, expected) in [
            (Rotation::Identity, [1, 2, 3, 4, 5, 6]),
            (Rotation::Clockwise, [4, 1, 5, 2, 6, 3]),
            (Rotation::HalfTurn, [6, 5, 4, 3, 2, 1]),
            (Rotation::Counterclockwise, [3, 6, 2, 5, 1, 4]),
        ] {
            rotate_desktop(&image, 16, 3, 2, rotation, &mut rotated)?;
            assert_eq!(
                rotated
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|pixel| pixel[0])
                    .collect::<Vec<_>>(),
                expected
            );
        }
        assert!(rotate_desktop(&image[..8], 16, 3, 2, Rotation::Identity, &mut rotated).is_err());
        Ok(())
    }

    #[test]
    fn cursor_masks_alpha_and_clipping() -> Result<(), String> {
        let mut pointer = Pointer {
            visible: true,
            width: 4,
            height: 2,
            pitch: 1,
            kind: CursorKind::Monochrome,
            shape: vec![0x30, 0x50],
            ..Pointer::default()
        };
        let mut desktop = vec![0x40; 16];
        composite_pointer(&mut desktop, 4, 1, &pointer)?;
        assert_eq!(
            [desktop[0], desktop[4], desktop[8], desktop[12]],
            [0, 255, 0x40, 0xBF]
        );
        pointer.kind = CursorKind::MaskedColor;
        pointer.width = 2;
        pointer.height = 1;
        pointer.pitch = 8;
        pointer.shape = vec![0x10, 0x20, 0x30, 0, 0xAA, 0xBB, 0xCC, 255];
        desktop.fill(0x40);
        composite_pointer(&mut desktop, 4, 1, &pointer)?;
        assert_eq!(
            &desktop[..8],
            &[
                0x10,
                0x20,
                0x30,
                255,
                0x40 ^ 0xAA,
                0x40 ^ 0xBB,
                0x40 ^ 0xCC,
                255
            ]
        );
        pointer.kind = CursorKind::Color;
        pointer.x = -1;
        pointer.shape = vec![0, 0, 0, 0, 64, 32, 16, 128];
        desktop.fill(100);
        composite_pointer(&mut desktop, 4, 1, &pointer)?;
        assert_eq!(&desktop[..8], &[82, 66, 58, 255, 100, 100, 100, 100]);
        pointer.pitch = 1;
        assert!(validate_pointer(&pointer).is_err());
        pointer.kind = CursorKind::Monochrome;
        assert!(validate_pointer(&pointer).is_err());
        Ok(())
    }

    #[test]
    fn bilinear_rounding_and_identity() -> Result<(), String> {
        let square = [
            0, 0, 0, 255, 40, 40, 40, 255, 80, 80, 80, 255, 120, 120, 120, 255,
        ];
        let mut scaled = Vec::new();
        scale_desktop(
            &square,
            2,
            2,
            &scale_axis(2, 1)?,
            &scale_axis(2, 1)?,
            &mut scaled,
        )?;
        assert_eq!(scaled, [60, 60, 60, 255]);
        scale_desktop(
            &square,
            2,
            2,
            &scale_axis(2, 2)?,
            &scale_axis(2, 2)?,
            &mut scaled,
        )?;
        assert_eq!(scaled, square);
        // Rounding each horizontal row precedes rounding the vertical blend.
        let rounding = [0, 0, 0, 255, 1, 1, 1, 255, 1, 1, 1, 255, 2, 2, 2, 255];
        scale_desktop(
            &rounding,
            2,
            2,
            &scale_axis(2, 1)?,
            &scale_axis(2, 1)?,
            &mut scaled,
        )?;
        assert_eq!(scaled, [2, 2, 2, 255]);
        Ok(())
    }

    #[test]
    fn simd_matches_two_stage_scalar_rounding() -> Result<(), String> {
        let source: Vec<u8> = (0_u32..17 * 11 * 4)
            .map(|index| index.wrapping_mul(157).wrapping_add(93).to_le_bytes()[0])
            .collect();
        let mut scalar = Vec::new();
        let mut simd = Vec::new();
        for (width, height) in [(1, 1), (7, 5), (18, 12), (32, 24)] {
            let xs = scale_axis(17, width)?;
            let ys = scale_axis(11, height)?;
            scale_desktop_impl(&source, 17, 11, &xs, &ys, &mut scalar, false)?;
            scale_desktop(&source, 17, 11, &xs, &ys, &mut simd)?;
            assert_eq!(scalar, simd);
        }
        Ok(())
    }

    #[test]
    #[ignore = "run in release mode to measure 4K to 1080p scaling throughput"]
    fn scaling_performance_4k_to_1080p() -> Result<(), String> {
        let source = vec![137; image_len(3840, 2160)?];
        let xs = scale_axis(3840, 1920)?;
        let ys = scale_axis(2160, 1080)?;
        let mut output = Vec::new();
        for simd in [false, true] {
            let started = std::time::Instant::now();
            for _ in 0..30 {
                scale_desktop_impl(
                    std::hint::black_box(&source),
                    3840,
                    2160,
                    &xs,
                    &ys,
                    &mut output,
                    simd,
                )?;
                std::hint::black_box(&output);
            }
            let mean = started.elapsed().as_secs_f64() * 1000.0 / 30.0;
            println!("4K -> 1080p scaling SIMD={simd}: {mean:.3} ms/frame (30fps total budget 33.333 ms)");
        }
        Ok(())
    }
}
