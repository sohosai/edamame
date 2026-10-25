//! RTMP で受信した FLV の映像・音声情報を解析する

use shiguredo_rtmp::{
    AudioFormat, AudioFrame, AudioSampleRate, AvcSequenceHeader, VideoCodec, VideoFrame,
};

/// 受信ストリームの映像情報
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoInfo {
    pub codec: VideoCodec,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub profile: Option<u8>,
    pub level: Option<u8>,
}

/// 受信ストリームの音声情報
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInfo {
    pub codec: AudioFormat,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
}

/// 受信ストリームの情報
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamInfo {
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    /// CTS offset を持つ映像フレーム、または破棄可能フレームを観測したか
    pub has_cts_offset: bool,
    /// 直前のキーフレームからの間隔
    pub keyframe_interval_ms: Option<u64>,
    /// 受信した映像フレーム数（シーケンスヘッダーを除く）
    pub video_frames: u64,
}

impl VideoInfo {
    /// GUI に出す短い説明
    pub fn summary(&self) -> String {
        let codec = video_codec_name(self.codec);
        let size = match (self.width, self.height) {
            (Some(width), Some(height)) => format!("{width}x{height}"),
            _ => "解像度不明".to_owned(),
        };
        let mut text = format!("{codec} {size}");
        if let (Some(profile), Some(level)) = (
            self.profile.and_then(profile_name),
            self.level.and_then(level_text),
        ) {
            text.push_str(&format!(" {profile}@{level}"));
        }
        text
    }

    /// 期待値と違う点。問題がなければ None
    pub fn problem(&self) -> Option<String> {
        if self.codec != VideoCodec::Avc {
            return Some(format!(
                "H.264 ではありません（{}）",
                video_codec_name(self.codec)
            ));
        }
        match (self.width, self.height) {
            (Some(1920), Some(1080)) => None,
            (Some(width), Some(height)) => {
                Some(format!("1920x1080 ではありません（{width}x{height}）"))
            }
            _ => Some("解像度を解析できません".to_owned()),
        }
    }
}

impl AudioInfo {
    /// GUI に出す短い説明
    pub fn summary(&self) -> String {
        let codec = audio_codec_name(self.codec);
        let rate = self
            .sample_rate
            .map(format_sample_rate)
            .unwrap_or_else(|| "設定不明".to_owned());
        let channels = self
            .channels
            .map(format_channels)
            .unwrap_or_else(|| "チャンネル不明".to_owned());
        format!("{codec} {rate} {channels}")
    }

    /// 期待値と違う点。問題がなければ None
    pub fn problem(&self) -> Option<String> {
        if self.codec != AudioFormat::Aac {
            return Some(format!(
                "AAC ではありません（{}）",
                audio_codec_name(self.codec)
            ));
        }
        if self.sample_rate.is_none() {
            return Some("AAC の音声設定を解析できません".to_owned());
        }
        None
    }
}

impl StreamInfo {
    /// B フレームの可能性を表示する文言
    pub fn bframe_text(&self) -> &'static str {
        if self.video_frames == 0 {
            "計測中"
        } else if self.has_cts_offset {
            "CTS offset あり"
        } else {
            "なし"
        }
    }

    /// B フレームの可能性についての問題
    pub fn bframe_problem(&self) -> Option<&'static str> {
        (self.video_frames > 0 && self.has_cts_offset)
            .then_some("CTS offset があり、B フレームの可能性があります")
    }

    /// キーフレーム間隔を秒で表した文言
    pub fn keyframe_text(&self) -> Option<String> {
        self.keyframe_interval_ms
            .map(|millis| format!("{:.1} 秒", millis as f64 / 1000.0))
    }

    /// キーフレーム間隔についての問題
    pub fn keyframe_problem(&self) -> Option<String> {
        self.keyframe_interval_ms
            .filter(|millis| *millis > 2_500)
            .map(|millis| {
                format!(
                    "キーフレーム間隔が長すぎます（{:.1} 秒）",
                    millis as f64 / 1000.0
                )
            })
    }

    /// 映像と音声の両方がそろっているか
    pub fn is_ready(&self) -> bool {
        self.video.is_some() && self.audio.is_some()
    }

    /// 期待値と違う点の一覧
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(video) = &self.video
            && let Some(problem) = video.problem()
        {
            problems.push(problem);
        }
        if let Some(audio) = &self.audio
            && let Some(problem) = audio.problem()
        {
            problems.push(problem);
        }
        if let Some(problem) = self.bframe_problem() {
            problems.push(problem.to_owned());
        }
        if let Some(problem) = self.keyframe_problem() {
            problems.push(problem);
        }
        problems
    }
}

/// 映像シーケンスヘッダーから映像情報を取り出す
pub(crate) fn video_info(frame: &VideoFrame) -> VideoInfo {
    let (width, height, profile, level) = if frame.codec == VideoCodec::Avc {
        match AvcSequenceHeader::from_bytes(&frame.data) {
            Ok(header) => {
                let dimensions = header
                    .sps_list
                    .first()
                    .and_then(|sps| parse_sps_dimensions(sps));
                let (width, height) = match dimensions {
                    Some((width, height)) => (Some(width), Some(height)),
                    None => (None, None),
                };
                (
                    width,
                    height,
                    Some(header.avc_profile_indication),
                    Some(header.avc_level_indication),
                )
            }
            Err(_) => (None, None, None, None),
        }
    } else {
        (None, None, None, None)
    };
    VideoInfo {
        codec: frame.codec,
        width,
        height,
        profile,
        level,
    }
}

/// 音声シーケンスヘッダーから音声情報を取り出す
pub(crate) fn audio_info(frame: &AudioFrame) -> AudioInfo {
    let (sample_rate, channels) = if frame.format == AudioFormat::Aac {
        match parse_aac_config(&frame.data) {
            Some(config) => (Some(config.sample_rate), config.channels),
            None => (None, None),
        }
    } else {
        (
            Some(sample_rate_from_flv(frame.sample_rate)),
            Some(if frame.is_stereo { 2 } else { 1 }),
        )
    };
    AudioInfo {
        codec: frame.format,
        sample_rate,
        channels,
    }
}

/// SPS から符号化解像度を取り出す
fn parse_sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    let rbsp = remove_emulation_prevention(sps);
    let mut bits = BitReader::new(&rbsp);
    if rbsp.first().is_some_and(|byte| *byte & 0x1F == 7) {
        bits.read_bits(8)?;
    }
    let profile_idc = bits.read_bits(8)?;
    bits.read_bits(8)?; // constraint flags
    bits.read_bits(8)?; // level_idc
    bits.read_ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = bits.read_ue()?;
        if chroma_format_idc == 3 {
            bits.read_bits(1)?;
        }
        bits.read_ue()?; // bit_depth_luma_minus8
        bits.read_ue()?; // bit_depth_chroma_minus8
        bits.read_bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.read_bits(1)? == 1 {
            let count = if chroma_format_idc == 3 { 12 } else { 8 };
            for index in 0..count {
                if bits.read_bits(1)? == 1 {
                    let size = if index < 6 { 16 } else { 64 };
                    skip_scaling_list(&mut bits, size)?;
                }
            }
        }
    }

    bits.read_ue()?; // log2_max_frame_num_minus4
    match bits.read_ue()? {
        0 => {
            bits.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.read_bits(1)?; // delta_pic_order_always_zero_flag
            bits.read_se()?; // offset_for_non_ref_pic
            bits.read_se()?; // offset_for_top_to_bottom_field
            let cycle = bits.read_ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                bits.read_se()?;
            }
        }
        _ => {}
    }
    bits.read_ue()?; // max_num_ref_frames
    bits.read_bits(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_in_mbs = bits.read_ue()?.checked_add(1)?;
    let height_in_map_units = bits.read_ue()?.checked_add(1)?;
    let frame_mbs_only_flag = bits.read_bits(1)?;
    if frame_mbs_only_flag == 0 {
        bits.read_bits(1)?; // mb_adaptive_frame_field_flag
    }
    bits.read_bits(1)?; // direct_8x8_inference_flag
    let (crop_left, crop_right, crop_top, crop_bottom) = if bits.read_bits(1)? == 1 {
        (
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };

    let width = width_in_mbs.checked_mul(16)?;
    let height = (2 - frame_mbs_only_flag)
        .checked_mul(height_in_map_units)?
        .checked_mul(16)?;
    let (sub_width, sub_height) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        3 => (1, 1),
        _ => (1, 1),
    };
    let crop_width = crop_left.checked_add(crop_right)?.checked_mul(sub_width)?;
    let crop_height = crop_top
        .checked_add(crop_bottom)?
        .checked_mul(sub_height)?
        .checked_mul(2 - frame_mbs_only_flag)?;
    Some((
        width.checked_sub(crop_width)?,
        height.checked_sub(crop_height)?,
    ))
}

/// AAC の AudioSpecificConfig からサンプリングレートとチャンネル数を取り出す
fn parse_aac_config(data: &[u8]) -> Option<AacConfig> {
    let mut bits = BitReader::new(data);
    let audio_object_type = bits.read_bits(5)?;
    if audio_object_type == 31 {
        bits.read_bits(6)?;
    }
    let sample_rate = match bits.read_bits(4)? {
        0 => 96_000,
        1 => 88_200,
        2 => 64_000,
        3 => 48_000,
        4 => 44_100,
        5 => 32_000,
        6 => 24_000,
        7 => 22_050,
        8 => 16_000,
        9 => 12_000,
        10 => 11_025,
        11 => 8_000,
        12 => 7_350,
        15 => bits.read_bits(24)?,
        _ => return None,
    };
    let channel_configuration = bits.read_bits(4)?;
    let channels = match channel_configuration {
        1..=6 => Some(channel_configuration as u8),
        7 => Some(8),
        _ => None,
    };
    Some(AacConfig {
        sample_rate,
        channels,
    })
}

/// AAC の設定
#[derive(Debug, Clone, PartialEq, Eq)]
struct AacConfig {
    pub sample_rate: u32,
    pub channels: Option<u8>,
}

fn video_codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Jpeg => "JPEG",
        VideoCodec::H263 => "H.263",
        VideoCodec::ScreenVideo => "ScreenVideo",
        VideoCodec::Vp6 => "VP6",
        VideoCodec::Vp6WithAlpha => "VP6 (alpha)",
        VideoCodec::ScreenVideoV2 => "ScreenVideoV2",
        VideoCodec::Avc => "H.264",
    }
}

fn audio_codec_name(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Adpcm => "ADPCM",
        AudioFormat::Mp3 => "MP3",
        AudioFormat::LinearPcmLittleEndian => "PCM",
        AudioFormat::Nellymoser16khzMono => "Nellymoser 16 kHz",
        AudioFormat::Nellymoser8KhzMono => "Nellymoser 8 kHz",
        AudioFormat::Nellymoser => "Nellymoser",
        AudioFormat::G711AlawLogarithmicPcm => "G.711 A-law",
        AudioFormat::G711MuLawLogarithmicPcm => "G.711 mu-law",
        AudioFormat::Aac => "AAC",
        AudioFormat::Speex => "Speex",
        AudioFormat::Mp3_8khz => "MP3 8 kHz",
        AudioFormat::DeviceSpecificSound => "デバイス固有",
    }
}

fn profile_name(profile: u8) -> Option<&'static str> {
    match profile {
        66 => Some("Baseline"),
        77 => Some("Main"),
        88 => Some("Extended"),
        100 => Some("High"),
        110 => Some("High 10"),
        122 => Some("High 4:2:2"),
        244 => Some("High 4:4:4"),
        _ => None,
    }
}

fn level_text(level: u8) -> Option<String> {
    (level > 0).then(|| format!("{}.{}", level / 10, level % 10))
}

fn format_sample_rate(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}

fn format_channels(channels: u8) -> String {
    match channels {
        1 => "モノラル".to_owned(),
        2 => "ステレオ".to_owned(),
        channels => format!("{channels} ch"),
    }
}

fn sample_rate_from_flv(rate: AudioSampleRate) -> u32 {
    match rate {
        AudioSampleRate::Khz5 => 5_512,
        AudioSampleRate::Khz11 => 11_025,
        AudioSampleRate::Khz22 => 22_050,
        AudioSampleRate::Khz44 => 44_100,
    }
}

fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    let mut zero_count = 0;
    for &byte in data {
        if zero_count >= 2 && byte == 0x03 {
            zero_count = 0;
            continue;
        }
        output.push(byte);
        if byte == 0 {
            zero_count += 1;
        } else {
            zero_count = 0;
        }
    }
    output
}

fn skip_scaling_list(bits: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last_scale = 8_i32;
    let mut next_scale = 8_i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = bits.read_se()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Some(())
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read_bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0_u32;
        for _ in 0..count {
            let byte = *self.data.get(self.bit / 8)?;
            let shift = 7 - (self.bit % 8);
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.bit += 1;
        }
        Some(value)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0_u32;
        while self.read_bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let suffix = self.read_bits(zeros)?;
        Some(((1_u64 << zeros) - 1 + u64::from(suffix)) as u32)
    }

    fn read_se(&mut self) -> Option<i32> {
        let value = self.read_ue()?;
        Some(if value % 2 == 1 {
            value.div_ceil(2) as i32
        } else {
            -((value / 2) as i32)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shiguredo_rtmp::{AvcPacketType, RtmpTimestamp, RtmpTimestampDelta, VideoFrameType};

    /// ffmpeg で 1920x1080 を符号化したときの SPS
    const SPS_1920X1080: &[u8] = &[
        0x67, 0x42, 0xc0, 0x28, 0xda, 0x01, 0xe0, 0x08, 0x9f, 0x97, 0x01, 0x10, 0x00, 0x00, 0x03,
        0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc8, 0xf1, 0x83, 0x2a,
    ];

    /// ffmpeg で High profile の 1920x1080 を符号化したときの SPS
    const SPS_1920X1080_HIGH: &[u8] = &[
        0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
    ];

    /// ffmpeg で 1280x720 を符号化したときの SPS
    const SPS_1280X720: &[u8] = &[
        0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x40, 0x16, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00,
        0x40, 0x00, 0x00, 0x0f, 0x23, 0xc6, 0x0c, 0xa8,
    ];

    fn video_frame(data: Vec<u8>) -> VideoFrame {
        VideoFrame {
            timestamp: RtmpTimestamp::ZERO,
            composition_timestamp_offset: RtmpTimestampDelta::ZERO,
            frame_type: VideoFrameType::KeyFrame,
            codec: VideoCodec::Avc,
            avc_packet_type: Some(AvcPacketType::SequenceHeader),
            data,
        }
    }

    fn audio_frame(data: Vec<u8>) -> AudioFrame {
        AudioFrame {
            timestamp: RtmpTimestamp::ZERO,
            format: AudioFormat::Aac,
            sample_rate: AudioSampleRate::Khz44,
            is_8bit_sample: false,
            is_stereo: true,
            is_aac_sequence_header: true,
            data,
        }
    }

    #[test]
    fn parses_1080p_sps_with_cropping() {
        assert_eq!(parse_sps_dimensions(SPS_1920X1080), Some((1920, 1080)));
    }

    #[test]
    fn parses_720p_sps() {
        assert_eq!(parse_sps_dimensions(SPS_1280X720), Some((1280, 720)));
    }

    #[test]
    fn parses_high_profile_1080p_sps() {
        assert_eq!(parse_sps_dimensions(SPS_1920X1080_HIGH), Some((1920, 1080)));
    }

    #[test]
    fn parses_aac_48khz_stereo() {
        let config = parse_aac_config(&[0x11, 0x90]).expect("AAC の設定を解析できる");
        assert_eq!(config.sample_rate, 48_000);
        assert_eq!(config.channels, Some(2));
    }

    #[test]
    fn parses_aac_44khz_mono() {
        let config = parse_aac_config(&[0x12, 0x08]).expect("AAC の設定を解析できる");
        assert_eq!(config.sample_rate, 44_100);
        assert_eq!(config.channels, Some(1));
    }

    #[test]
    fn builds_video_info_from_sequence_header() {
        // VideoFrame.data は SPS 単体ではなく AVCDecoderConfigurationRecord 全体
        let mut record = vec![0x01, 0x42, 0xc0, 0x28, 0xff, 0xe1, 0x00, 0x19];
        record.extend_from_slice(SPS_1920X1080);
        record.extend_from_slice(&[0x01, 0x00, 0x04, 0x68, 0xce, 0x0f, 0xc8]);

        let info = video_info(&video_frame(record));
        assert_eq!(info.codec, VideoCodec::Avc);
        assert_eq!(info.width, Some(1920));
        assert_eq!(info.height, Some(1080));
        assert_eq!(info.profile, Some(66));
        assert_eq!(info.level, Some(40));
        assert!(info.problem().is_none());
    }

    #[test]
    fn builds_audio_info_from_sequence_header() {
        let info = audio_info(&audio_frame(vec![0x11, 0x90]));
        assert_eq!(info.codec, AudioFormat::Aac);
        assert_eq!(info.sample_rate, Some(48_000));
        assert_eq!(info.channels, Some(2));
        assert!(info.problem().is_none());
    }

    #[test]
    fn reports_unexpected_video_and_audio() {
        let video = VideoInfo {
            codec: VideoCodec::Avc,
            width: Some(1280),
            height: Some(720),
            profile: None,
            level: None,
        };
        assert!(video.problem().unwrap().contains("1920x1080"));

        let audio = AudioInfo {
            codec: AudioFormat::Mp3,
            sample_rate: Some(44_100),
            channels: Some(2),
        };
        assert!(audio.problem().unwrap().contains("AAC"));
    }

    #[test]
    fn reports_bframe_and_keyframe_problems() {
        let mut info = StreamInfo::default();
        assert_eq!(info.bframe_text(), "計測中");
        assert!(info.bframe_problem().is_none());

        info.video_frames = 1;
        info.has_cts_offset = true;
        info.keyframe_interval_ms = Some(3_000);
        assert_eq!(info.bframe_text(), "CTS offset あり");
        assert!(info.bframe_problem().is_some());
        assert!(info.keyframe_problem().is_some());
        assert_eq!(info.problems().len(), 2);
    }
}
