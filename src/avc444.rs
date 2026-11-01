//! AVC444 ([MS-RDPEGFX] 2.2.4.5 and 2.2.4.6, 3.3.8.3): a 4:4:4 picture sent as
//! two 4:2:0 views, as Windows servers do for clients that can.
//!
//! The main view holds luma and each 2x2 block's average chroma, the auxiliary
//! view the chroma samples the main one leaves out. The client puts them back
//! together into full-resolution colour, so text and thin coloured lines stay
//! sharp where 4:2:0 alone smears them. AVC444 (v1) and AVC444v2 pack the
//! auxiliary view differently; Windows App for Mac takes only v1.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use std::ptr::NonNull;

use anyhow::{anyhow, ensure};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
};

pub use ironrdp_egfx::avc444::Avc444Layout;

use crate::capture::PIXEL_FORMAT_420F;

/// Splits captured BGRA frames into the two NV12 views, from a pool of reused buffers.
pub struct Splitter {
    pool: CFRetained<CVPixelBufferPool>,
    width: usize,
    height: usize,
    layout: Avc444Layout,
}

// SAFETY: a pixel buffer pool is thread-safe; one pipeline thread uses it.
unsafe impl Send for Splitter {}

impl Splitter {
    /// `width` and `height` are whole macroblocks, like every encoded frame here.
    pub fn new(width: usize, height: usize, layout: Avc444Layout) -> anyhow::Result<Self> {
        ensure!(
            width.is_multiple_of(16) && height.is_multiple_of(16),
            "{width}x{height} is not macroblock-aligned"
        );
        let number = |n: usize| -> anyhow::Result<CFRetained<CFNumber>> {
            Ok(CFNumber::new_i32(i32::try_from(n)?))
        };
        let format = CFNumber::new_i64(i64::from(PIXEL_FORMAT_420F));
        let (w, h) = (number(width)?, number(height)?);
        let iosurface = CFDictionary::<CFString, CFType>::empty();
        let attrs = CFDictionary::<CFString, CFType>::from_slices(
            &[
                unsafe { kCVPixelBufferPixelFormatTypeKey },
                unsafe { kCVPixelBufferWidthKey },
                unsafe { kCVPixelBufferHeightKey },
                unsafe { kCVPixelBufferIOSurfacePropertiesKey },
            ],
            &[format.as_ref(), w.as_ref(), h.as_ref(), iosurface.as_ref()],
        );
        let mut raw: *mut CVPixelBufferPool = std::ptr::null_mut();
        // SAFETY: out-pointer per CVPixelBufferPoolCreate; the attributes have the documented types.
        let status = unsafe {
            CVPixelBufferPool::create(None, None, Some(attrs.as_opaque()), NonNull::from(&mut raw))
        };
        let raw = NonNull::new(raw)
            .filter(|_| status == 0)
            .ok_or_else(|| anyhow!("CVPixelBufferPoolCreate: {status}"))?;
        // SAFETY: Create returns a +1 reference.
        let pool = unsafe { CFRetained::from_raw(raw) };
        Ok(Self {
            pool,
            width,
            height,
            layout,
        })
    }

    /// The main and auxiliary views of one BGRA frame of this splitter's size.
    pub fn split(
        &self,
        bgra: &CVPixelBuffer,
    ) -> anyhow::Result<(CFRetained<CVPixelBuffer>, CFRetained<CVPixelBuffer>)> {
        ensure!(
            CVPixelBufferGetWidth(bgra) == self.width
                && CVPixelBufferGetHeight(bgra) == self.height,
            "frame is {}x{}, views are {}x{}",
            CVPixelBufferGetWidth(bgra),
            CVPixelBufferGetHeight(bgra),
            self.width,
            self.height
        );
        let main = self.buffer()?;
        let aux = self.buffer()?;
        {
            let source = Locked::new(bgra, CVPixelBufferLockFlags::ReadOnly)?;
            let main_lock = Locked::new(&main, CVPixelBufferLockFlags::empty())?;
            let aux_lock = Locked::new(&aux, CVPixelBufferLockFlags::empty())?;
            let planes = Planes {
                bgra: source.plane(None),
                main_y: main_lock.plane(Some(0)),
                main_uv: main_lock.plane(Some(1)),
                aux_y: aux_lock.plane(Some(0)),
                aux_uv: aux_lock.plane(Some(1)),
            };
            // SAFETY: every plane is locked, and the buffers are `width` by `height`
            // (checked above for the source, set by the pool for the views).
            unsafe { convert(&planes, self.width, self.height, self.layout) };
        }
        Ok((main, aux))
    }

    fn buffer(&self) -> anyhow::Result<CFRetained<CVPixelBuffer>> {
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: valid pool and out-pointer.
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut raw))
        };
        let raw = NonNull::new(raw)
            .filter(|_| status == 0)
            .ok_or_else(|| anyhow!("CVPixelBufferPoolCreatePixelBuffer: {status}"))?;
        // SAFETY: Create returns a +1 reference.
        Ok(unsafe { CFRetained::from_raw(raw) })
    }
}

/// Holds a pixel buffer's base address locked.
struct Locked<'a> {
    buffer: &'a CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
}

impl<'a> Locked<'a> {
    fn new(buffer: &'a CVPixelBuffer, flags: CVPixelBufferLockFlags) -> anyhow::Result<Self> {
        // SAFETY: valid buffer; unlocked with the same flags in Drop.
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        ensure!(status == 0, "CVPixelBufferLockBaseAddress: {status}");
        Ok(Self { buffer, flags })
    }

    /// The whole buffer for `None`, else one plane.
    fn plane(&self, index: Option<usize>) -> Plane {
        let (base, stride) = match index {
            None => (
                CVPixelBufferGetBaseAddress(self.buffer),
                CVPixelBufferGetBytesPerRow(self.buffer),
            ),
            Some(index) => (
                CVPixelBufferGetBaseAddressOfPlane(self.buffer, index),
                CVPixelBufferGetBytesPerRowOfPlane(self.buffer, index),
            ),
        };
        Plane {
            base: base.cast(),
            stride,
        }
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: locked in `new` with these flags.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, self.flags) };
    }
}

#[derive(Clone, Copy)]
struct Plane {
    base: *mut u8,
    stride: usize,
}

struct Planes {
    bgra: Plane,
    main_y: Plane,
    main_uv: Plane,
    aux_y: Plane,
    aux_uv: Plane,
}

// SAFETY: the threads of `convert` write disjoint rows.
unsafe impl Sync for Planes {}

/// Fills both views from the BGRA plane, two source rows at a time, in bands across threads.
///
/// # Safety
/// The planes are valid for `width` by `height` pixels: BGRA 4 bytes per pixel, NV12 luma one
/// byte per pixel and NV12 chroma one CbCr pair per 2x2 block; `width` is a multiple of 4.
unsafe fn convert(planes: &Planes, width: usize, height: usize, layout: Avc444Layout) {
    let pairs = height / 2;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
    let band = pairs.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for start in (0..pairs).step_by(band) {
            let end = (start + band).min(pairs);
            scope.spawn(move || {
                let mut rows = Rows::new(width);
                for pair in start..end {
                    // SAFETY: per this function's contract; bands do not overlap.
                    unsafe { convert_pair(planes, width, pair, layout, &mut rows) };
                }
            });
        }
    });
}

/// Chroma of the two source rows of one pair.
struct Rows {
    u: [Vec<u8>; 2],
    v: [Vec<u8>; 2],
}

impl Rows {
    fn new(width: usize) -> Self {
        Self {
            u: [vec![0; width], vec![0; width]],
            v: [vec![0; width], vec![0; width]],
        }
    }
}

/// # Safety
/// As for [`convert`], with `pair < height / 2`.
unsafe fn convert_pair(
    planes: &Planes,
    width: usize,
    pair: usize,
    layout: Avc444Layout,
    rows: &mut Rows,
) {
    let half = width / 2;
    let quarter = width / 4;
    let row_of = |plane: Plane, row: usize, len: usize| {
        // SAFETY: the caller guarantees `row` and `len` lie inside the plane.
        unsafe { std::slice::from_raw_parts_mut(plane.base.add(row * plane.stride), len) }
    };

    for k in 0..2 {
        let row = 2 * pair + k;
        let bgra = row_of(planes.bgra, row, width * 4);
        let luma = row_of(planes.main_y, row, width);
        let (u, v) = (&mut rows.u[k], &mut rows.v[k]);
        for (x, pixel) in bgra.as_chunks::<4>().0.iter().enumerate() {
            let (y, cb, cr) = rgb_to_yuv(pixel[2], pixel[1], pixel[0]);
            luma[x] = y;
            u[x] = cb;
            v[x] = cr;
        }
        match layout {
            // B4 and B5: odd chroma columns, U in the left half of the auxiliary luma row, V in the right.
            Avc444Layout::V2 => {
                let aux = row_of(planes.aux_y, row, width);
                for c in 0..half {
                    aux[c] = u[2 * c + 1];
                    aux[half + c] = v[2 * c + 1];
                }
            }
            // B4 and B5: in each macroblock row, auxiliary luma rows 0 to 7 carry the odd U rows
            // and rows 8 to 15 the odd V rows.
            Avc444Layout::V1 if k == 1 => {
                let macroblock = row - row % MACROBLOCK;
                let within = (row % MACROBLOCK) / 2;
                row_of(planes.aux_y, macroblock + within, width).copy_from_slice(u);
                row_of(planes.aux_y, macroblock + MACROBLOCK / 2 + within, width)
                    .copy_from_slice(v);
            }
            Avc444Layout::V1 => {}
        }
    }

    let [u0, u1] = &rows.u;
    let [v0, v1] = &rows.v;
    // B2 and B3: each 2x2 block's average; the client's reverse filter recovers the even sample.
    let main = row_of(planes.main_uv, pair, width);
    for c in 0..half {
        let (l, r) = (2 * c, 2 * c + 1);
        main[2 * c] = average(u0[l], u0[r], u1[l], u1[r]);
        main[2 * c + 1] = average(v0[l], v0[r], v1[l], v1[r]);
    }
    let aux = row_of(planes.aux_uv, pair, width);
    match layout {
        // B6 to B9: every other even column of the odd row. NV12 interleaves the auxiliary U and
        // V planes, each U-then-V across its two quarters.
        Avc444Layout::V2 => {
            for c in 0..quarter {
                aux[2 * c] = u1[4 * c];
                aux[2 * c + 1] = u1[4 * c + 2];
                aux[2 * (c + quarter)] = v1[4 * c];
                aux[2 * (c + quarter) + 1] = v1[4 * c + 2];
            }
        }
        // B6 and B7: odd columns of the even row.
        Avc444Layout::V1 => {
            for c in 0..half {
                aux[2 * c] = u0[2 * c + 1];
                aux[2 * c + 1] = v0[2 * c + 1];
            }
        }
    }
}

/// Pixel alignment of H.264 macroblocks; the V1 auxiliary view is laid out per macroblock row.
const MACROBLOCK: usize = 16;

/// The forward transform of [MS-RDPEGFX] 3.3.8.3.1 ("Color Conversion").
///
/// [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0
fn rgb_to_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    (
        clamp((54 * r + 183 * g + 18 * b) >> 8),
        clamp(((-29 * r - 99 * g + 128 * b) >> 8) + 128),
        clamp(((128 * r - 116 * g - 12 * b) >> 8) + 128),
    )
}

fn average(a: u8, b: u8, c: u8, d: u8) -> u8 {
    clamp((i32::from(a) + i32::from(b) + i32::from(c) + i32::from(d) + 2) >> 2)
}

fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use ironrdp_egfx::avc444::Yuv444Frame;

    use super::*;

    /// Times one split of the largest virtual display; run by hand with `--release`.
    #[test]
    #[ignore]
    fn split_speed() {
        let (width, height) = (3072usize, 1920usize);
        let mut bgra = vec![77u8; width * height * 4];
        let mut planes_data = [
            vec![0u8; width * height],
            vec![0u8; width * height / 2],
            vec![0u8; width * height],
            vec![0u8; width * height / 2],
        ];
        let [main_y, main_uv, aux_y, aux_uv] = &mut planes_data;
        let planes = Planes {
            bgra: Plane {
                base: bgra.as_mut_ptr(),
                stride: width * 4,
            },
            main_y: Plane {
                base: main_y.as_mut_ptr(),
                stride: width,
            },
            main_uv: Plane {
                base: main_uv.as_mut_ptr(),
                stride: width,
            },
            aux_y: Plane {
                base: aux_y.as_mut_ptr(),
                stride: width,
            },
            aux_uv: Plane {
                base: aux_uv.as_mut_ptr(),
                stride: width,
            },
        };
        let started = std::time::Instant::now();
        for _ in 0..20 {
            // SAFETY: the vectors have exactly the sizes `convert` expects.
            unsafe { convert(&planes, width, height, Avc444Layout::V1) };
        }
        println!("split {:?} per frame", started.elapsed() / 20);
    }

    /// The views match IronRDP's reference split, which its client decoder inverts.
    #[test]
    fn views_match_the_reference_split() {
        for layout in [Avc444Layout::V1, Avc444Layout::V2] {
            views_match(layout);
        }
    }

    fn views_match(layout: Avc444Layout) {
        let (width, height) = (48usize, 32usize);
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        };
        let mut bgra: Vec<u8> = (0..width * height * 4).map(|_| next()).collect();
        let rgba: Vec<u8> = bgra
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();

        let mut main_y = vec![0u8; width * height];
        let mut main_uv = vec![0u8; width * height / 2];
        let mut aux_y = vec![0u8; width * height];
        let mut aux_uv = vec![0u8; width * height / 2];
        let plane = |data: &mut Vec<u8>, stride| Plane {
            base: data.as_mut_ptr(),
            stride,
        };
        let planes = Planes {
            bgra: plane(&mut bgra, width * 4),
            main_y: plane(&mut main_y, width),
            main_uv: plane(&mut main_uv, width),
            aux_y: plane(&mut aux_y, width),
            aux_uv: plane(&mut aux_uv, width),
        };
        // SAFETY: the vectors have exactly the sizes `convert` expects.
        unsafe { convert(&planes, width, height, layout) };

        let reference = Yuv444Frame::from_rgba(&rgba, width as u16, height as u16).unwrap();
        let (main, aux) = reference.split(layout).unwrap();
        for row in 0..height {
            assert_eq!(
                &main_y[row * width..][..width],
                &main.luma_row(row).unwrap()[..width],
                "main Y {row}"
            );
            assert_eq!(
                &aux_y[row * width..][..width],
                &aux.luma_row(row).unwrap()[..width],
                "aux Y {row}"
            );
        }
        for row in 0..height / 2 {
            for (nv12, view) in [(&main_uv, &main), (&aux_uv, &aux)] {
                let line = &nv12[row * width..][..width];
                let u: Vec<u8> = line.iter().step_by(2).copied().collect();
                let v: Vec<u8> = line.iter().skip(1).step_by(2).copied().collect();
                assert_eq!(u, &view.u_row(row).unwrap()[..width / 2], "U {row}");
                assert_eq!(v, &view.v_row(row).unwrap()[..width / 2], "V {row}");
            }
        }
    }
}
