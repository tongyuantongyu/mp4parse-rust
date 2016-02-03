//! Module for parsing ISO Base Media Format aka video/mp4 streams.

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
#![cfg_attr(feature = "fuzz", feature(plugin))]
#![cfg_attr(feature = "fuzz", plugin(afl_plugin))]
#[cfg(feature = "fuzz")]
extern crate afl;

extern crate byteorder;
use byteorder::ReadBytesExt;
use std::io::{Read, Take};
use std::cmp;
use std::fmt;

// Expose C api wrapper.
pub mod capi;
pub use capi::*;

// Unit tests.
#[cfg(test)]
mod tests;

// Arbitrary buffer size limit used for raw read_bufs on a box.
const BUF_SIZE_LIMIT: u64 = 1024 * 1024;

/// Describes parser failures.
///
/// This enum wraps athe standard `io::Error` type, unified with
/// our own parser error states and those of crates we use.
#[derive(Debug)]
pub enum Error {
    /// Parse error caused by corrupt or malformed data.
    InvalidData(&'static str),
    /// Parse error caused by limited parser support rather than invalid data.
    Unsupported(&'static str),
    /// Reflect `byteorder::Error::UnexpectedEOF` for short data.
    UnexpectedEOF,
    /// Caught panic! or assert! meaning the parser couldn't recover.
    AssertCaught,
    /// Propagate underlying errors from `std::io`.
    Io(std::io::Error),
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        Error::Io(err)
    }
}

impl From<byteorder::Error> for Error {
    fn from(err: byteorder::Error) -> Error {
        match err {
            byteorder::Error::UnexpectedEOF => Error::UnexpectedEOF,
            byteorder::Error::Io(e) => Error::Io(e),
        }
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(_: std::string::FromUtf8Error) -> Error {
        Error::InvalidData("invalid utf8")
    }
}

/// Result shorthand using our Error enum.
pub type Result<T> = std::result::Result<T, Error>;

/// Four-byte 'character code' describing the type of a piece of data.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct FourCC([u8; 4]);

impl FourCC {
    fn as_bytes(&self) -> &[u8; 4] {
        &self.0
    }
}

impl fmt::Debug for FourCC {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "'{}'", String::from_utf8_lossy(&self.0))
    }
}

/// Basic ISO box structure.
///
/// mp4 files are a sequence of possibly-nested 'box' structures.
/// Each box begins with a header describing the length of the
/// box's data and a four-byte 'character code' or `FourCC` which
/// identifies the type of the box. Together these are enough to
/// interpret the contents of that section of the file.
#[derive(Debug, Clone, Copy)]
pub struct BoxHeader {
    /// Four character box type.
    pub name: FourCC,
    /// Size of the box in bytes.
    pub size: u64,
    /// Offset to the start of the contained data (or header size).
    pub offset: u64,
}

/// File type box 'ftyp'.
#[derive(Debug)]
struct FileTypeBox {
    header: BoxHeader,
    major_brand: FourCC,
    minor_version: u32,
    compatible_brands: Vec<FourCC>,
}

/// Movie header box 'mvhd'.
#[derive(Debug)]
struct MovieHeaderBox {
    header: BoxHeader,
    timescale: u32,
    duration: u64,
}

/// Track header box 'tkhd'
#[derive(Debug, Clone)]
struct TrackHeaderBox {
    header: BoxHeader,
    track_id: u32,
    disabled: bool,
    duration: u64,
    width: u32,
    height: u32,
}

/// Edit list box 'elst'
#[derive(Debug)]
struct EditListBox {
    header: BoxHeader,
    edits: Vec<Edit>,
}

#[derive(Debug)]
struct Edit {
    segment_duration: u64,
    media_time: i64,
    media_rate_integer: i16,
    media_rate_fraction: i16,
}

/// Media header box 'mdhd'
#[derive(Debug)]
struct MediaHeaderBox {
    header: BoxHeader,
    timescale: u32,
    duration: u64,
}

// Chunk offset box 'stco' or 'co64'
#[derive(Debug)]
struct ChunkOffsetBox {
    header: BoxHeader,
    offsets: Vec<u64>,
}

// Sync sample box 'stss'
#[derive(Debug)]
struct SyncSampleBox {
    header: BoxHeader,
    samples: Vec<u32>,
}

// Sample to chunk box 'stsc'
#[derive(Debug)]
struct SampleToChunkBox {
    header: BoxHeader,
    samples: Vec<SampleToChunk>,
}

#[derive(Debug)]
struct SampleToChunk {
    first_chunk: u32,
    samples_per_chunk: u32,
    sample_description_index: u32,
}

// Sample size box 'stsz'
#[derive(Debug)]
struct SampleSizeBox {
    header: BoxHeader,
    sample_size: u32,
    sample_sizes: Vec<u32>,
}

// Time to sample box 'stts'
#[derive(Debug)]
struct TimeToSampleBox {
    header: BoxHeader,
    samples: Vec<Sample>,
}

#[derive(Debug)]
struct Sample {
    sample_count: u32,
    sample_delta: u32,
}

// Handler reference box 'hdlr'
#[derive(Debug)]
struct HandlerBox {
    header: BoxHeader,
    handler_type: FourCC,
}

// Sample description box 'stsd'
#[derive(Debug)]
struct SampleDescriptionBox {
    header: BoxHeader,
    descriptions: Vec<SampleEntry>,
}

#[derive(Debug, Clone)]
enum SampleEntry {
    Audio(AudioSampleEntry),
    Video(VideoSampleEntry),
    Unknown,
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone)]
enum AudioCodecSpecific {
    ES_Descriptor(Vec<u8>),
    OpusSpecificBox(OpusSpecificBox),
}

#[derive(Debug, Clone)]
struct AudioSampleEntry {
    data_reference_index: u16,
    channelcount: u16,
    samplesize: u16,
    samplerate: u32,
    codec_specific: AudioCodecSpecific,
}

#[derive(Debug, Clone)]
enum VideoCodecSpecific {
    AVCConfig(Vec<u8>),
    VPxConfig(VPxConfigBox),
}

#[derive(Debug, Clone)]
struct VideoSampleEntry {
    data_reference_index: u16,
    width: u16,
    height: u16,
    codec_specific: VideoCodecSpecific,
}

/// Represent a Video Partition Codec Configuration 'vpcC' box (aka vp9).
#[derive(Debug, Clone)]
struct VPxConfigBox {
    profile: u8,
    level: u8,
    bit_depth: u8,
    color_space: u8, // Really an enum
    chroma_subsampling: u8,
    transfer_function: u8,
    video_full_range: bool,
    codec_init: Vec<u8>, // Empty for vp8/vp9.
}

#[derive(Debug, Clone)]
struct ChannelMappingTable {
    stream_count: u8,
    coupled_count: u8,
    channel_mapping: Vec<u8>,
}

/// Represent an OpusSpecificBox 'dOps'
#[derive(Debug, Clone)]
struct OpusSpecificBox {
    version: u8,
    output_channel_count: u8,
    pre_skip: u16,
    input_sample_rate: u32,
    output_gain: i16,
    channel_mapping_family: u8,
    channel_mapping_table: Option<ChannelMappingTable>,
}

/// Internal data structures.
#[derive(Debug)]
pub struct MediaContext {
    timescale: Option<MediaTimeScale>,
    /// Tracks found in the file.
    tracks: Vec<Track>,
    /// Print boxes and other info as parsing proceeds. For debugging.
    trace: bool,
}

impl MediaContext {
    pub fn new() -> MediaContext {
        MediaContext {
            timescale: None,
            tracks: Vec::new(),
            trace: false,
        }
    }

    pub fn trace(&mut self, on: bool) {
        self.trace = on;
    }
}

macro_rules! log {
    ( $ctx:expr, $( $args:tt )* ) => {
        if $ctx.trace {
            println!( $( $args )* );
        }
    }
}

#[derive(Debug)]
enum TrackType {
    Audio,
    Video,
    Unknown,
}

/// The media's global (mvhd) timescale.
#[derive(Debug, Copy, Clone)]
struct MediaTimeScale(u64);

/// A time scaled by the media's global (mvhd) timescale.
#[derive(Debug, Copy, Clone)]
struct MediaScaledTime(u64);

/// The track's local (mdhd) timescale.
#[derive(Debug, Copy, Clone)]
struct TrackTimeScale(u64, usize);

/// A time scaled by the track's local (mdhd) timescale.
#[derive(Debug, Copy, Clone)]
struct TrackScaledTime(u64, usize);

#[derive(Debug)]
struct Track {
    id: usize,
    track_type: TrackType,
    empty_duration: Option<MediaScaledTime>,
    media_time: Option<TrackScaledTime>,
    timescale: Option<TrackTimeScale>,
    duration: Option<TrackScaledTime>,
    track_id: Option<u32>,
    mime_type: String,
    data: Option<SampleEntry>,
    tkhd: Option<TrackHeaderBox>, // TODO(kinetik): find a nicer way to export this.
    trace: bool,
}

impl Track {
    fn new(id: usize) -> Track {
        Track {
            id: id,
            track_type: TrackType::Unknown,
            empty_duration: None,
            media_time: None,
            timescale: None,
            duration: None,
            track_id: None,
            mime_type: String::new(),
            data: None,
            tkhd: None,
            trace: false,
        }
    }
}

struct BMFFBox<'a, T: 'a + Read> {
    head: BoxHeader,
    content: Take<&'a mut T>,
}

struct Input<'a, T: 'a + Read> {
    src: &'a mut T,
}

impl<'b, 'a, T: 'a + Read> Input<'b, T> {
    fn new(src: &'a mut T) -> Input<'a, T> {
        Input { src: src }
    }

    fn next(&'a mut self) -> Option<Result<BMFFBox<'a, T>>> {
        let r = read_box_header(self.src);
        match r {
            Ok(h) => Some(Ok(BMFFBox {
                head: h,
                content: limit(self.src, &h),
            })),
            Err(Error::UnexpectedEOF) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Read and parse a box header.
///
/// Call this first to determine the type of a particular mp4 box
/// and its length. Used internally for dispatching to specific
/// parsers for the internal content, or to get the length to
/// skip unknown or uninteresting boxes.
pub fn read_box_header<T: ReadBytesExt>(src: &mut T) -> Result<BoxHeader> {
    let size32 = try!(be_u32(src));
    let name = try!(be_fourcc(src));
    let size = match size32 {
        0 => return Err(Error::Unsupported("unknown sized box")),
        1 => {
            let size64 = try!(be_u64(src));
            if size64 < 16 {
                return Err(Error::InvalidData("malformed wide size"));
            }
            size64
        }
        2...7 => return Err(Error::InvalidData("malformed size")),
        _ => size32 as u64,
    };
    let offset = match size32 {
        1 => 4 + 4 + 8,
        _ => 4 + 4,
    };
    assert!(offset <= size);
    Ok(BoxHeader {
        name: name,
        size: size,
        offset: offset,
    })
}

/// Parse the extra header fields for a full box.
fn read_fullbox_extra<T: ReadBytesExt>(src: &mut T) -> Result<(u8, u32)> {
    let version = try!(src.read_u8());
    let flags_a = try!(src.read_u8());
    let flags_b = try!(src.read_u8());
    let flags_c = try!(src.read_u8());
    Ok((version,
        (flags_a as u32) << 16 | (flags_b as u32) << 8 | (flags_c as u32)))
}

/// Skip over the entire contents of a box.
fn skip_box_content<T: Read>(src: &mut T, header: &BoxHeader) -> Result<()> {
    skip(src, (header.size - header.offset) as usize)
}

/// Helper to construct a Take over the contents of a box.
fn limit<'a, T: Read>(f: &'a mut T, h: &BoxHeader) -> Take<&'a mut T> {
    f.take(h.size - h.offset)
}

macro_rules! check_parser_state {
    ( $ctx:expr, $src:expr ) => {
        if $src.limit() > 0 {
            log!($ctx, "bad parser state: {} content bytes left", $src.limit());
            return Err(Error::InvalidData("unread box content or bad parser sync"));
        }
    }
}

/// Read the contents of a box, including sub boxes.
///
/// Metadata is accumulated in the passed-through MediaContext struct,
/// which can be examined later.
pub fn read_mp4<T: Read>(f: &mut T, context: &mut MediaContext) -> Result<()> {
    let mut x = Input::new(f);
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"ftyp" => {
                let ftyp = try!(read_ftyp(&mut b.content, &b.head));
                log!(context, "{:?}", ftyp);
            }
            b"moov" => try!(read_moov(&mut b.content, context)),
            _ => {
                // Skip the contents of unknown chunks.
                log!(context, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        };
        check_parser_state!(context, b.content);
    }
    Ok(())
}

fn parse_mvhd<T: Read>(f: &mut T, h: &BoxHeader) -> Result<(MovieHeaderBox, Option<MediaTimeScale>)> {
    let mvhd = try!(read_mvhd(f, &h));
    if mvhd.timescale == 0 {
        return Err(Error::InvalidData("zero timescale in mdhd"));
    }
    let timescale = Some(MediaTimeScale(mvhd.timescale as u64));
    Ok((mvhd, timescale))
}

fn read_moov<T: Read>(f: &mut T, context: &mut MediaContext) -> Result<()> {
    let mut x = Input::new(f);
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"mvhd" => {
                let (mvhd, timescale) = try!(parse_mvhd(&mut b.content, &b.head));
                context.timescale = timescale;
                log!(context, "  {:?}", mvhd);
            }
            b"trak" => {
                let mut track = Track::new(context.tracks.len());
                track.trace = context.trace;
                try!(read_trak(&mut b.content, context, &mut track));
                context.tracks.push(track);
            }
            _ => {
                // Skip the contents of unknown chunks.
                log!(context, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        };
        check_parser_state!(context, b.content);
    }
    Ok(())
}

fn read_trak<T: Read>(f: &mut T, context: &mut MediaContext, track: &mut Track) -> Result<()> {
    let mut x = Input::new(f);
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"tkhd" => {
                let tkhd = try!(read_tkhd(&mut b.content, &b.head));
                track.track_id = Some(tkhd.track_id);
                track.tkhd = Some(tkhd.clone());
                log!(context, "  {:?}", tkhd);
            }
            b"edts" => try!(read_edts(&mut b.content, context, track)),
            b"mdia" => try!(read_mdia(&mut b.content, context, track)),
            _ => {
                // Skip the contents of unknown chunks.
                log!(context, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        };
        check_parser_state!(context, b.content);
    }
    Ok(())
}

fn read_edts<T: Read>(f: &mut T, context: &mut MediaContext, track: &mut Track) -> Result<()> {
    let mut x = Input::new(f);
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"elst" => {
                let elst = try!(read_elst(&mut b.content, &b.head));
                let mut empty_duration = 0;
                let mut idx = 0;
                if elst.edits.len() > 2 {
                    return Err(Error::Unsupported("more than two edits"));
                }
                if elst.edits[idx].media_time == -1 {
                    empty_duration = elst.edits[idx].segment_duration;
                    if elst.edits.len() < 2 {
                        return Err(Error::InvalidData("expected additional edit"));
                    }
                    idx += 1;
                }
                track.empty_duration = Some(MediaScaledTime(empty_duration));
                if elst.edits[idx].media_time < 0 {
                    return Err(Error::InvalidData("unexpected negative media time in edit"));
                }
                track.media_time = Some(TrackScaledTime(elst.edits[idx].media_time as u64,
                                                        track.id));
                log!(context, "  {:?}", elst);
            }
            _ => {
                // Skip the contents of unknown chunks.
                log!(context, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        };
        check_parser_state!(context, b.content);
    }
    Ok(())
}

fn parse_mdhd<T: Read>(f: &mut T, h: &BoxHeader, track: &mut Track) -> Result<(MediaHeaderBox, Option<TrackScaledTime>, Option<TrackTimeScale>)> {
    let mdhd = try!(read_mdhd(f, h));
    let duration = match mdhd.duration {
        std::u64::MAX => None,
        duration => Some(TrackScaledTime(duration, track.id)),
    };
    if mdhd.timescale == 0 {
        return Err(Error::InvalidData("zero timescale in mdhd"));
    }
    let timescale = Some(TrackTimeScale(mdhd.timescale as u64, track.id));
    Ok((mdhd, duration, timescale))
}

fn read_mdia<T: Read>(f: &mut T, context: &mut MediaContext, track: &mut Track) -> Result<()> {
    let mut x = Input::new(f);
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"mdhd" => {
                let (mdhd, duration, timescale) = try!(parse_mdhd(&mut b.content, &b.head, track));
                track.duration = duration;
                track.timescale = timescale;
                log!(context, "  {:?}", mdhd);
            }
            b"hdlr" => {
                let hdlr = try!(read_hdlr(&mut b.content, &b.head));
                match &hdlr.handler_type.0 {
                    b"vide" => track.track_type = TrackType::Video,
                    b"soun" => track.track_type = TrackType::Audio,
                    _ => (),
                }
                log!(context, "  {:?}", hdlr);
            }
            b"minf" => try!(read_minf(&mut b.content, context, track)),
            _ => {
                // Skip the contents of unknown chunks.
                log!(context, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        };
        check_parser_state!(context, b.content);
    }
    Ok(())
}

fn read_minf<T: Read>(f: &mut T, context: &mut MediaContext, track: &mut Track) -> Result<()> {
    let mut x = Input::new(f);
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"stbl" => try!(read_stbl(&mut b.content, context, track)),
            _ => {
                // Skip the contents of unknown chunks.
                log!(context, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        };
        check_parser_state!(context, b.content);
    }
    Ok(())
}

fn read_stbl<T: Read>(f: &mut T, context: &mut MediaContext, track: &mut Track) -> Result<()> {
    let mut x = Input::new(f);
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"stsd" => {
                let stsd = try!(read_stsd(&mut b.content, &b.head, track));
                log!(context, "  {:?}", stsd);
            }
            b"stts" => {
                let stts = try!(read_stts(&mut b.content, &b.head));
                log!(context, "  {:?}", stts);
            }
            b"stsc" => {
                let stsc = try!(read_stsc(&mut b.content, &b.head));
                log!(context, "  {:?}", stsc);
            }
            b"stsz" => {
                let stsz = try!(read_stsz(&mut b.content, &b.head));
                log!(context, "  {:?}", stsz);
            }
            b"stco" => {
                let stco = try!(read_stco(&mut b.content, &b.head));
                log!(context, "  {:?}", stco);
            }
            b"co64" => {
                let co64 = try!(read_co64(&mut b.content, &b.head));
                log!(context, "  {:?}", co64);
            }
            b"stss" => {
                let stss = try!(read_stss(&mut b.content, &b.head));
                log!(context, "  {:?}", stss);
            }
            _ => {
                // Skip the contents of unknown chunks.
                log!(context, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        };
        check_parser_state!(context, b.content);
    }
    Ok(())
}

/// Parse an ftyp box.
fn read_ftyp<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<FileTypeBox> {
    let major = try!(be_fourcc(src));
    let minor = try!(be_u32(src));
    let bytes_left = head.size - head.offset - 8;
    if bytes_left % 4 != 0 {
        return Err(Error::InvalidData("invalid ftyp size"));
    }
    // Is a brand_count of zero valid?
    let brand_count = bytes_left / 4;
    let mut brands = Vec::new();
    for _ in 0..brand_count {
        brands.push(try!(be_fourcc(src)));
    }
    Ok(FileTypeBox {
        header: *head,
        major_brand: major,
        minor_version: minor,
        compatible_brands: brands,
    })
}

/// Parse an mvhd box.
fn read_mvhd<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<MovieHeaderBox> {
    let (version, _) = try!(read_fullbox_extra(src));
    match version {
        // 64 bit creation and modification times.
        1 => {
            try!(skip(src, 16));
        }
        // 32 bit creation and modification times.
        0 => {
            try!(skip(src, 8));
        }
        _ => return Err(Error::InvalidData("unhandled mvhd version")),
    }
    let timescale = try!(be_u32(src));
    let duration = match version {
        1 => try!(be_u64(src)),
        0 => {
            let d = try!(be_u32(src));
            if d == std::u32::MAX {
                std::u64::MAX
            } else {
                d as u64
            }
        }
        _ => return Err(Error::InvalidData("unhandled mvhd version")),
    };
    // Skip remaining fields.
    try!(skip(src, 80));
    Ok(MovieHeaderBox {
        header: *head,
        timescale: timescale,
        duration: duration,
    })
}

/// Parse a tkhd box.
fn read_tkhd<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<TrackHeaderBox> {
    let (version, flags) = try!(read_fullbox_extra(src));
    let disabled = flags & 0x1u32 == 0 || flags & 0x2u32 == 0;
    match version {
        // 64 bit creation and modification times.
        1 => {
            try!(skip(src, 16));
        }
        // 32 bit creation and modification times.
        0 => {
            try!(skip(src, 8));
        }
        _ => return Err(Error::InvalidData("unhandled tkhd version")),
    }
    let track_id = try!(be_u32(src));
    try!(skip(src, 4));
    let duration = match version {
        1 => try!(be_u64(src)),
        0 => try!(be_u32(src)) as u64,
        _ => return Err(Error::InvalidData("unhandled tkhd version")),
    };
    // Skip uninteresting fields.
    try!(skip(src, 52));
    let width = try!(be_u32(src));
    let height = try!(be_u32(src));
    Ok(TrackHeaderBox {
        header: *head,
        track_id: track_id,
        disabled: disabled,
        duration: duration,
        width: width,
        height: height,
    })
}

/// Parse a elst box.
fn read_elst<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<EditListBox> {
    let (version, _) = try!(read_fullbox_extra(src));
    let edit_count = try!(be_u32(src));
    if edit_count == 0 {
        return Err(Error::InvalidData("invalid edit count"));
    }
    let mut edits = Vec::new();
    for _ in 0..edit_count {
        let (segment_duration, media_time) = match version {
            1 => {
                // 64 bit segment duration and media times.
                (try!(be_u64(src)), try!(be_i64(src)))
            }
            0 => {
                // 32 bit segment duration and media times.
                (try!(be_u32(src)) as u64, try!(be_i32(src)) as i64)
            }
            _ => return Err(Error::InvalidData("unhandled elst version")),
        };
        let media_rate_integer = try!(be_i16(src));
        let media_rate_fraction = try!(be_i16(src));
        edits.push(Edit {
            segment_duration: segment_duration,
            media_time: media_time,
            media_rate_integer: media_rate_integer,
            media_rate_fraction: media_rate_fraction,
        })
    }

    Ok(EditListBox {
        header: *head,
        edits: edits,
    })
}

/// Parse a mdhd box.
fn read_mdhd<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<MediaHeaderBox> {
    let (version, _) = try!(read_fullbox_extra(src));
    let (timescale, duration) = match version {
        1 => {
            // Skip 64-bit creation and modification times.
            try!(skip(src, 16));

            // 64 bit duration.
            (try!(be_u32(src)), try!(be_u64(src)))
        }
        0 => {
            // Skip 32-bit creation and modification times.
            try!(skip(src, 8));

            // 32 bit duration.
            let timescale = try!(be_u32(src));
            let duration = {
                // Since we convert the 32-bit duration to 64-bit by
                // upcasting, we need to preserve the special all-1s
                // ("unknown") case by hand.
                let d = try!(be_u32(src));
                if d == std::u32::MAX {
                    std::u64::MAX
                } else {
                    d as u64
                }
            };
            (timescale, duration)
        }
        _ => return Err(Error::InvalidData("unhandled mdhd version")),
    };

    // Skip uninteresting fields.
    try!(skip(src, 4));

    Ok(MediaHeaderBox {
        header: *head,
        timescale: timescale,
        duration: duration,
    })
}

/// Parse a stco box.
fn read_stco<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<ChunkOffsetBox> {
    let (_, _) = try!(read_fullbox_extra(src));
    let offset_count = try!(be_u32(src));
    let mut offsets = Vec::new();
    for _ in 0..offset_count {
        offsets.push(try!(be_u32(src)) as u64);
    }

    Ok(ChunkOffsetBox {
        header: *head,
        offsets: offsets,
    })
}

/// Parse a stco box.
fn read_co64<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<ChunkOffsetBox> {
    let (_, _) = try!(read_fullbox_extra(src));
    let offset_count = try!(be_u32(src));
    let mut offsets = Vec::new();
    for _ in 0..offset_count {
        offsets.push(try!(be_u64(src)));
    }

    Ok(ChunkOffsetBox {
        header: *head,
        offsets: offsets,
    })
}

/// Parse a stss box.
fn read_stss<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<SyncSampleBox> {
    let (_, _) = try!(read_fullbox_extra(src));
    let sample_count = try!(be_u32(src));
    let mut samples = Vec::new();
    for _ in 0..sample_count {
        samples.push(try!(be_u32(src)));
    }

    Ok(SyncSampleBox {
        header: *head,
        samples: samples,
    })
}

/// Parse a stsc box.
fn read_stsc<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<SampleToChunkBox> {
    let (_, _) = try!(read_fullbox_extra(src));
    let sample_count = try!(be_u32(src));
    let mut samples = Vec::new();
    for _ in 0..sample_count {
        let first_chunk = try!(be_u32(src));
        let samples_per_chunk = try!(be_u32(src));
        let sample_description_index = try!(be_u32(src));
        samples.push(SampleToChunk {
            first_chunk: first_chunk,
            samples_per_chunk: samples_per_chunk,
            sample_description_index: sample_description_index,
        });
    }

    Ok(SampleToChunkBox {
        header: *head,
        samples: samples,
    })
}

/// Parse a stsz box.
fn read_stsz<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<SampleSizeBox> {
    let (_, _) = try!(read_fullbox_extra(src));
    let sample_size = try!(be_u32(src));
    let sample_count = try!(be_u32(src));
    let mut sample_sizes = Vec::new();
    if sample_size == 0 {
        for _ in 0..sample_count {
            sample_sizes.push(try!(be_u32(src)));
        }
    }

    Ok(SampleSizeBox {
        header: *head,
        sample_size: sample_size,
        sample_sizes: sample_sizes,
    })
}

/// Parse a stts box.
fn read_stts<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<TimeToSampleBox> {
    let (_, _) = try!(read_fullbox_extra(src));
    let sample_count = try!(be_u32(src));
    let mut samples = Vec::new();
    for _ in 0..sample_count {
        let sample_count = try!(be_u32(src));
        let sample_delta = try!(be_u32(src));
        samples.push(Sample {
            sample_count: sample_count,
            sample_delta: sample_delta,
        });
    }

    Ok(TimeToSampleBox {
        header: *head,
        samples: samples,
    })
}

/// Parse a VPx Config Box.
fn read_vpcc<T: ReadBytesExt>(src: &mut T) -> Result<VPxConfigBox> {
    let (version, _) = try!(read_fullbox_extra(src));
    if version != 0 {
        return Err(Error::Unsupported("unknown vpcC version"));
    }

    let profile = try!(src.read_u8());
    let level = try!(src.read_u8());
    let (bit_depth, color_space) = {
        let byte = try!(src.read_u8());
        ((byte >> 4) & 0x0f, byte & 0x0f)
    };
    let (chroma_subsampling, transfer_function, video_full_range) = {
        let byte = try!(src.read_u8());
        ((byte >> 4) & 0x0f, (byte >> 1) & 0x07, (byte & 1) == 1)
    };

    let codec_init_size = try!(be_u16(src));
    let codec_init = try!(read_buf(src, codec_init_size as usize));

    // TODO(rillian): validate field value ranges.
    Ok(VPxConfigBox {
        profile: profile,
        level: level,
        bit_depth: bit_depth,
        color_space: color_space,
        chroma_subsampling: chroma_subsampling,
        transfer_function: transfer_function,
        video_full_range: video_full_range,
        codec_init: codec_init,
    })
}

/// Parse OpusSpecificBox.
fn read_dops<T: ReadBytesExt>(src: &mut T) -> Result<OpusSpecificBox> {
    let version = try!(src.read_u8());
    if version != 0 {
        return Err(Error::Unsupported("unknown dOps version"));
    }

    let output_channel_count = try!(src.read_u8());
    let pre_skip = try!(be_u16(src));
    let input_sample_rate = try!(be_u32(src));
    let output_gain = try!(be_i16(src));
    let channel_mapping_family = try!(src.read_u8());

    let channel_mapping_table = if channel_mapping_family == 0 {
        None
    } else {
        let stream_count = try!(src.read_u8());
        let coupled_count = try!(src.read_u8());
        let channel_mapping = try!(read_buf(src, output_channel_count as usize));

        Some(ChannelMappingTable {
            stream_count: stream_count,
            coupled_count: coupled_count,
            channel_mapping: channel_mapping,
        })
    };

    // TODO(kinetik): validate field value ranges.
    Ok(OpusSpecificBox {
        version: version,
        output_channel_count: output_channel_count,
        pre_skip: pre_skip,
        input_sample_rate: input_sample_rate,
        output_gain: output_gain,
        channel_mapping_family: channel_mapping_family,
        channel_mapping_table: channel_mapping_table,
    })
}

/// Parse a hdlr box.
fn read_hdlr<T: ReadBytesExt>(src: &mut T, head: &BoxHeader) -> Result<HandlerBox> {
    let (_, _) = try!(read_fullbox_extra(src));

    // Skip uninteresting fields.
    try!(skip(src, 4));

    let handler_type = try!(be_fourcc(src));

    // Skip uninteresting fields.
    try!(skip(src, 12));

    let bytes_left = head.size - head.offset - 24;
    let _name = try!(read_null_terminated_string(src, bytes_left as usize));

    Ok(HandlerBox {
        header: *head,
        handler_type: handler_type,
    })
}

/// Parse an video description inside an stsd box.
fn read_video_desc<T: ReadBytesExt>(src: &mut T, h: &BoxHeader, track: &mut Track) -> Result<SampleEntry> {
    track.mime_type = match h.name.as_bytes() {
        b"avc1" | b"avc3" => String::from("video/avc"),
        b"vp08" => String::from("video/vp8"),
        b"vp09" => String::from("video/vp9"),
        // TODO(kinetik): encv here also.
        b"encv" => String::from("video/crypto"),
        _ => return Err(Error::Unsupported("unhandled video sample entry type")),
    };

    // Skip uninteresting fields.
    try!(skip(src, 6));

    let data_reference_index = try!(be_u16(src));

    // Skip uninteresting fields.
    try!(skip(src, 16));

    let width = try!(be_u16(src));
    let height = try!(be_u16(src));

    // Skip uninteresting fields.
    try!(skip(src, 14));

    let _compressorname = try!(read_fixed_length_pascal_string(src, 32));

    // Skip uninteresting fields.
    try!(skip(src, 4));

    // Skip clap/pasp/etc. for now.
    let mut x = Input::new(src);
    let mut codec_specific = None;
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"avcC" => {
                if (h.name.as_bytes() != b"avc1" &&
                    h.name.as_bytes() != b"avc3" &&
                    h.name.as_bytes() != b"encv") ||
                    codec_specific.is_some() {
                        return Err(Error::InvalidData("malformed video sample entry"));
                    }
                let avcc_size = h.size - h.offset;
                if avcc_size > BUF_SIZE_LIMIT {
                    return Err(Error::InvalidData("avcC box exceeds BUF_SIZE_LIMIT"));
                }
                let avcc = try!(read_buf(&mut b.content, avcc_size as usize));
                // TODO(kinetik): Parse avcC atom?  For now we just stash the data.
                codec_specific = Some(VideoCodecSpecific::AVCConfig(avcc));
            }
            b"vpcC" => {
                if (h.name.as_bytes() != b"vp08" &&
                    h.name.as_bytes() != b"vp09") ||
                    codec_specific.is_some() {
                        return Err(Error::InvalidData("malformed video sample entry"));
                    }
                let vpcc = try!(read_vpcc(&mut b.content));
                codec_specific = Some(VideoCodecSpecific::VPxConfig(vpcc));
            }
            _ => {
                // Skip the contents of unknown chunks.
                log!(track, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        }
        check_parser_state!(track, b.content);
    }

    if codec_specific.is_none() {
        return Err(Error::InvalidData("malformed video sample entry"));
    }

    Ok(SampleEntry::Video(VideoSampleEntry {
        data_reference_index: data_reference_index,
        width: width,
        height: height,
        codec_specific: codec_specific.unwrap(),
    }))
}

/// Parse an audio description inside an stsd box.
fn read_audio_desc<T: ReadBytesExt>(src: &mut T, h: &BoxHeader, track: &mut Track) -> Result<SampleEntry> {
    // TODO(kinetik): enforce expected subbox, e.g. mp4a should have esds not dOps
    track.mime_type = match h.name.as_bytes() {
        // TODO(kinetik): stagefright inspects ESDS to detect MP3 (audio/mpeg).
        b"mp4a" => String::from("audio/mp4a-latm"),
        // TODO(kinetik): stagefright doesn't have a MIME mapping for this, revisit.
        b"Opus" => String::from("audio/opus"),
        // TODO(kinetik): enca here also?
        b"enca" => String::from("audio/crypto"),
        _ => return Err(Error::Unsupported("unhandled audio sample entry type")),
    };

    // Skip uninteresting fields.
    try!(skip(src, 6));

    let data_reference_index = try!(be_u16(src));

    // Skip uninteresting fields.
    try!(skip(src, 8));

    let channelcount = try!(be_u16(src));
    let samplesize = try!(be_u16(src));

    // Skip uninteresting fields.
    try!(skip(src, 4));

    let samplerate = try!(be_u32(src));

    // Skip chan/etc. for now.
    let mut x = Input::new(src);
    let mut codec_specific = None;
    while let Some(b) = x.next() {
        let mut b = try!(b);
        match b.head.name.as_bytes() {
            b"esds" => {
                if (h.name.as_bytes() != b"mp4a" &&
                    h.name.as_bytes() != b"enca") ||
                    codec_specific.is_some() {
                        return Err(Error::InvalidData("malformed audio sample entry"));
                    }
                let (_, _) = try!(read_fullbox_extra(&mut b.content));
                let esds_size = b.head.size - b.head.offset - 4;
                if esds_size > BUF_SIZE_LIMIT {
                    return Err(Error::InvalidData("esds box exceeds BUF_SIZE_LIMIT"));
                }
                let esds = try!(read_buf(&mut b.content, esds_size as usize));
                // TODO(kinetik): Parse esds atom?  For now we just stash the data.
                codec_specific = Some(AudioCodecSpecific::ES_Descriptor(esds));
            }
            b"dOps" => {
                if h.name.as_bytes() != b"Opus" ||
                    codec_specific.is_some() {
                    return Err(Error::InvalidData("malformed audio sample entry"));
                }
                let dops = try!(read_dops(&mut b.content));
                codec_specific = Some(AudioCodecSpecific::OpusSpecificBox(dops));
            }
            _ => {
                // Skip the contents of unknown chunks.
                log!(track, "{:?} (skipped)", b.head);
                try!(skip_box_content(&mut b.content, &b.head));
            }
        }
        check_parser_state!(track, b.content);
    }

    if codec_specific.is_none() {
        return Err(Error::InvalidData("malformed audio sample entry"));
    }

    Ok(SampleEntry::Audio(AudioSampleEntry {
        data_reference_index: data_reference_index,
        channelcount: channelcount,
        samplesize: samplesize,
        samplerate: samplerate,
        codec_specific: codec_specific.unwrap(),
    }))
}

/// Parse a stsd box.
fn read_stsd<T: ReadBytesExt>(src: &mut T, head: &BoxHeader, track: &mut Track) -> Result<SampleDescriptionBox> {
    let (_, _) = try!(read_fullbox_extra(src));

    let description_count = try!(be_u32(src));
    let mut descriptions = Vec::new();

    // TODO(kinetik): check if/when more than one desc per track? do we need to support?
    for _ in 0..description_count {
        let head = try!(read_box_header(src));
        let description = match track.track_type {
            TrackType::Video => try!(read_video_desc(&mut limit(src, &head), &head, track)),
            TrackType::Audio => try!(read_audio_desc(&mut limit(src, &head), &head, track)),
            TrackType::Unknown => {
                try!(skip_box_content(&mut limit(src, &head), &head));
                SampleEntry::Unknown
            }
        };
        if track.data.is_none() {
            track.data = Some(description.clone());
        } else {
            log!(track, "** don't know how to handle multiple descriptions **");
        }
        descriptions.push(description);
    }

    Ok(SampleDescriptionBox {
        header: *head,
        descriptions: descriptions,
    })
}

/// Skip a number of bytes that we don't care to parse.
fn skip<T: Read>(src: &mut T, mut bytes: usize) -> Result<()> {
    const BUF_SIZE: usize = 64 * 1024;
    let mut buf = vec![0; BUF_SIZE];
    while bytes > 0 {
        let buf_size = cmp::min(bytes, BUF_SIZE);
        let len = try!(src.take(buf_size as u64).read(&mut buf));
        if len == 0 {
            return Err(Error::UnexpectedEOF);
        }
        bytes -= len;
    }
    Ok(())
}

/// Read size bytes into a Vector or return error.
fn read_buf<T: ReadBytesExt>(src: &mut T, size: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0; size];
    let r = try!(src.read(&mut buf));
    if r != size {
        return Err(Error::InvalidData("failed buffer read"));
    }
    Ok(buf)
}

// TODO(kinetik): Find a copy of ISO/IEC 14496-1 to confirm various string encodings.
// XXX(kinetik): definition of "null-terminated" string is fuzzy, we have:
// - zero or more byte strings, with a single null terminating the string.
// - zero byte strings with no null terminator (i.e. zero space in the box for the string)
// - length-prefixed strings with no null terminator (e.g. bear_rotate_0.mp4)
fn read_null_terminated_string<T: ReadBytesExt>(src: &mut T, mut size: usize) -> Result<String> {
    let mut buf = Vec::new();
    while size > 0 {
        let c = try!(src.read_u8());
        if c == 0 {
            break;
        }
        buf.push(c);
        size -= 1;
    }
    Ok(try!(String::from_utf8(buf)))
}

#[allow(dead_code)]
fn read_pascal_string<T: ReadBytesExt>(src: &mut T) -> Result<String> {
    let len = try!(src.read_u8());
    let buf = try!(read_buf(src, len as usize));
    Ok(try!(String::from_utf8(buf)))
}

// Weird string encoding with a length prefix and a fixed sized buffer which
// contains padding if the string doesn't fill the buffer.
fn read_fixed_length_pascal_string<T: Read>(src: &mut T, size: usize) -> Result<String> {
    assert!(size > 0);
    let len = cmp::min(try!(src.read_u8()) as usize, size - 1);
    let buf = try!(read_buf(src, len as usize));
    try!(skip(src, size - 1 - buf.len()));
    Ok(try!(String::from_utf8(buf)))
}

fn media_time_to_ms(time: MediaScaledTime, scale: MediaTimeScale) -> u64 {
    assert!(scale.0 != 0);
    time.0 * 1000000 / scale.0
}

fn track_time_to_ms(time: TrackScaledTime, scale: TrackTimeScale) -> u64 {
    assert!(time.1 == scale.1);
    assert!(scale.0 != 0);
    time.0 * 1000000 / scale.0
}

fn be_i16<T: ReadBytesExt>(src: &mut T) -> byteorder::Result<i16> {
    src.read_i16::<byteorder::BigEndian>()
}

fn be_i32<T: ReadBytesExt>(src: &mut T) -> byteorder::Result<i32> {
    src.read_i32::<byteorder::BigEndian>()
}

fn be_i64<T: ReadBytesExt>(src: &mut T) -> byteorder::Result<i64> {
    src.read_i64::<byteorder::BigEndian>()
}

fn be_u16<T: ReadBytesExt>(src: &mut T) -> byteorder::Result<u16> {
    src.read_u16::<byteorder::BigEndian>()
}

fn be_u32<T: ReadBytesExt>(src: &mut T) -> byteorder::Result<u32> {
    src.read_u32::<byteorder::BigEndian>()
}

fn be_u64<T: ReadBytesExt>(src: &mut T) -> byteorder::Result<u64> {
    src.read_u64::<byteorder::BigEndian>()
}

fn be_fourcc<T: Read>(src: &mut T) -> Result<FourCC> {
    let mut fourcc = [0; 4];
    match src.read(&mut fourcc) {
        // Expect all 4 bytes read.
        Ok(4) => Ok(FourCC(fourcc)),
        // Short read means EOF.
        Ok(_) => Err(Error::UnexpectedEOF),
        // Propagate std::io errors.
        Err(e) => Err(Error::Io(e)),
    }
}
