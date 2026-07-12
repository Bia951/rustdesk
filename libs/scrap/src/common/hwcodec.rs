use crate::{
    codec::{base_bitrate, codec_thread_num, enable_hwcodec_option, EncoderApi, EncoderCfg},
    convert::*,
    CodecFormat, EncodeInput, ImageFormat, ImageRgb, Pixfmt, HW_STRIDE_ALIGN,
};
use hbb_common::{
    anyhow::{anyhow, bail, Context},
    bytes::Bytes,
    log,
    message_proto::{EncodedVideoFrame, EncodedVideoFrames, VideoFrame},
    serde_derive::{Deserialize, Serialize},
    serde_json, ResultType,
};
#[cfg(target_os = "linux")]
use hwcodec::ffmpeg_ram::encode::{
    DmabufFrame as HwcodecDmabufFrame, DmabufPlane as HwcodecDmabufPlane,
};
use hwcodec::{
    common::{
        DataFormat, HwcodecErrno,
        Quality::{self, *},
        RateControl::{self, *},
    },
    ffmpeg::AVPixelFormat,
    ffmpeg_ram::{
        decode::{DecodeContext, DecodeFrame, Decoder},
        encode::{EncodeContext, EncodeFrame, Encoder},
        ffmpeg_linesize_offset_length, CodecInfo,
    },
};
#[cfg(target_os = "linux")]
use std::{convert::TryFrom, os::fd::AsRawFd};

const DEFAULT_PIXFMT: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_NV12;
pub const DEFAULT_FPS: i32 = 30;
const DEFAULT_GOP: i32 = i32::MAX;
const DEFAULT_HW_QUALITY: Quality = Quality_Default;
pub const ERR_HEVC_POC: i32 = HwcodecErrno::HWCODEC_ERR_HEVC_COULD_NOT_FIND_POC as i32;
#[cfg(target_os = "linux")]
pub const HAS_AVFILTER: bool = hwcodec::ffmpeg_ram::HAS_AVFILTER;

crate::generate_call_macro!(call_yuv, false);

#[cfg(not(target_os = "android"))]
lazy_static::lazy_static! {
    static ref CONFIG: std::sync::Arc<std::sync::Mutex<Option<HwCodecConfig>>> = Default::default();
    static ref CONFIG_SET_BY_IPC: std::sync::Arc<std::sync::Mutex<bool>> = Default::default();
}

#[derive(Debug, Clone)]
pub struct HwRamEncoderConfig {
    pub name: String,
    pub mc_name: Option<String>,
    pub width: usize,
    pub height: usize,
    pub quality: f32,
    pub keyframe_interval: Option<usize>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct HwDmabufEncoderConfig {
    pub name: String,
    pub mc_name: Option<String>,
    pub device_path: Option<String>,
    pub width: usize,
    pub height: usize,
    pub quality: f32,
    pub keyframe_interval: Option<usize>,
}

pub struct HwRamEncoder {
    encoder: Encoder,
    pub format: DataFormat,
    pub pixfmt: AVPixelFormat,
    bitrate: u32, //kbs
    config: HwRamEncoderConfig,
}

#[cfg(target_os = "linux")]
pub struct HwDmabufEncoder {
    encoder: Encoder,
    pub format: DataFormat,
    pub pixfmt: AVPixelFormat,
    bitrate: u32,
    config: HwDmabufEncoderConfig,
}

impl EncoderApi for HwRamEncoder {
    fn new(cfg: EncoderCfg, _i444: bool) -> ResultType<Self>
    where
        Self: Sized,
    {
        match cfg {
            EncoderCfg::HWRAM(config) => {
                let rc = Self::rate_control(&config);
                let mut bitrate =
                    Self::bitrate(&config.name, config.width, config.height, config.quality);
                bitrate = Self::check_bitrate_range(&config, bitrate);
                let gop = config.keyframe_interval.unwrap_or(DEFAULT_GOP as _) as i32;
                let ctx = EncodeContext {
                    name: config.name.clone(),
                    mc_name: config.mc_name.clone(),
                    width: config.width as _,
                    height: config.height as _,
                    pixfmt: DEFAULT_PIXFMT,
                    align: HW_STRIDE_ALIGN as _,
                    kbs: bitrate as i32,
                    fps: DEFAULT_FPS,
                    gop,
                    quality: DEFAULT_HW_QUALITY,
                    rc,
                    q: -1,
                    thread_count: codec_thread_num(16) as _, // ffmpeg's thread_count is used for cpu
                };
                let format = match Encoder::format_from_name(config.name.clone()) {
                    Ok(format) => format,
                    Err(_) => {
                        return Err(anyhow!(format!(
                            "failed to get format from name:{}",
                            config.name
                        )))
                    }
                };
                match Encoder::new(ctx.clone()) {
                    Ok(encoder) => Ok(HwRamEncoder {
                        encoder,
                        format,
                        pixfmt: ctx.pixfmt,
                        bitrate,
                        config,
                    }),
                    Err(_) => Err(anyhow!(format!("Failed to create encoder"))),
                }
            }
            _ => Err(anyhow!("encoder type mismatch")),
        }
    }

    fn encode_to_message(&mut self, input: EncodeInput, ms: i64) -> ResultType<VideoFrame> {
        let mut vf = VideoFrame::new();
        let mut frames = Vec::new();
        for frame in self
            .encode(input.yuv()?, ms)
            .with_context(|| "Failed to encode")?
        {
            frames.push(EncodedVideoFrame {
                data: Bytes::from(frame.data),
                pts: frame.pts,
                key: frame.key == 1,
                ..Default::default()
            });
        }
        if frames.len() > 0 {
            let frames = EncodedVideoFrames {
                frames: frames.into(),
                ..Default::default()
            };
            match self.format {
                DataFormat::H264 => vf.set_h264s(frames),
                DataFormat::H265 => vf.set_h265s(frames),
                _ => bail!("unsupported format: {:?}", self.format),
            }
            Ok(vf)
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into())
        }
    }

    fn yuvfmt(&self) -> crate::EncodeYuvFormat {
        let pixfmt = if self.pixfmt == AVPixelFormat::AV_PIX_FMT_NV12 {
            Pixfmt::NV12
        } else {
            Pixfmt::I420
        };
        let stride = self
            .encoder
            .linesize
            .clone()
            .drain(..)
            .map(|i| i as usize)
            .collect();
        crate::EncodeYuvFormat {
            pixfmt,
            w: self.encoder.ctx.width as _,
            h: self.encoder.ctx.height as _,
            stride,
            u: self.encoder.offset[0] as _,
            v: if pixfmt == Pixfmt::NV12 {
                0
            } else {
                self.encoder.offset[1] as _
            },
        }
    }

    #[cfg(feature = "vram")]
    fn input_texture(&self) -> bool {
        false
    }

    fn set_quality(&mut self, ratio: f32) -> ResultType<()> {
        let mut bitrate = Self::bitrate(
            &self.config.name,
            self.config.width,
            self.config.height,
            ratio,
        );
        if bitrate > 0 {
            bitrate = Self::check_bitrate_range(&self.config, bitrate);
            self.encoder.set_bitrate(bitrate as _).ok();
            self.bitrate = bitrate;
        }
        self.config.quality = ratio;
        Ok(())
    }

    fn bitrate(&self) -> u32 {
        self.bitrate
    }

    fn support_changing_quality(&self) -> bool {
        ["vaapi"].iter().all(|&x| !self.config.name.contains(x))
    }

    fn latency_free(&self) -> bool {
        ["mediacodec", "videotoolbox"]
            .iter()
            .all(|&x| !self.config.name.contains(x))
    }

    fn is_hardware(&self) -> bool {
        true
    }

    fn disable(&self) {
        HwCodecConfig::clear(false, true);
    }
}

#[cfg(target_os = "linux")]
impl EncoderApi for HwDmabufEncoder {
    fn new(cfg: EncoderCfg, _i444: bool) -> ResultType<Self>
    where
        Self: Sized,
    {
        match cfg {
            EncoderCfg::HWDMABUF(config) => {
                let rc = HwRamEncoder::rate_control(&HwRamEncoderConfig {
                    name: config.name.clone(),
                    mc_name: config.mc_name.clone(),
                    width: config.width,
                    height: config.height,
                    quality: config.quality,
                    keyframe_interval: config.keyframe_interval,
                });
                let ram_config = HwRamEncoderConfig {
                    name: config.name.clone(),
                    mc_name: config.mc_name.clone(),
                    width: config.width,
                    height: config.height,
                    quality: config.quality,
                    keyframe_interval: config.keyframe_interval,
                };
                let mut bitrate = HwRamEncoder::bitrate(
                    &config.name,
                    config.width,
                    config.height,
                    config.quality,
                );
                bitrate = HwRamEncoder::check_bitrate_range(&ram_config, bitrate);
                let gop = config.keyframe_interval.unwrap_or(DEFAULT_GOP as _) as i32;
                let ctx = EncodeContext {
                    name: config.name.clone(),
                    mc_name: config.mc_name.clone(),
                    width: config.width as _,
                    height: config.height as _,
                    pixfmt: DEFAULT_PIXFMT,
                    align: HW_STRIDE_ALIGN as _,
                    kbs: bitrate as i32,
                    fps: DEFAULT_FPS,
                    gop,
                    quality: DEFAULT_HW_QUALITY,
                    rc,
                    q: -1,
                    thread_count: codec_thread_num(16) as _,
                };
                let format = match Encoder::format_from_name(config.name.clone()) {
                    Ok(format) => format,
                    Err(_) => {
                        return Err(anyhow!(format!(
                            "failed to get format from name:{}",
                            config.name
                        )))
                    }
                };
                match Encoder::new_with_device(ctx.clone(), config.device_path.as_deref()) {
                    Ok(encoder) => Ok(HwDmabufEncoder {
                        encoder,
                        format,
                        pixfmt: ctx.pixfmt,
                        bitrate,
                        config,
                    }),
                    Err(_) => Err(anyhow!(
                        "Failed to create dmabuf encoder: codec={}, size={}x{}",
                        config.name,
                        config.width,
                        config.height
                    )),
                }
            }
            _ => Err(anyhow!("encoder type mismatch")),
        }
    }

    fn encode_to_message(&mut self, input: EncodeInput, ms: i64) -> ResultType<VideoFrame> {
        let frame = input.dmabuf()?;
        let dmabuf = HwcodecDmabufFrame {
            width: frame.width,
            height: frame.height,
            encode_width: self.config.width,
            encode_height: self.config.height,
            fourcc: frame.fourcc,
            modifier: frame.modifier,
            planes: frame
                .planes
                .iter()
                .map(|plane| HwcodecDmabufPlane {
                    fd: plane.fd.as_raw_fd(),
                    stride: plane.stride,
                    offset: plane.offset,
                })
                .collect(),
        };

        let mut vf = VideoFrame::new();
        let mut frames = Vec::new();
        let mut encoded_frames = Vec::new();
        let encoded = match self.encoder.encode_dmabuf(&dmabuf, ms) {
            Ok(encoded) => {
                crate::note_linux_kms_dmabuf_encode_result(true);
                encoded
            }
            Err(err) => {
                // Don't nuke the dmabuf path on a single failure; only give up
                // after enough consecutive failures (tracked process-globally,
                // since the caller recreates the encoder on each failure).
                crate::note_linux_kms_dmabuf_encode_result(false);
                bail!("Failed to encode dmabuf: {err}");
            }
        };
        encoded_frames.append(encoded);
        for frame in encoded_frames {
            frames.push(EncodedVideoFrame {
                data: Bytes::from(frame.data),
                pts: frame.pts,
                key: frame.key == 1,
                ..Default::default()
            });
        }
        if frames.len() > 0 {
            let frames = EncodedVideoFrames {
                frames: frames.into(),
                ..Default::default()
            };
            match self.format {
                DataFormat::H264 => vf.set_h264s(frames),
                DataFormat::H265 => vf.set_h265s(frames),
                _ => bail!("unsupported format: {:?}", self.format),
            }
            Ok(vf)
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into())
        }
    }

    fn yuvfmt(&self) -> crate::EncodeYuvFormat {
        let pixfmt = if self.pixfmt == AVPixelFormat::AV_PIX_FMT_NV12 {
            Pixfmt::NV12
        } else {
            Pixfmt::I420
        };
        let stride = self
            .encoder
            .linesize
            .clone()
            .drain(..)
            .map(|i| i as usize)
            .collect();
        crate::EncodeYuvFormat {
            pixfmt,
            w: self.encoder.ctx.width as _,
            h: self.encoder.ctx.height as _,
            stride,
            u: self.encoder.offset[0] as _,
            v: if pixfmt == Pixfmt::NV12 {
                0
            } else {
                self.encoder.offset[1] as _
            },
        }
    }

    #[cfg(feature = "vram")]
    fn input_texture(&self) -> bool {
        false
    }

    fn set_quality(&mut self, ratio: f32) -> ResultType<()> {
        let ram_config = HwRamEncoderConfig {
            name: self.config.name.clone(),
            mc_name: self.config.mc_name.clone(),
            width: self.config.width,
            height: self.config.height,
            quality: self.config.quality,
            keyframe_interval: self.config.keyframe_interval,
        };
        let mut bitrate = HwRamEncoder::bitrate(
            &self.config.name,
            self.config.width,
            self.config.height,
            ratio,
        );
        if bitrate > 0 {
            bitrate = HwRamEncoder::check_bitrate_range(&ram_config, bitrate);
            self.encoder.set_bitrate(bitrate as _).ok();
            self.bitrate = bitrate;
        }
        self.config.quality = ratio;
        Ok(())
    }

    fn bitrate(&self) -> u32 {
        self.bitrate
    }

    fn support_changing_quality(&self) -> bool {
        ["vaapi"].iter().all(|&x| !self.config.name.contains(x))
    }

    fn latency_free(&self) -> bool {
        ["mediacodec", "videotoolbox", "vaapi"]
            .iter()
            .all(|&x| !self.config.name.contains(x))
    }

    fn is_hardware(&self) -> bool {
        true
    }

    fn is_dmabuf(&self) -> bool {
        true
    }

    fn download_dmabuf_rgba(
        &mut self,
        frame: &crate::DmabufFrame,
    ) -> ResultType<(usize, usize, Vec<u8>)> {
        self.download_rgba(frame)
    }

    fn disable(&self) {
        crate::set_linux_kms_dmabuf_capture_disabled_for_process(true);
    }
}

#[cfg(target_os = "linux")]
impl HwDmabufEncoder {
    pub fn download_rgba(
        &mut self,
        frame: &crate::DmabufFrame,
    ) -> ResultType<(usize, usize, Vec<u8>)> {
        let dmabuf = HwcodecDmabufFrame {
            width: frame.width,
            height: frame.height,
            encode_width: self.config.width,
            encode_height: self.config.height,
            fourcc: frame.fourcc,
            modifier: frame.modifier,
            planes: frame
                .planes
                .iter()
                .map(|plane| HwcodecDmabufPlane {
                    fd: plane.fd.as_raw_fd(),
                    stride: plane.stride,
                    offset: plane.offset,
                })
                .collect(),
        };
        let downloaded = self
            .encoder
            .download_dmabuf(&dmabuf)
            .map_err(|err| anyhow!("Failed to download dmabuf: {err}"))?;
        downloaded_nv12_to_rgba(&downloaded)
    }
}

#[cfg(target_os = "linux")]
fn downloaded_nv12_to_rgba(
    downloaded: &DecodeFrame,
) -> ResultType<(usize, usize, Vec<u8>)> {
    if downloaded.pixfmt != AVPixelFormat::AV_PIX_FMT_NV12
        || downloaded.width <= 0
        || downloaded.height <= 0
        || downloaded.data.len() < 2
        || downloaded.linesize.len() < 2
    {
        bail!(
            "invalid downloaded dmabuf frame: format={:?}, size={}x{}, planes={}",
            downloaded.pixfmt,
            downloaded.width,
            downloaded.height,
            downloaded.data.len()
        );
    }
    let width = downloaded.width as usize;
    let height = downloaded.height as usize;
    let y_stride = usize::try_from(downloaded.linesize[0])
        .map_err(|_| anyhow!("invalid downloaded dmabuf Y stride"))?;
    let uv_stride = usize::try_from(downloaded.linesize[1])
        .map_err(|_| anyhow!("invalid downloaded dmabuf UV stride"))?;
    let y_len = y_stride
        .checked_mul(height)
        .ok_or_else(|| anyhow!("downloaded dmabuf Y plane size overflow"))?;
    let uv_len = uv_stride
        .checked_mul((height + 1) / 2)
        .ok_or_else(|| anyhow!("downloaded dmabuf UV plane size overflow"))?;
    if y_stride < width
        || uv_stride < width
        || downloaded.data[0].len() < y_len
        || downloaded.data[1].len() < uv_len
    {
        bail!("downloaded dmabuf plane is smaller than its declared layout");
    }
    let rgba_stride = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("downloaded dmabuf RGBA stride overflow"))?;
    let rgba_len = rgba_stride
        .checked_mul(height)
        .ok_or_else(|| anyhow!("downloaded dmabuf RGBA size overflow"))?;
    let mut rgba = vec![0_u8; rgba_len];
    call_yuv!(NV12ToABGR(
        downloaded.data[0].as_ptr(),
        downloaded.linesize[0],
        downloaded.data[1].as_ptr(),
        downloaded.linesize[1],
        rgba.as_mut_ptr(),
        i32::try_from(rgba_stride)
            .map_err(|_| anyhow!("downloaded dmabuf RGBA stride overflow"))?,
        downloaded.width,
        downloaded.height,
    ));
    Ok((width, height, rgba))
}

#[cfg(all(test, target_os = "linux"))]
mod dmabuf_download_tests {
    use super::*;

    #[test]
    fn converts_downloaded_nv12_to_rgba() {
        let downloaded = DecodeFrame {
            pixfmt: AVPixelFormat::AV_PIX_FMT_NV12,
            width: 2,
            height: 2,
            data: vec![vec![16; 4], vec![128; 2]],
            linesize: vec![2, 2],
            key: false,
        };

        let (width, height, rgba) = downloaded_nv12_to_rgba(&downloaded).unwrap();
        assert_eq!((width, height), (2, 2));
        assert_eq!(rgba.len(), 16);
        assert!(rgba.chunks_exact(4).all(|pixel| pixel[3] == 0xff));
    }
}

impl HwRamEncoder {
    pub fn try_get(format: CodecFormat) -> Option<CodecInfo> {
        let mut info = None;
        let best = CodecInfo::prioritized(HwCodecConfig::get().ram_encode);
        match format {
            CodecFormat::H264 => {
                if let Some(v) = best.h264 {
                    info = Some(v);
                }
            }
            CodecFormat::H265 => {
                if let Some(v) = best.h265 {
                    info = Some(v);
                }
            }
            _ => {}
        }
        info
    }

    #[cfg(target_os = "linux")]
    pub fn try_get_vaapi(format: CodecFormat) -> Option<CodecInfo> {
        let format = match format {
            CodecFormat::H264 => DataFormat::H264,
            CodecFormat::H265 => DataFormat::H265,
            _ => return None,
        };
        HwCodecConfig::get()
            .ram_encode
            .into_iter()
            .filter(|codec| codec.format == format && codec.name.contains("vaapi"))
            .min_by_key(|codec| codec.priority)
    }

    pub fn encode(&mut self, yuv: &[u8], ms: i64) -> ResultType<Vec<EncodeFrame>> {
        match self.encoder.encode(yuv, ms) {
            Ok(v) => {
                let mut data = Vec::<EncodeFrame>::new();
                data.append(v);
                Ok(data)
            }
            Err(err) => Err(anyhow!("hardware encoder failed: {err}")),
        }
    }

    fn rate_control(_config: &HwRamEncoderConfig) -> RateControl {
        #[cfg(target_os = "android")]
        if _config.name.contains("mediacodec") {
            return RC_VBR;
        }
        RC_CBR
    }

    pub fn bitrate(name: &str, width: usize, height: usize, ratio: f32) -> u32 {
        Self::calc_bitrate(width, height, ratio, name.contains("h264"))
    }

    pub fn calc_bitrate(width: usize, height: usize, ratio: f32, h264: bool) -> u32 {
        let base = base_bitrate(width as _, height as _) as f32 * ratio;
        let threshold = 2000.0;
        let decay_rate = 0.001; // 1000 * 0.001 = 1
        let factor: f32 = if cfg!(target_os = "android") {
            // https://stackoverflow.com/questions/26110337/what-are-valid-bit-rates-to-set-for-mediacodec?rq=3
            if base > threshold {
                1.0 + 4.0 / (1.0 + (base - threshold) * decay_rate)
            } else {
                5.0
            }
        } else if h264 {
            if base > threshold {
                1.0 + 1.0 / (1.0 + (base - threshold) * decay_rate)
            } else {
                2.0
            }
        } else {
            if base > threshold {
                1.0 + 0.5 / (1.0 + (base - threshold) * decay_rate)
            } else {
                1.5
            }
        };
        (base * factor) as u32
    }

    pub fn check_bitrate_range(_config: &HwRamEncoderConfig, bitrate: u32) -> u32 {
        #[cfg(target_os = "android")]
        if _config.name.contains("mediacodec") {
            let info = crate::android::ffi::get_codec_info();
            if let Some(info) = info {
                if let Some(codec) = info
                    .codecs
                    .iter()
                    .find(|c| Some(c.name.clone()) == _config.mc_name && c.is_encoder)
                {
                    if codec.max_bitrate > codec.min_bitrate {
                        if bitrate > codec.max_bitrate {
                            return codec.max_bitrate;
                        }
                        if bitrate < codec.min_bitrate {
                            return codec.min_bitrate;
                        }
                    }
                }
            }
        }
        bitrate
    }
}

pub struct HwRamDecoder {
    decoder: Decoder,
    pub info: CodecInfo,
}

impl HwRamDecoder {
    pub fn try_get(format: CodecFormat) -> Option<CodecInfo> {
        let mut info = None;
        let soft = CodecInfo::soft();
        match format {
            CodecFormat::H264 => {
                if let Some(v) = soft.h264 {
                    info = Some(v);
                }
            }
            CodecFormat::H265 => {
                if let Some(v) = soft.h265 {
                    info = Some(v);
                }
            }
            _ => {}
        }
        if enable_hwcodec_option() {
            let best = CodecInfo::prioritized(HwCodecConfig::get().ram_decode);
            match format {
                CodecFormat::H264 => {
                    if let Some(v) = best.h264 {
                        info = Some(v);
                    }
                }
                CodecFormat::H265 => {
                    if let Some(v) = best.h265 {
                        info = Some(v);
                    }
                }
                _ => {}
            }
        }
        info
    }

    pub fn new(format: CodecFormat) -> ResultType<Self> {
        let info = HwRamDecoder::try_get(format);
        log::info!("try create {info:?} ram decoder");
        let Some(info) = info else {
            bail!("unsupported format: {:?}", format);
        };
        let ctx = DecodeContext {
            name: info.name.clone(),
            device_type: info.hwdevice.clone(),
            thread_count: codec_thread_num(16) as _,
        };
        match Decoder::new(ctx) {
            Ok(decoder) => Ok(HwRamDecoder { decoder, info }),
            Err(_) => {
                HwCodecConfig::clear(false, false);
                Err(anyhow!(format!("Failed to create decoder")))
            }
        }
    }
    pub fn decode<'a>(&'a mut self, data: &[u8]) -> ResultType<Vec<HwRamDecoderImage<'a>>> {
        match self.decoder.decode(data) {
            Ok(v) => Ok(v.iter().map(|f| HwRamDecoderImage { frame: f }).collect()),
            Err(e) => Err(anyhow!(e)),
        }
    }
}

pub struct HwRamDecoderImage<'a> {
    frame: &'a DecodeFrame,
}

impl HwRamDecoderImage<'_> {
    // rgb [in/out] fmt and stride must be set in ImageRgb
    pub fn to_fmt(&self, rgb: &mut ImageRgb, i420: &mut Vec<u8>) -> ResultType<()> {
        let frame = self.frame;
        let width = frame.width;
        let height = frame.height;
        rgb.w = width as _;
        rgb.h = height as _;
        let dst_align = rgb.align();
        let bytes_per_row = (rgb.w * 4 + dst_align - 1) & !(dst_align - 1);
        rgb.raw.resize(rgb.h * bytes_per_row, 0);
        match frame.pixfmt {
            AVPixelFormat::AV_PIX_FMT_NV12 => {
                // I420ToARGB is much faster than NV12ToARGB in tests on Windows
                if cfg!(windows) {
                    let Ok((linesize_i420, offset_i420, len_i420)) = ffmpeg_linesize_offset_length(
                        AVPixelFormat::AV_PIX_FMT_YUV420P,
                        width as _,
                        height as _,
                        HW_STRIDE_ALIGN,
                    ) else {
                        bail!("failed to get i420 linesize, offset, length");
                    };
                    i420.resize(len_i420 as _, 0);
                    let i420_offset_y = unsafe { i420.as_ptr().add(0) as _ };
                    let i420_offset_u = unsafe { i420.as_ptr().add(offset_i420[0] as _) as _ };
                    let i420_offset_v = unsafe { i420.as_ptr().add(offset_i420[1] as _) as _ };
                    call_yuv!(NV12ToI420(
                        frame.data[0].as_ptr(),
                        frame.linesize[0],
                        frame.data[1].as_ptr(),
                        frame.linesize[1],
                        i420_offset_y,
                        linesize_i420[0],
                        i420_offset_u,
                        linesize_i420[1],
                        i420_offset_v,
                        linesize_i420[2],
                        width,
                        height,
                    ));
                    let f = match rgb.fmt() {
                        ImageFormat::ARGB => I420ToARGB,
                        ImageFormat::ABGR => I420ToABGR,
                        _ => bail!("unsupported format: {:?} -> {:?}", frame.pixfmt, rgb.fmt()),
                    };
                    call_yuv!(f(
                        i420_offset_y,
                        linesize_i420[0],
                        i420_offset_u,
                        linesize_i420[1],
                        i420_offset_v,
                        linesize_i420[2],
                        rgb.raw.as_mut_ptr(),
                        bytes_per_row as _,
                        width,
                        height,
                    ));
                } else {
                    let f = match rgb.fmt() {
                        ImageFormat::ARGB => NV12ToARGB,
                        ImageFormat::ABGR => NV12ToABGR,
                        _ => bail!("unsupported format: {:?} -> {:?}", frame.pixfmt, rgb.fmt()),
                    };
                    call_yuv!(f(
                        frame.data[0].as_ptr(),
                        frame.linesize[0],
                        frame.data[1].as_ptr(),
                        frame.linesize[1],
                        rgb.raw.as_mut_ptr(),
                        bytes_per_row as _,
                        width,
                        height,
                    ));
                }
            }
            AVPixelFormat::AV_PIX_FMT_YUV420P => {
                let f = match rgb.fmt() {
                    ImageFormat::ARGB => I420ToARGB,
                    ImageFormat::ABGR => I420ToABGR,
                    _ => bail!("unsupported format: {:?} -> {:?}", frame.pixfmt, rgb.fmt()),
                };
                call_yuv!(f(
                    frame.data[0].as_ptr(),
                    frame.linesize[0],
                    frame.data[1].as_ptr(),
                    frame.linesize[1],
                    frame.data[2].as_ptr(),
                    frame.linesize[2],
                    rgb.raw.as_mut_ptr(),
                    bytes_per_row as _,
                    width,
                    height,
                ));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "android")]
fn get_mime_type(codec: DataFormat) -> &'static str {
    match codec {
        DataFormat::VP8 => "video/x-vnd.on2.vp8",
        DataFormat::VP9 => "video/x-vnd.on2.vp9",
        DataFormat::AV1 => "video/av01",
        DataFormat::H264 => "video/avc",
        DataFormat::H265 => "video/hevc",
    }
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct HwCodecConfig {
    #[serde(default)]
    pub signature: u64,
    #[serde(default)]
    pub ram_encode: Vec<CodecInfo>,
    #[serde(default)]
    pub ram_decode: Vec<CodecInfo>,
    #[cfg(feature = "vram")]
    #[serde(default)]
    pub vram_encode: Vec<hwcodec::vram::FeatureContext>,
    #[cfg(feature = "vram")]
    #[serde(default)]
    pub vram_decode: Vec<hwcodec::vram::DecodeContext>,
}

// HwCodecConfig2 is used to store the config in json format,
// confy can't serde HwCodecConfig successfully if the non-first struct Vec is empty due to old toml version.
// struct T { a: Vec<A>, b: Vec<String>} will fail if b is empty, but struct T { a: Vec<String>, b: Vec<String>} is ok.
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
struct HwCodecConfig2 {
    #[serde(default)]
    pub config: String,
}

// ipc server process start check process once, other process get from ipc server once
// install: --server start check process, check process send to --server,  ui get from --server
// portable: ui start check process, check process send to ui
// sciter and unilink: get from ipc server
impl HwCodecConfig {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn set(config: String) {
        let config = serde_json::from_str(&config).unwrap_or_default();
        log::info!("set hwcodec config");
        log::debug!("{config:?}");
        #[cfg(any(windows, target_os = "macos"))]
        hbb_common::config::common_store(
            &HwCodecConfig2 {
                config: serde_json::to_string_pretty(&config).unwrap_or_default(),
            },
            "_hwcodec",
        );
        *CONFIG.lock().unwrap() = Some(config);
        *CONFIG_SET_BY_IPC.lock().unwrap() = true;
    }

    pub fn get() -> HwCodecConfig {
        #[cfg(target_os = "android")]
        {
            let info = crate::android::ffi::get_codec_info();
            log::info!("all codec info: {info:?}");
            struct T {
                name_prefix: &'static str,
                data_format: DataFormat,
            }
            let ts = vec![
                T {
                    name_prefix: "h264",
                    data_format: DataFormat::H264,
                },
                T {
                    name_prefix: "hevc",
                    data_format: DataFormat::H265,
                },
            ];
            let mut e = vec![];
            if let Some(info) = info {
                ts.iter().for_each(|t| {
                    let codecs: Vec<_> = info
                        .codecs
                        .iter()
                        .filter(|c| {
                            c.is_encoder
                                && c.mime_type.as_str() == get_mime_type(t.data_format)
                                && c.nv12
                                && c.hw == Some(true) //only use hardware codec
                        })
                        .collect();
                    let screen_wh = std::cmp::max(info.w, info.h);
                    let mut best = None;
                    if let Some(codec) = codecs
                        .iter()
                        .find(|c| c.max_width >= screen_wh && c.max_height >= screen_wh)
                    {
                        best = Some(codec.name.clone());
                    } else {
                        // find the max resolution
                        let mut max_area = 0;
                        for codec in codecs.iter() {
                            if codec.max_width * codec.max_height > max_area {
                                best = Some(codec.name.clone());
                                max_area = codec.max_width * codec.max_height;
                            }
                        }
                    }
                    if let Some(best) = best {
                        e.push(CodecInfo {
                            name: format!("{}_mediacodec", t.name_prefix),
                            mc_name: Some(best),
                            format: t.data_format,
                            hwdevice: hwcodec::ffmpeg::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE,
                            priority: 0,
                        });
                    }
                });
            }
            log::debug!("e: {e:?}");
            HwCodecConfig {
                ram_encode: e,
                ..Default::default()
            }
        }
        #[cfg(any(windows, target_os = "macos"))]
        {
            let config = CONFIG.lock().unwrap().clone();
            match config {
                Some(c) => c,
                None => {
                    log::info!("try load cached hwcodec config");
                    let c = hbb_common::config::common_load::<HwCodecConfig2>("_hwcodec");
                    let c: HwCodecConfig = serde_json::from_str(&c.config).unwrap_or_default();
                    let new_signature = hwcodec::common::get_gpu_signature();
                    if c.signature == new_signature {
                        log::debug!("load cached hwcodec config: {c:?}");
                        *CONFIG.lock().unwrap() = Some(c.clone());
                        c
                    } else {
                        log::info!(
                            "gpu signature changed, {} -> {}",
                            c.signature,
                            new_signature
                        );
                        HwCodecConfig::default()
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            CONFIG.lock().unwrap().clone().unwrap_or_default()
        }
        #[cfg(target_os = "ios")]
        {
            HwCodecConfig::default()
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn get_set_value() -> Option<HwCodecConfig> {
        let set = CONFIG_SET_BY_IPC.lock().unwrap().clone();
        if set {
            CONFIG.lock().unwrap().clone()
        } else {
            None
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn already_set() -> bool {
        CONFIG_SET_BY_IPC.lock().unwrap().clone()
    }

    pub fn clear(vram: bool, encode: bool) {
        log::info!("clear hwcodec config, vram: {vram}, encode: {encode}");
        #[cfg(target_os = "android")]
        crate::android::ffi::clear_codec_info();
        #[cfg(not(target_os = "android"))]
        {
            let mut c = CONFIG.lock().unwrap();
            if let Some(c) = c.as_mut() {
                if vram {
                    #[cfg(feature = "vram")]
                    if encode {
                        c.vram_encode = vec![];
                    } else {
                        c.vram_decode = vec![];
                    }
                } else {
                    if encode {
                        c.ram_encode = vec![];
                    } else {
                        c.ram_decode = vec![];
                    }
                }
            }
        }
        crate::codec::Encoder::update(crate::codec::EncodingUpdate::Check);
    }
}

pub fn check_available_hwcodec() -> String {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    hwcodec::common::setup_parent_death_signal();
    let ctx = EncodeContext {
        name: String::from(""),
        mc_name: None,
        width: 1280,
        height: 720,
        pixfmt: DEFAULT_PIXFMT,
        align: HW_STRIDE_ALIGN as _,
        kbs: 1000,
        fps: DEFAULT_FPS,
        gop: DEFAULT_GOP,
        quality: DEFAULT_HW_QUALITY,
        rc: RC_CBR,
        q: -1,
        thread_count: 4,
    };
    #[cfg(feature = "vram")]
    let vram = crate::vram::check_available_vram();
    #[cfg(feature = "vram")]
    let vram_string = vram.2;
    #[cfg(not(feature = "vram"))]
    let vram_string = "".to_owned();
    let c = HwCodecConfig {
        ram_encode: Encoder::available_encoders(ctx, Some(vram_string)),
        ram_decode: Decoder::available_decoders(),
        #[cfg(feature = "vram")]
        vram_encode: vram.0,
        #[cfg(feature = "vram")]
        vram_decode: vram.1,
        signature: hwcodec::common::get_gpu_signature(),
    };
    log::debug!("{c:?}");
    serde_json::to_string(&c).unwrap_or_default()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn start_check_process() {
    if !enable_hwcodec_option() || HwCodecConfig::already_set() {
        return;
    }
    use hbb_common::allow_err;
    use std::sync::Once;
    let f = || {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(_) = exe.file_name().to_owned() {
                let arg = "--check-hwcodec-config";
                if let Ok(mut child) = std::process::Command::new(exe).arg(arg).spawn() {
                    #[cfg(windows)]
                    hwcodec::common::child_exit_when_parent_exit(child.id());
                    // wait up to 30 seconds, it maybe slow on windows startup for poorly performing machines
                    for _ in 0..30 {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        if let Ok(Some(_)) = child.try_wait() {
                            break;
                        }
                    }
                    allow_err!(child.kill());
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            log::info!("Check hwcodec config, exit with: {status}")
                        }
                        Ok(None) => {
                            log::info!(
                                "Check hwcodec config, status not ready yet, let's really wait"
                            );
                            let res = child.wait();
                            log::info!("Check hwcodec config, wait result: {res:?}");
                        }
                        Err(e) => {
                            log::error!("Check hwcodec config, error attempting to wait: {e}")
                        }
                    }
                }
            }
        };
    };
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(f);
    });
}
