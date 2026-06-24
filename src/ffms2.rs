#![allow(non_camel_case_types, non_snake_case, dead_code)]

use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::io::Write;
use std::os::raw::{c_char, c_double, c_int, c_uint};
use std::path::Path;
use std::sync::Once;

use crate::ext::external_bin;

const FFMS_ERROR_BUFFER_SIZE: usize = 1024;
const FFMS_SEEK_NORMAL: c_int = 1; // FFMS2 5.0: enum shifted, 1 = SEEK_NORMAL (supports random access)
const FFMS_TYPE_VIDEO: c_int = 0;
const FFMS_RESIZER_BICUBIC: c_int  = 4;
const FFMS_RESIZER_LANCZOS: c_int  = 512;

// Must match FFMS2's FFMS_ErrorInfo exactly: ErrorType, SubType, BufferSize, Buffer
#[repr(C)]
struct RawErrorInfo {
    error_type: c_int,
    sub_type: c_int,
    buffer_size: c_int,
    buffer: *mut c_char,
}

#[repr(C)]
pub struct FFMS_VideoSource {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FFMS_Index {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FFMS_Frame {
    pub data: [*const u8; 4],
    pub linesize: [c_int; 4],
    pub encoded_width: c_int,
    pub encoded_height: c_int,
    pub encoded_pixel_format: c_int,
    pub scaled_width: c_int,
    pub scaled_height: c_int,
    pub converted_pixel_format: c_int,
    pub key_frame: c_int,
    pub repeat_pict: c_int,
    pub interlaced_frame: c_int,
    pub top_field_first: c_int,
    pub pict_type: c_char,
    pub color_space: c_int,
    pub color_range: c_int,
    pub color_primaries: c_int,
    pub transfer_characteristics: c_int,
    pub chroma_location: c_int,
    pub has_mastering_display_primaries: c_int,
    pub mastering_display_primaries_x: [c_double; 3],
    pub mastering_display_primaries_y: [c_double; 3],
    pub mastering_display_white_point_x: c_double,
    pub mastering_display_white_point_y: c_double,
    pub has_mastering_display_luminance: c_int,
    pub mastering_display_min_luminance: c_double,
    pub mastering_display_max_luminance: c_double,
    pub has_content_light_level: c_int,
    pub content_light_level_max: c_uint,
    pub content_light_level_average: c_uint,
    pub flip: c_int,
    pub dolby_vision_rpu: *mut u8,
    pub dolby_vision_rpu_size: c_int,
    pub hdr10_plus: *mut u8,
    pub hdr10_plus_size: c_int,
}

#[repr(C)]
pub struct FFMS_VideoProperties {
    pub fps_numerator: c_int,
    pub fps_denominator: c_int,
    pub rff_numerator: c_int,
    pub rff_denominator: c_int,
    pub num_frames: c_int,
    pub sar_num: c_int,
    pub sar_den: c_int,
    pub crop_top: c_int,
    pub crop_bottom: c_int,
    pub crop_left: c_int,
    pub crop_right: c_int,
    pub top_field_first: c_int,
    pub color_space: c_int,
    pub color_range: c_int,
    pub first_time: c_double,
    pub last_time: c_double,
    pub rotation: c_int,
    pub stereo3d_type: c_int,
    pub stereo3d_flags: c_int,
    pub last_end_time: c_double,
    pub has_mastering_display_primaries: c_int,
    pub mastering_display_primaries_x: [c_double; 3],
    pub mastering_display_primaries_y: [c_double; 3],
    pub mastering_display_white_point_x: c_double,
    pub mastering_display_white_point_y: c_double,
    pub has_mastering_display_luminance: c_int,
    pub mastering_display_min_luminance: c_double,
    pub mastering_display_max_luminance: c_double,
    pub has_content_light_level: c_int,
    pub content_light_level_max: c_uint,
    pub content_light_level_average: c_uint,
    pub flip: c_int,
}

unsafe extern "C" {
    fn FFMS_Init(reserved: c_int, deprecated: c_int);
    fn FFMS_GetPixFmt(name: *const c_char) -> c_int;
    fn FFMS_ReadIndex(index_file: *const c_char, error_info: *mut RawErrorInfo) -> *mut FFMS_Index;
    fn FFMS_DestroyIndex(index: *mut FFMS_Index);
    fn FFMS_GetFirstTrackOfType(
        index: *mut FFMS_Index,
        track_type: c_int,
        error_info: *mut RawErrorInfo,
    ) -> c_int;
    fn FFMS_CreateVideoSource(
        source_file: *const c_char,
        track: c_int,
        index: *mut FFMS_Index,
        threads: c_int,
        seek_mode: c_int,
        error_info: *mut RawErrorInfo,
    ) -> *mut FFMS_VideoSource;
    fn FFMS_DestroyVideoSource(v: *mut FFMS_VideoSource);
    fn FFMS_GetVideoProperties(v: *mut FFMS_VideoSource) -> *const FFMS_VideoProperties;
    fn FFMS_SetOutputFormatV2(
        v: *mut FFMS_VideoSource,
        target_formats: *const c_int,
        width: c_int,
        height: c_int,
        resizer: c_int,
        error_info: *mut RawErrorInfo,
    ) -> c_int;
    fn FFMS_GetFrame(
        v: *mut FFMS_VideoSource,
        n: c_int,
        error_info: *mut RawErrorInfo,
    ) -> *const FFMS_Frame;
}

struct ErrorInfo {
    _buf: Box<[c_char; FFMS_ERROR_BUFFER_SIZE]>,
    pub raw: RawErrorInfo,
}

impl ErrorInfo {
    fn new() -> Self {
        let mut buf = Box::new([0; FFMS_ERROR_BUFFER_SIZE]);
        let raw = RawErrorInfo {
            error_type: 0,
            sub_type: 0,
            buffer_size: FFMS_ERROR_BUFFER_SIZE as c_int,
            buffer: buf.as_mut_ptr(),
        };
        ErrorInfo { _buf: buf, raw }
    }

    fn message(&self) -> String {
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(self._buf.as_ptr() as *const u8, FFMS_ERROR_BUFFER_SIZE)
        };
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).to_string()
    }
}

pub fn get_pixel_format(name: &str) -> c_int {
    let cname = CString::new(name).unwrap();
    unsafe { FFMS_GetPixFmt(cname.as_ptr()) }
}

/// Crop region in crop=W:H:X:Y order (ffmpeg convention).
/// Applied in the Y4M pipe so the encoder only sees the cropped frame.
#[derive(Debug, Clone, Copy)]
pub struct Crop {
    pub w: u32,
    pub h: u32,
    pub x: u32,
    pub y: u32,
}

impl Crop {
    /// Parse from "crop=W:H:X:Y" or "W:H:X:Y". Requires exactly 4 parts.
    pub fn from_str(s: &str) -> Option<Self> {
        let s = s.trim_start_matches("crop=");
        let p: Vec<&str> = s.split(':').collect();
        if p.len() != 4 {
            return None;
        }
        Some(Crop {
            w: p[0].parse().ok()?,
            h: p[1].parse().ok()?,
            x: p[2].parse().ok()?,
            y: p[3].parse().ok()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelSubsampling {
    Yuv420,
    Yuv422,
    Yuv444,
}

#[derive(Debug, Clone, Copy)]
pub struct PixelFormat {
    pub pix_fmt: c_int,
    pub bit_depth: u32,
    pub subsampling: PixelSubsampling,
}

impl PixelFormat {
    pub fn y4m_colorspace(&self) -> String {
        match (self.subsampling, self.bit_depth) {
            (PixelSubsampling::Yuv420, 8) => "420".into(),
            (PixelSubsampling::Yuv420, 10) => "420p10".into(),
            (PixelSubsampling::Yuv420, 12) => "420p12".into(),
            (PixelSubsampling::Yuv422, 8) => "422".into(),
            (PixelSubsampling::Yuv422, 10) => "422p10".into(),
            (PixelSubsampling::Yuv422, 12) => "422p12".into(),
            (PixelSubsampling::Yuv444, 8) => "444".into(),
            (PixelSubsampling::Yuv444, 10) => "444p10".into(),
            (PixelSubsampling::Yuv444, 12) => "444p12".into(),
            (sub, depth) => {
                tracing::warn!(
                    "unsupported Y4M format ({sub:?}, {depth}-bit) - falling back to 420 8-bit"
                );
                "420".into()
            }
        }
    }

    pub fn bytes_per_sample(&self) -> usize {
        if self.bit_depth > 8 { 2 } else { 1 }
    }
}

fn pixfmt_for(sub: PixelSubsampling, depth: u8) -> Option<c_int> {
    use PixelSubsampling::*;
    let name = match (sub, depth) {
        (Yuv420,  8) => "yuv420p",
        (Yuv420, 10) => "yuv420p10le",
        (Yuv422,  8) => "yuv422p",
        (Yuv422, 10) => "yuv422p10le",
        (Yuv444,  8) => "yuv444p",
        (Yuv444, 10) => "yuv444p10le",
        _ => return None,
    };
    Some(get_pixel_format(name))
}

fn detect_pixel_format(pix_fmt: c_int) -> PixelFormat {
    use PixelSubsampling::*;
    const TABLE: &[(&str, u32, PixelSubsampling)] = &[
        ("yuv420p",      8, Yuv420),
        ("yuvj420p",     8, Yuv420),
        ("yuv420p10le", 10, Yuv420),
        ("yuv420p12le", 12, Yuv420),
        ("yuv420p16le", 16, Yuv420),
        ("yuv422p",      8, Yuv422),
        ("yuv422p10le", 10, Yuv422),
        ("yuv422p12le", 12, Yuv422),
        ("yuv444p",      8, Yuv444),
        ("yuv444p10le", 10, Yuv444),
        ("yuv444p12le", 12, Yuv444),
    ];

    for &(name, bit_depth, subsampling) in TABLE {
        if pix_fmt == get_pixel_format(name) {
            return PixelFormat { pix_fmt, bit_depth, subsampling };
        }
    }

    tracing::warn!("unrecognized FFMS pixel format {pix_fmt} - falling back to yuv420p 8-bit");
    PixelFormat {
        pix_fmt: get_pixel_format("yuv420p"),
        bit_depth: 8,
        subsampling: PixelSubsampling::Yuv420,
    }
}

struct Index(*mut FFMS_Index);

impl Drop for Index {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { FFMS_DestroyIndex(self.0) }
        }
    }
}

static FFMS_INIT: Once = Once::new();

impl Index {
    fn read(index_path: &Path) -> Result<Self> {
        FFMS_INIT.call_once(|| unsafe { FFMS_Init(0, 0) });
        let path_str = index_path.to_str()
            .with_context(|| format!("non-UTF8 index path: {}", index_path.display()))?;
        let cpath = CString::new(path_str)
            .with_context(|| format!("NUL byte in index path: {}", index_path.display()))?;
        let mut ei = ErrorInfo::new();
        let ptr = unsafe { FFMS_ReadIndex(cpath.as_ptr(), &mut ei.raw) };
        if ptr.is_null() {
            bail!("FFMS_ReadIndex failed: {}", ei.message());
        }
        Ok(Index(ptr))
    }

    fn first_video_track(&mut self) -> Result<c_int> {
        let mut ei = ErrorInfo::new();
        let track = unsafe { FFMS_GetFirstTrackOfType(self.0, FFMS_TYPE_VIDEO, &mut ei.raw) };
        if track < 0 {
            bail!("no video track found: {}", ei.message());
        }
        Ok(track)
    }
}

#[derive(Clone)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub sar_num: u32,
    pub sar_den: u32,
    pub num_frames: u32,
    pub pixel_format: PixelFormat,
}

pub struct VideoSource {
    ptr: *mut FFMS_VideoSource,
    pub info: VideoInfo,
}

// FFMS2 is not thread-safe; each worker owns its own VideoSource on a spawn_blocking thread.
unsafe impl Send for VideoSource {}

impl Drop for VideoSource {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { FFMS_DestroyVideoSource(self.ptr) }
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct OpenOpts {
    /// Force input bit depth (8 or 10); None = match source.
    pub target_bit_depth: Option<u8>,
}

impl VideoSource {
    pub fn open(source_file: &Path, index_file: &Path, opts: OpenOpts) -> Result<Self> {
        Self::open_inner(source_file, index_file, opts)
    }

    fn open_inner(
        source_file: &Path,
        index_file: &Path,
        opts: OpenOpts,
    ) -> Result<Self> {
        let mut idx = Index::read(index_file)?;
        let track = idx.first_video_track()?;

        let src_str = source_file.to_str()
            .with_context(|| format!("non-UTF8 source path: {}", source_file.display()))?;
        let csrc = CString::new(src_str)
            .with_context(|| format!("NUL byte in source path: {}", source_file.display()))?;
        let mut ei = ErrorInfo::new();
        let ptr = unsafe {
            FFMS_CreateVideoSource(csrc.as_ptr(), track, idx.0, 1, FFMS_SEEK_NORMAL, &mut ei.raw)
        };
        if ptr.is_null() {
            bail!("FFMS_CreateVideoSource failed: {}", ei.message());
        }

        let props = unsafe { &*FFMS_GetVideoProperties(ptr) };

        let mut ei2 = ErrorInfo::new();
        let first_frame = unsafe { FFMS_GetFrame(ptr, 0, &mut ei2.raw) };
        if first_frame.is_null() {
            unsafe { FFMS_DestroyVideoSource(ptr) }
            bail!("FFMS_GetFrame(0) failed: {}", ei2.message());
        }
        let raw_pix_fmt = unsafe { (*first_frame).encoded_pixel_format };
        let mut pixel_format = detect_pixel_format(raw_pix_fmt);

        // Native resolution only; scaling happens later in the encode pipe (crop before scale).
        let out_w = unsafe { (*first_frame).encoded_width };
        let out_h = unsafe { (*first_frame).encoded_height };
        let resizer = FFMS_RESIZER_BICUBIC;

        // SVT-AV1 accepts only 8/10-bit input. Honor an explicit override, otherwise
        // clamp sources deeper than 10-bit (12/16) down to 10-bit so they still encode.
        let target_depth = opts.target_bit_depth
            .or((pixel_format.bit_depth > 10).then_some(10u8));

        if let Some(depth) = target_depth
            && depth as u32 != pixel_format.bit_depth
        {
            match pixfmt_for(pixel_format.subsampling, depth) {
                Some(pf) => {
                    tracing::info!(
                        "bit-depth conversion: {}-bit to {}-bit",
                        pixel_format.bit_depth, depth
                    );
                    pixel_format.pix_fmt = pf;
                    pixel_format.bit_depth = depth as u32;
                }
                None => {
                    unsafe { FFMS_DestroyVideoSource(ptr) }
                    bail!(
                        "no pixfmt available for {:?} at {}-bit",
                        pixel_format.subsampling, depth
                    );
                }
            }
        }

        let target_formats = [pixel_format.pix_fmt, -1i32];
        let mut ei3 = ErrorInfo::new();
        let rc = unsafe {
            FFMS_SetOutputFormatV2(
                ptr,
                target_formats.as_ptr(),
                out_w,
                out_h,
                resizer,
                &mut ei3.raw,
            )
        };
        if rc != 0 {
            unsafe { FFMS_DestroyVideoSource(ptr) }
            bail!("FFMS_SetOutputFormatV2 failed: {}", ei3.message());
        }

        let info = VideoInfo {
            width:      out_w as u32,
            height:     out_h as u32,
            fps_num:    props.fps_numerator as u32,
            fps_den:    props.fps_denominator as u32,
            sar_num:    props.sar_num.max(0) as u32,
            sar_den:    props.sar_den.max(1) as u32,
            num_frames: props.num_frames as u32,
            pixel_format,
        };

        Ok(VideoSource { ptr, info })
    }

    pub fn write_y4m_range<W: Write>(
        &mut self,
        writer: &mut W,
        start: u64,
        end: u64,
        crop: Option<Crop>,
    ) -> Result<()> {
        let info = &self.info;
        let cs = info.pixel_format.y4m_colorspace();
        let sar_str = if info.sar_num > 0 && info.sar_den > 0 {
            format!("{}:{}", info.sar_num, info.sar_den)
        } else {
            "0:0".to_string()
        };

        // Output dimensions: cropped if requested, full otherwise.
        // Crop offsets are rounded down to even for correct chroma alignment (YUV420/422).
        let (out_w, out_h, crop_x, crop_y) = match crop {
            Some(c) => (
                c.w as usize,
                c.h as usize,
                (c.x & !1) as usize,
                (c.y & !1) as usize,
            ),
            None => (info.width as usize, info.height as usize, 0, 0),
        };

        let header = format!(
            "YUV4MPEG2 W{out_w} H{out_h} F{}:{} Ip A{sar_str} C{cs}\n",
            info.fps_num, info.fps_den
        );
        writer.write_all(header.as_bytes()).context("write Y4M header")?;

        let bps = info.pixel_format.bytes_per_sample();

        let (chroma_out_w, chroma_out_h, chroma_cx, chroma_cy) =
            match info.pixel_format.subsampling {
                PixelSubsampling::Yuv420 => {
                    (out_w / 2, out_h / 2, crop_x / 2, crop_y / 2)
                }
                PixelSubsampling::Yuv422 => {
                    (out_w / 2, out_h, crop_x / 2, crop_y)
                }
                PixelSubsampling::Yuv444 => {
                    (out_w, out_h, crop_x, crop_y)
                }
            };

        let mut ei = ErrorInfo::new();
        for frame_n in start..=end {
            let frame = unsafe { FFMS_GetFrame(self.ptr, frame_n as c_int, &mut ei.raw) };
            if frame.is_null() {
                bail!("FFMS_GetFrame({frame_n}) failed: {}", ei.message());
            }

            writer.write_all(b"FRAME\n").context("write FRAME marker")?;

            let frame_ref = unsafe { &*frame };
            for plane_idx in 0..3usize {
                let plane_data = frame_ref.data[plane_idx];
                let linesize   = frame_ref.linesize[plane_idx];

                if plane_data.is_null() {
                    break;
                }

                let (plane_w, plane_h, px, py) = if plane_idx == 0 {
                    (out_w, out_h, crop_x, crop_y)
                } else {
                    (chroma_out_w, chroma_out_h, chroma_cx, chroma_cy)
                };

                let row_bytes = plane_w * bps;
                let stride    = linesize as usize;
                let col_off   = px * bps;

                for row in 0..plane_h {
                    let src_row = py + row;
                    let ptr = unsafe { plane_data.add(src_row * stride + col_off) };
                    let data = unsafe { std::slice::from_raw_parts(ptr, row_bytes) };
                    writer.write_all(data).context("write frame plane")?;
                }
            }
        }

        writer.flush().context("flush Y4M")
    }
}

pub async fn run_ffmsindex(source_file: &Path, index_file: &Path) -> Result<()> {
    let out = tokio::process::Command::new(external_bin("ffmsindex"))
        .arg("-f")
        .arg(source_file)
        .arg(index_file)
        .output()
        .await
        .context("start ffmsindex")?;

    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffmsindex failed:\n{stdout}{stderr}");
    }
    Ok(())
}
