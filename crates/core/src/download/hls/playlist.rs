use std::cmp::Reverse;

use m3u8_rs::{
    AlternativeMedia, AlternativeMediaType, ExtTag, KeyMethod, MasterPlaylist, MediaPlaylist,
    MediaPlaylistType, Playlist as ParsedPlaylist, VariantStream,
};
use reqwest::Url;

const MAX_PLAYLIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_SEGMENTS: usize = 100_000;

/// Tags this crate knows how to parse. Finding one of them in the "unknown" tag list means the
/// parser rejected a critical tag instead of silently skipping it.
const PARSEABLE_TAGS: &[&str] = &[
    "X-STREAM-INF",
    "X-I-FRAME-STREAM-INF",
    "X-MEDIA",
    "X-SESSION-KEY",
    "X-SESSION-DATA",
    "X-START",
    "X-TARGETDURATION",
    "X-MEDIA-SEQUENCE",
    "X-DISCONTINUITY-SEQUENCE",
    "X-ENDLIST",
    "X-PLAYLIST-TYPE",
    "X-I-FRAMES-ONLY",
    "X-KEY",
    "X-MAP",
    "X-BYTERANGE",
    "X-DISCONTINUITY",
    "X-PROGRAM-DATE-TIME",
    "X-DATERANGE",
];

/// Tags outside the approved scope: live/low-latency hints, playlist variables and segment gaps.
const UNSUPPORTED_TAGS: &[&str] = &[
    "X-GAP",
    "X-DEFINE",
    "X-SKIP",
    "X-CONTENT-STEERING",
    "X-PART",
    "X-PART-INF",
    "X-SERVER-CONTROL",
    "X-PRELOAD-HINT",
    "X-RENDITION-REPORT",
    "X-BLOCK-RELOAD",
];

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Playlist {
    Master(Vec<Variant>),
    Media(MediaPlan),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Variant {
    pub url: String,
    pub bandwidth: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MediaPlan {
    pub playlist_url: String,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    pub sequence: u64,
    pub url: String,
    pub duration: f64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PlaylistError {
    #[error("playlist is empty")]
    EmptyPlaylist,
    #[error("playlist exceeds the {limit} byte limit")]
    PlaylistTooLarge { limit: usize },
    #[error("playlist is malformed: {0}")]
    Malformed(String),
    #[error("playlist contains an unsupported HLS tag: {0}")]
    UnsupportedTag(String),
    #[error("playlist contains an invalid URL: {0}")]
    InvalidUrl(String),
    #[error("playlist URL scheme {0:?} is not supported")]
    UnsupportedScheme(String),
    #[error("playlist URLs must not embed credentials")]
    EmbeddedCredentials,
    #[error("playlist URLs must not contain fragments")]
    FragmentNotAllowed,
    #[error("encrypted HLS segments are not supported")]
    Encrypted,
    #[error("HLS segment byte ranges are not supported")]
    ByteRange,
    #[error("HLS fragmented MP4 initialization sections (EXT-X-MAP) are not supported")]
    FragmentedMp4,
    #[error("HLS discontinuities are not supported")]
    Discontinuity,
    #[error("master playlist has no self-contained variant stream")]
    NoUsableVariants,
    #[error("media playlist has no segments")]
    NoSegments,
    #[error("media playlist exceeds the {limit} segment limit")]
    TooManySegments { limit: usize },
    #[error("media playlist is live or incomplete (missing EXT-X-ENDLIST)")]
    MissingEndList,
}

pub(crate) fn parse_playlist(bytes: &[u8], final_url: &str) -> Result<Playlist, PlaylistError> {
    let base = playlist_url(final_url)?;
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(PlaylistError::EmptyPlaylist);
    }
    if body.len() > MAX_PLAYLIST_BYTES {
        return Err(PlaylistError::PlaylistTooLarge {
            limit: MAX_PLAYLIST_BYTES,
        });
    }

    let text = std::str::from_utf8(body)
        .map_err(|_| PlaylistError::Malformed("playlist must be UTF-8".into()))?;
    for line in text.lines().map(str::trim) {
        if let Some(attributes) = line.strip_prefix("#EXT-X-KEY:")
            && attributes != "METHOD=NONE"
        {
            return Err(PlaylistError::Encrypted);
        }
    }

    let (rest, parsed) = m3u8_rs::parse_playlist(body)
        .map_err(|_| PlaylistError::Malformed("playlist syntax error".into()))?;
    if !rest.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(PlaylistError::Malformed(
            "unexpected trailing content after playlist".into(),
        ));
    }

    match parsed {
        ParsedPlaylist::MasterPlaylist(master) => master_plan(&base, master),
        ParsedPlaylist::MediaPlaylist(media) => media_plan(&base, media),
    }
}

fn master_plan(base: &Url, master: MasterPlaylist) -> Result<Playlist, PlaylistError> {
    if !master.session_key.is_empty() {
        return Err(PlaylistError::Encrypted);
    }
    check_tags(&master.unknown_tags)?;

    let mut variants = Vec::with_capacity(master.variants.len());
    for stream in &master.variants {
        if stream.is_i_frame || !is_self_contained(stream, &master.alternatives) {
            continue;
        }
        variants.push(Variant {
            url: resolve_url(base, &stream.uri)?,
            bandwidth: stream.bandwidth,
        });
    }
    if variants.is_empty() {
        return Err(PlaylistError::NoUsableVariants);
    }
    variants.sort_by_key(|variant| Reverse(variant.bandwidth));

    Ok(Playlist::Master(variants))
}

/// A variant is only usable when audio and video are muxed into it and no subtitle track is
/// required, so a single segment stream can be downloaded on its own.
fn is_self_contained(stream: &VariantStream, alternatives: &[AlternativeMedia]) -> bool {
    stream.subtitles.is_none()
        && is_muxed_group(
            alternatives,
            AlternativeMediaType::Audio,
            stream.audio.as_deref(),
        )
        && is_muxed_group(
            alternatives,
            AlternativeMediaType::Video,
            stream.video.as_deref(),
        )
}

fn is_muxed_group(
    alternatives: &[AlternativeMedia],
    media_type: AlternativeMediaType,
    group: Option<&str>,
) -> bool {
    let Some(group) = group else {
        return true;
    };
    let mut found = false;
    for rendition in alternatives
        .iter()
        .filter(|rendition| rendition.media_type == media_type && rendition.group_id == group)
    {
        found = true;
        if rendition.uri.is_some() {
            return false;
        }
    }
    found
}

fn media_plan(base: &Url, media: MediaPlaylist) -> Result<Playlist, PlaylistError> {
    if media.i_frames_only {
        return Err(PlaylistError::UnsupportedTag("#EXT-X-I-FRAMES-ONLY".into()));
    }
    check_tags(&media.unknown_tags)?;
    if media.target_duration == 0 {
        return Err(PlaylistError::Malformed(
            "missing or non-positive EXT-X-TARGETDURATION".into(),
        ));
    }
    if let Some(MediaPlaylistType::Other(value)) = &media.playlist_type {
        return Err(PlaylistError::Malformed(format!(
            "unknown playlist type {value:?}"
        )));
    }
    if media.discontinuity_sequence != 0 {
        return Err(PlaylistError::Discontinuity);
    }
    if !media.end_list {
        return Err(PlaylistError::MissingEndList);
    }
    if media.segments.is_empty() {
        return Err(PlaylistError::NoSegments);
    }
    if media.segments.len() > MAX_SEGMENTS {
        return Err(PlaylistError::TooManySegments {
            limit: MAX_SEGMENTS,
        });
    }

    let mut segments = Vec::with_capacity(media.segments.len());
    for (index, segment) in media.segments.iter().enumerate() {
        check_tags(&segment.unknown_tags)?;
        if segment.discontinuity {
            return Err(PlaylistError::Discontinuity);
        }
        if segment.map.is_some() {
            return Err(PlaylistError::FragmentedMp4);
        }
        if segment
            .key
            .as_ref()
            .is_some_and(|key| key.method != KeyMethod::None)
        {
            return Err(PlaylistError::Encrypted);
        }
        if segment.byte_range.is_some() {
            return Err(PlaylistError::ByteRange);
        }

        let duration = f64::from(segment.duration);
        if !duration.is_finite() || duration <= 0.0 {
            return Err(PlaylistError::Malformed(format!(
                "segment {index} has an invalid duration {duration}"
            )));
        }
        let sequence = media
            .media_sequence
            .checked_add(u64::try_from(index).unwrap_or(u64::MAX))
            .ok_or_else(|| PlaylistError::Malformed("media sequence overflow".into()))?;

        segments.push(Segment {
            sequence,
            url: resolve_url(base, &segment.uri)?,
            duration,
        });
    }

    Ok(Playlist::Media(MediaPlan {
        playlist_url: base.as_str().to_owned(),
        segments,
    }))
}

fn check_tags(tags: &[ExtTag]) -> Result<(), PlaylistError> {
    for tag in tags {
        let name = tag.tag.as_str();
        if name == "X-KEY" && is_clear_key(tag) {
            continue;
        }
        if PARSEABLE_TAGS.contains(&name) {
            return Err(PlaylistError::Malformed(format!(
                "#EXT-{name} could not be parsed"
            )));
        }
        if UNSUPPORTED_TAGS.contains(&name) {
            return Err(PlaylistError::UnsupportedTag(format!("#EXT-{name}")));
        }
    }
    Ok(())
}

/// `#EXT-X-KEY:METHOD=NONE` marks a return to clear segments; the parser turns it into an unknown
/// tag, so classify it explicitly instead of treating it as encryption. The METHOD value must match
/// the attribute token exactly, otherwise a typo would pass as a clear key.
fn is_clear_key(tag: &ExtTag) -> bool {
    tag.rest
        .as_deref()
        .is_some_and(|rest| rest.trim() == "METHOD=NONE")
}

/// Keeps signed query strings out of persisted error messages.
fn playlist_url(final_url: &str) -> Result<Url, PlaylistError> {
    let url = Url::parse(final_url).map_err(|_| {
        PlaylistError::InvalidUrl("playlist URL is not a valid absolute URL".into())
    })?;
    validate_url(&url)?;
    Ok(url)
}

fn resolve_url(base: &Url, reference: &str) -> Result<String, PlaylistError> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(PlaylistError::Malformed(
            "playlist contains an empty URI".into(),
        ));
    }
    let url = base.join(reference).map_err(|_| {
        PlaylistError::InvalidUrl("playlist URI cannot be resolved to an absolute URL".into())
    })?;
    validate_url(&url)?;
    Ok(url.into())
}

fn validate_url(url: &Url) -> Result<(), PlaylistError> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(PlaylistError::UnsupportedScheme(other.to_owned())),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PlaylistError::EmbeddedCredentials);
    }
    if url.fragment().is_some() {
        return Err(PlaylistError::FragmentNotAllowed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEDIA_URL: &str = "https://cdn.example.com/vod/index.m3u8";

    fn media(body: &str) -> Result<Playlist, PlaylistError> {
        parse_playlist(body.as_bytes(), MEDIA_URL)
    }

    fn media_body(segments: &str) -> String {
        format!(
            concat!(
                "#EXTM3U\n",
                "#EXT-X-VERSION:3\n",
                "#EXT-X-TARGETDURATION:6\n",
                "#EXT-X-MEDIA-SEQUENCE:7\n",
                "#EXT-X-PLAYLIST-TYPE:VOD\n",
                "{}\n",
                "#EXT-X-ENDLIST\n"
            ),
            segments
        )
    }

    fn error(body: &str) -> PlaylistError {
        media(body).expect_err("playlist should be rejected")
    }

    #[test]
    fn rejects_ambiguous_or_misspelled_clear_keys() {
        for attributes in [
            "METHOD=NONEBAD",
            "METHOD=NONE,METHOD=AES-128",
            "METHOD=AES-128,METHOD=NONE",
        ] {
            let body = media_body(&format!("#EXT-X-KEY:{attributes}\n#EXTINF:5.0,\nseg.ts"));
            assert!(matches!(media(&body), Err(PlaylistError::Encrypted)));
        }
    }

    #[test]
    fn parses_vod_media_playlist() {
        let body = media_body(concat!(
            "#EXTINF:5.0,\n",
            "seg-1.ts\n",
            "#EXTINF:5.5,\n",
            "https://other.example.com/seg-2.ts\n",
            "#EXTINF:4.25,\n",
            "../audio/seg-3.ts"
        ));

        let Playlist::Media(plan) = media(&body).expect("media playlist") else {
            panic!("expected media plan");
        };

        assert_eq!(plan.playlist_url, MEDIA_URL);
        assert_eq!(plan.segments.len(), 3);
        assert_eq!(
            plan.segments[0],
            Segment {
                sequence: 7,
                url: "https://cdn.example.com/vod/seg-1.ts".into(),
                duration: 5.0,
            }
        );
        assert_eq!(plan.segments[1].sequence, 8);
        assert_eq!(plan.segments[1].url, "https://other.example.com/seg-2.ts");
        assert_eq!(plan.segments[2].sequence, 9);
        assert_eq!(
            plan.segments[2].url,
            "https://cdn.example.com/audio/seg-3.ts"
        );
        assert!((plan.segments[2].duration - 4.25).abs() < f64::EPSILON);
    }

    #[test]
    fn resolves_urls_against_final_redirected_url() {
        let body = media_body("#EXTINF:5.0,\nseg.ts");
        let playlist = parse_playlist(
            body.as_bytes(),
            "https://edge.example.com/live/stream/index.m3u8?token=abc",
        )
        .expect("media playlist");

        let Playlist::Media(plan) = playlist else {
            panic!("expected media plan");
        };
        assert_eq!(
            plan.segments[0].url,
            "https://edge.example.com/live/stream/seg.ts"
        );
        assert_eq!(
            plan.playlist_url,
            "https://edge.example.com/live/stream/index.m3u8?token=abc"
        );
    }

    #[test]
    fn parses_master_playlist_and_prefers_muxed_variants() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,",
            "URI=\"audio/en.m3u8\"\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"muxed\",NAME=\"Muxed\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=800000,AUDIO=\"muxed\"\n",
            "low/index.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=2000000,AUDIO=\"muxed\",RESOLUTION=1280x720\n",
            "high/index.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=4000000,AUDIO=\"aud\"\n",
            "external/index.m3u8\n"
        );

        let Playlist::Master(variants) = media(body).expect("master playlist") else {
            panic!("expected master playlist");
        };

        assert_eq!(
            variants,
            vec![
                Variant {
                    url: "https://cdn.example.com/vod/high/index.m3u8".into(),
                    bandwidth: 2_000_000,
                },
                Variant {
                    url: "https://cdn.example.com/vod/low/index.m3u8".into(),
                    bandwidth: 800_000,
                },
            ]
        );
    }

    #[test]
    fn rejects_master_with_only_external_tracks() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",URI=\"audio/en.m3u8\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=4000000,AUDIO=\"aud\"\n",
            "high/index.m3u8\n"
        );
        assert!(matches!(media(body), Err(PlaylistError::NoUsableVariants)));
    }

    #[test]
    fn rejects_variants_requiring_subtitles() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",URI=\"subs/en.m3u8\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=800000,SUBTITLES=\"subs\"\n",
            "low/index.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=1600000\n",
            "high/index.m3u8\n"
        );

        let Playlist::Master(variants) = media(body).expect("master playlist") else {
            panic!("expected master playlist");
        };
        assert_eq!(
            variants,
            vec![Variant {
                url: "https://cdn.example.com/vod/high/index.m3u8".into(),
                bandwidth: 1_600_000,
            }]
        );
    }

    #[test]
    fn rejects_missing_endlist() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-TARGETDURATION:6\n",
            "#EXTINF:5.0,\n",
            "seg-1.ts\n"
        );
        assert!(matches!(error(body), PlaylistError::MissingEndList));
    }

    #[test]
    fn rejects_encrypted_segments() {
        let body = media_body(concat!(
            "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x0123456789ABCDEF0123456789ABCDEF\n",
            "#EXTINF:5.0,\nseg-1.ts"
        ));
        assert!(matches!(error(&body), PlaylistError::Encrypted));
    }

    #[test]
    fn allows_cleared_key_method() {
        let body = media_body(concat!(
            "#EXT-X-KEY:METHOD=NONE\n",
            "#EXTINF:5.0,\nseg-1.ts"
        ));
        assert!(matches!(media(&body), Ok(Playlist::Media(_))));
    }

    #[test]
    fn rejects_byte_ranges() {
        let body = media_body("#EXT-X-BYTERANGE:1000@0\n#EXTINF:5.0,\nseg-1.ts");
        assert!(matches!(error(&body), PlaylistError::ByteRange));
    }

    #[test]
    fn rejects_fragmented_mp4() {
        let body = media_body("#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:5.0,\nseg-1.ts");
        assert!(matches!(error(&body), PlaylistError::FragmentedMp4));
    }

    #[test]
    fn rejects_discontinuities() {
        let body = media_body(concat!(
            "#EXTINF:5.0,\nseg-1.ts\n",
            "#EXT-X-DISCONTINUITY\n",
            "#EXTINF:5.0,\nseg-2.ts"
        ));
        assert!(matches!(error(&body), PlaylistError::Discontinuity));
    }

    #[test]
    fn rejects_i_frame_playlists_and_gaps() {
        let i_frames = concat!(
            "#EXTM3U\n",
            "#EXT-X-TARGETDURATION:6\n",
            "#EXT-X-I-FRAMES-ONLY\n",
            "#EXTINF:5.0,\nseg-1.ts\n",
            "#EXT-X-ENDLIST\n"
        );
        assert!(matches!(error(i_frames), PlaylistError::UnsupportedTag(_)));

        let gap = media_body("#EXTINF:5.0,\n#EXT-X-GAP\nseg-1.ts");
        assert!(matches!(error(&gap), PlaylistError::UnsupportedTag(_)));
    }

    #[test]
    fn rejects_malformed_critical_tags_the_parser_skipped() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-TARGETDURATION:abc\n",
            "#EXTINF:5.0,\n",
            "seg-1.ts\n",
            "#EXT-X-ENDLIST\n"
        );
        assert!(matches!(error(body), PlaylistError::Malformed(_)));

        let master = concat!(
            "#EXTM3U\n",
            "#EXT-X-STREAM-INF:CODECS=\"avc1.4d401f\"\n",
            "low/index.m3u8\n"
        );
        assert!(matches!(media(master), Err(PlaylistError::Malformed(_))));
    }

    #[test]
    fn rejects_trailing_garbage() {
        let mut body = media_body("#EXTINF:5.0,\nseg-1.ts").into_bytes();
        body.extend_from_slice(b"\xFF\xFE");
        assert!(matches!(
            parse_playlist(&body, MEDIA_URL),
            Err(PlaylistError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_empty_and_oversized_playlists() {
        assert!(matches!(media(""), Err(PlaylistError::EmptyPlaylist)));
        assert!(matches!(media("  \n\n"), Err(PlaylistError::EmptyPlaylist)));

        let oversized = vec![b'#'; MAX_PLAYLIST_BYTES + 1];
        assert!(matches!(
            parse_playlist(&oversized, MEDIA_URL),
            Err(PlaylistError::PlaylistTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_invalid_durations() {
        let zero = media_body("#EXTINF:0,\nseg-1.ts");
        assert!(matches!(error(&zero), PlaylistError::Malformed(_)));

        let missing = media_body("seg-1.ts");
        assert!(matches!(error(&missing), PlaylistError::Malformed(_)));
    }

    #[test]
    fn rejects_unsafe_url_schemes_and_parts() {
        assert!(matches!(
            parse_playlist(b"#EXTM3U\n", "ftp://example.com/index.m3u8"),
            Err(PlaylistError::UnsupportedScheme(_))
        ));

        let credentials = media_body("#EXTINF:5.0,\nhttps://user:pass@example.com/seg.ts");
        assert!(matches!(
            error(&credentials),
            PlaylistError::EmbeddedCredentials
        ));

        let fragment = media_body("#EXTINF:5.0,\nseg.ts#frag");
        assert!(matches!(
            error(&fragment),
            PlaylistError::FragmentNotAllowed
        ));

        let file = media_body("#EXTINF:5.0,\nfile:///etc/passwd");
        assert!(matches!(error(&file), PlaylistError::UnsupportedScheme(_)));
    }
}
