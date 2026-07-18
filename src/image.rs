//! Compression detection and the `ImageReader` enum that dispatches reads
//! across raw / gzip / xz / bzip2 / zstd inputs.
//!
//! Why an enum, not `Box<dyn Read>`? Two reasons, neither is "static dispatch"
//! in the monomorphization sense — `match` on an enum variant is a runtime
//! branch, same class as a vtable. What the enum *does* avoid is heap
//! allocation of the trait object and the indirection through its vtable
//! pointer, and it lets the compiler inline each variant's `Read` impl where
//! the type is locally known.
//!
//! Buffering strategy: every decompressor wraps a `BufReader<File>`. We use
//! the `bufread::*` flavour of each decoder so that the decoder reuses our
//! 2 MiB buffer instead of allocating a second internal one.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};

/// Size of the buffered reader placed between the raw file and the
/// decompressor. 2 MiB matches the bash original's `pv` buffer hygiene and
/// is large enough to amortise file-read syscall cost across hundreds of
/// small decoder reads.
pub(crate) const IMG_BUFREAD_CAP: usize = 2 * 1024 * 1024;

/// Detected compression format (or `Raw` for an uncompressed image).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    /// No compression: bytes are written to the device verbatim.
    Raw,
    /// Gzip stream (magic `1F 8B`).
    Gzip,
    /// XZ stream (magic `FD 37 7A 58 5A 00`).
    Xz,
    /// Bzip2 stream (magic `42 5A 68`).
    Bzip2,
    /// Zstandard stream (magic `28 B5 2F FD`).
    Zstd,
}

impl Compression {
    /// True for every variant except [`Compression::Raw`].
    pub(crate) fn is_compressed(self) -> bool {
        !matches!(self, Compression::Raw)
    }

    /// Short lowercase name shown in the confirmation prompt.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Compression::Raw => "raw",
            Compression::Gzip => "gzip",
            Compression::Xz => "xz",
            Compression::Bzip2 => "bzip2",
            Compression::Zstd => "zstd",
        }
    }
}

/// Peek at the first 8 bytes of `path` and classify the compression format.
///
/// Reads a fixed prefix — never the whole file — and does not retain an
/// open FD past return. Safe to call before any destructive step.
pub(crate) fn detect_compression(path: &Path) -> Result<Compression> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = [0_u8; 8];
    let n = read_full_best_effort(&mut f, &mut buf)?;
    // Classify only the bytes actually read. Passing the zero-padded
    // tail would be wrong, not just untidy: the xz magic ends in 0x00,
    // so a truncated 5-byte xz prefix plus zero padding would
    // false-match the full 6-byte magic. `get` keeps this panic-free;
    // `n <= buf.len()` always holds, so the fallback is unreachable and
    // degrades to the safe default (Raw) if it ever weren't.
    Ok(classify(buf.get(..n).unwrap_or(&[])))
}

/// Read up to `buf.len()` bytes, returning however many we actually got.
/// Tolerates short files (a 3-byte file is still a legitimate image, just a
/// trivially small one).
#[expect(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "loop invariant: filled < buf.len() at the index site, and \
              n <= buf.len() - filled per the Read contract, so filled + n \
              cannot overflow buf.len(), let alone usize"
)]
fn read_full_best_effort<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            // Retry: EINTR is transparent to the caller.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Classify a magic-byte prefix.
fn classify(prefix: &[u8]) -> Compression {
    if prefix.starts_with(&[0x1F, 0x8B]) {
        Compression::Gzip
    } else if prefix.starts_with(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]) {
        Compression::Xz
    } else if prefix.starts_with(&[0x42, 0x5A, 0x68]) {
        Compression::Bzip2
    } else if prefix.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Compression::Zstd
    } else {
        Compression::Raw
    }
}

/// A `Read` that dispatches across compression variants without boxing.
///
/// Constructed by [`ImageReader::open`]; a single reader is good for one
/// linear pass. Verification (Phase 5) builds a brand new `ImageReader`
/// from the same path rather than trying to rewind, because none of the
/// decompressors support `Seek`.
pub(crate) enum ImageReader {
    /// Uncompressed passthrough.
    Raw(BufReader<File>),
    /// Gzip decoder over the buffered file. `MultiGzDecoder`, so
    /// multi-member files (pigz, bgzf, `cat a.gz b.gz`) decode in
    /// full — the single-member `GzDecoder` stops at the first member
    /// boundary, which silently truncates the flash.
    Gzip(flate2::bufread::MultiGzDecoder<BufReader<File>>),
    /// XZ decoder over the buffered file. Built with
    /// `new_multi_decoder`, so multi-stream files (produced by
    /// `xz --block-list`, some parallel compressors, or plain stream
    /// concatenation) decode in full instead of erroring with a
    /// misleading "corrupt xz stream" at the first stream boundary.
    Xz(xz2::bufread::XzDecoder<BufReader<File>>),
    /// Bzip2 decoder over the buffered file. `MultiBzDecoder`, so
    /// multi-stream files decode in full — critically, **pbzip2
    /// output is always multi-stream**, and the single-stream
    /// `BzDecoder` would silently flash only the first stream.
    Bz2(bzip2::bufread::MultiBzDecoder<BufReader<File>>),
    /// Zstd decoder over the buffered file. libzstd's streaming
    /// decoder handles concatenated frames natively; no multi variant
    /// is needed (pinned by `multi_member_zst_decodes_in_full`).
    Zstd(zstd::Decoder<'static, BufReader<File>>),
}

impl ImageReader {
    /// Open `path` fresh and construct an appropriate reader for the given
    /// detected compression format.
    ///
    /// The caller passes the pre-detected `Compression` to avoid a second
    /// magic-bytes peek (and to guarantee Phase 4 sees the same classification
    /// Phase 0 made its decision on).
    pub(crate) fn open(path: &Path, comp: Compression) -> Result<Self> {
        let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // Ensure we start at offset 0 (defence in depth; a freshly opened file
        // already is, but cheap assertion).
        let mut f = f;
        f.seek(SeekFrom::Start(0))
            .with_context(|| format!("seek to start of {}", path.display()))?;
        let buf = BufReader::with_capacity(IMG_BUFREAD_CAP, f);
        Ok(match comp {
            Compression::Raw => ImageReader::Raw(buf),
            Compression::Gzip => ImageReader::Gzip(flate2::bufread::MultiGzDecoder::new(buf)),
            Compression::Xz => ImageReader::Xz(xz2::bufread::XzDecoder::new_multi_decoder(buf)),
            Compression::Bzip2 => ImageReader::Bz2(bzip2::bufread::MultiBzDecoder::new(buf)),
            Compression::Zstd => ImageReader::Zstd(
                zstd::Decoder::with_buffer(buf).context("construct zstd decoder")?,
            ),
        })
    }
}

impl Read for ImageReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ImageReader::Raw(r) => r.read(buf),
            ImageReader::Gzip(r) => r.read(buf),
            ImageReader::Xz(r) => r.read(buf),
            ImageReader::Bz2(r) => r.read(buf),
            ImageReader::Zstd(r) => r.read(buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Compression methods --------------------------------------------

    #[test]
    fn is_compressed_distinguishes_raw_from_others() {
        assert!(!Compression::Raw.is_compressed());
        assert!(Compression::Gzip.is_compressed());
        assert!(Compression::Xz.is_compressed());
        assert!(Compression::Bzip2.is_compressed());
        assert!(Compression::Zstd.is_compressed());
    }

    #[test]
    fn label_is_stable_for_each_variant() {
        // Stability matters: this string appears in the operator-facing
        // confirmation prompt. A regression that changes it could break
        // log-scrapers or operator expectations.
        assert_eq!(Compression::Raw.label(), "raw");
        assert_eq!(Compression::Gzip.label(), "gzip");
        assert_eq!(Compression::Xz.label(), "xz");
        assert_eq!(Compression::Bzip2.label(), "bzip2");
        assert_eq!(Compression::Zstd.label(), "zstd");
    }

    // -- classify --------------------------------------------------------

    /// Gzip magic: `1F 8B`. Just two bytes are enough.
    #[test]
    fn classify_recognises_gzip() {
        assert_eq!(classify(&[0x1F, 0x8B]), Compression::Gzip);
        // With trailing flag/MTIME bytes that real gzip files carry.
        assert_eq!(classify(&[0x1F, 0x8B, 0x08, 0x00, 0, 0, 0, 0]), Compression::Gzip);
    }

    /// XZ magic: `FD 37 7A 58 5A 00`. All six bytes required.
    #[test]
    fn classify_recognises_xz() {
        assert_eq!(classify(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]), Compression::Xz);
        assert_eq!(classify(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00, 0x00, 0x04]), Compression::Xz);
    }

    /// Bzip2 magic: `42 5A 68` ("`BZh`").
    #[test]
    fn classify_recognises_bzip2() {
        assert_eq!(classify(&[0x42, 0x5A, 0x68]), Compression::Bzip2);
        assert_eq!(classify(&[0x42, 0x5A, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26]), Compression::Bzip2);
    }

    /// Zstd magic: `28 B5 2F FD`.
    #[test]
    fn classify_recognises_zstd() {
        assert_eq!(classify(&[0x28, 0xB5, 0x2F, 0xFD]), Compression::Zstd);
        assert_eq!(classify(&[0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x58, 0x35, 0x00]), Compression::Zstd);
    }

    /// Anything that doesn't match a known magic is `Raw`.
    /// This is a safety-relevant default: an unrecognised magic on a real
    /// ISO/IMG file means we treat it as raw bytes (correct), not panic.
    #[test]
    fn classify_defaults_to_raw_for_unknown() {
        assert_eq!(classify(&[]), Compression::Raw);
        assert_eq!(classify(&[0x00]), Compression::Raw);
        assert_eq!(classify(&[0xCD; 8]), Compression::Raw);
        // Real ISO9660 magic at offset 0x8001 — but our detect_compression
        // only peeks the first 8 bytes, where ISO9660 has no magic.
        // Treating ISO as raw is correct — we write the bytes verbatim.
        assert_eq!(classify(b"ISO9660"), Compression::Raw);
    }

    /// Truncated magic prefixes must not match. A 1-byte gzip magic
    /// (`1F` alone) is ambiguous and must be classified as Raw — better
    /// to write a 1-byte file as raw than to gzip-decode and crash.
    #[test]
    fn classify_rejects_truncated_magics() {
        assert_eq!(classify(&[0x1F]), Compression::Raw); // partial gzip
        assert_eq!(classify(&[0xFD, 0x37]), Compression::Raw); // partial xz
        assert_eq!(classify(&[0x42, 0x5A]), Compression::Raw); // partial bzip2
        assert_eq!(classify(&[0x28, 0xB5, 0x2F]), Compression::Raw); // partial zstd
        // Boundary truncations — each magic minus only its LAST byte.
        // The xz case is load-bearing: the 6-byte xz magic ends in 0x00,
        // so a 5-byte xz prefix followed by zero padding would
        // false-match if classification ever saw padded bytes (which is
        // why `detect_compression` slices to exactly the n bytes read)
        // or if the magic table were shortened by one byte.
        assert_eq!(classify(&[0xFD, 0x37, 0x7A, 0x58, 0x5A]), Compression::Raw);
        assert_eq!(classify(&[0x28, 0xB5, 0x2F]), Compression::Raw);
        assert_eq!(classify(&[0x1F]), Compression::Raw);
    }

    /// Magic-byte collision check: each format's first few bytes are
    /// distinct, so `starts_with` matching is unambiguous. This is partly
    /// redundant with the per-format `classify_recognises_*` tests (a
    /// prefix collision would cause one of those to fail), but stating
    /// the property explicitly here documents the design contract: when
    /// adding a new compression format, the magic must not be a prefix
    /// of an existing one or vice versa.
    #[test]
    fn classify_magics_are_pairwise_distinct() {
        let prefixes: &[(&[u8], Compression)] = &[
            (&[0x1F, 0x8B], Compression::Gzip),
            (&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00], Compression::Xz),
            (&[0x42, 0x5A, 0x68], Compression::Bzip2),
            (&[0x28, 0xB5, 0x2F, 0xFD], Compression::Zstd),
        ];
        for (i, (bytes, expected)) in prefixes.iter().enumerate() {
            assert_eq!(classify(bytes), *expected);
            for (j, (other, _)) in prefixes.iter().enumerate() {
                if i == j {
                    continue;
                }
                let n = bytes.len().min(other.len());
                assert_ne!(
                    &bytes[..n],
                    &other[..n],
                    "magic prefix collision between {bytes:?} and {other:?}"
                );
            }
        }
    }

    // -- read_full_best_effort ------------------------------------------

    /// EOF before the buffer is filled returns the partial count; this
    /// is what lets `detect_compression` work on a 3-byte file without
    /// crashing.
    #[test]
    fn read_full_best_effort_tolerates_short_files() {
        use std::io::Cursor;
        let mut r = Cursor::new(vec![0x42, 0x5A, 0x68]);
        let mut buf = [0_u8; 8];
        let n = read_full_best_effort(&mut r, &mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..3], &[0x42, 0x5A, 0x68]);
    }

    /// Empty file: returns 0 bytes, classifies to Raw downstream.
    #[test]
    fn read_full_best_effort_handles_empty_input() {
        use std::io::Cursor;
        let mut r = Cursor::new(Vec::<u8>::new());
        let mut buf = [0_u8; 8];
        let n = read_full_best_effort(&mut r, &mut buf).unwrap();
        assert_eq!(n, 0);
    }
    // ------------------------------------------------------------------
    // Multi-member / multi-stream decompression.
    //
    // These vectors are two independently-compressed members
    // concatenated: b"first-member-payload-"x3 ++ b"second-member-data-xx"x3
    // (126 bytes total). Multi-member archives are *valid* output of
    // real tools — pbzip2 is always multi-stream, pigz and
    // `cat a.gz b.gz` produce multi-member gzip — and the single-member
    // decoders stop at the first boundary, silently flashing a
    // truncated image that verification then blesses (it re-reads
    // through the same truncating decoder). Execution-verified against
    // loop devices before the fix; these tests pin the fix.
    // ------------------------------------------------------------------

    /// Expected concatenated payload for the MULTI_* vectors below.
    fn multi_expected() -> Vec<u8> {
        let mut v = b"first-member-payload-".repeat(3);
        v.extend_from_slice(&b"second-member-data-xx".repeat(3));
        v
    }

    /// Write `bytes` to a unique temp file and decode it in full via
    /// `ImageReader::open` with the given compression.
    fn decode_via_reader(bytes: &[u8], comp: Compression, tag: &str) -> Vec<u8> {
        let p = std::env::temp_dir().join(format!("imi-multi-{tag}-{}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        let mut r = ImageReader::open(&p, comp).unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        std::fs::remove_file(&p).unwrap();
        out
    }

    #[test]
    fn multi_member_gz_decodes_in_full() {
        const MULTI_GZ: &[u8] = &[
            0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x4B, 0xCB, 0x2C, 0x2A,
            0x2E, 0xD1, 0xCD, 0x4D, 0xCD, 0x4D, 0x4A, 0x2D, 0xD2, 0x2D, 0x48, 0xAC, 0xCC, 0xC9,
            0x4F, 0x4C, 0xD1, 0x4D, 0x23, 0x5A, 0x10, 0x00, 0xE5, 0x91, 0x2C, 0x7B, 0x3F, 0x00,
            0x00, 0x00, 0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x2B, 0x4E,
            0x4D, 0xCE, 0xCF, 0x4B, 0xD1, 0xCD, 0x4D, 0xCD, 0x4D, 0x4A, 0x2D, 0xD2, 0x4D, 0x49,
            0x2C, 0x49, 0xD4, 0xAD, 0xA8, 0x28, 0x26, 0x5A, 0x10, 0x00, 0xB3, 0x75, 0xB5, 0x39,
            0x3F, 0x00, 0x00, 0x00,
        ];
        assert_eq!(decode_via_reader(MULTI_GZ, Compression::Gzip, "gz"), multi_expected());
    }

    #[test]
    //#[cfg_attr(miri, ignore)] // MultiBzDecoder is C FFI (libbz2), like the xz/zst siblings
    fn multi_member_bz2_decodes_in_full() {
        const MULTI_BZ2: &[u8] = &[
            0x42, 0x5A, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x46, 0xD1, 0x48, 0x27,
            0x00, 0x00, 0x0E, 0x91, 0x80, 0x00, 0x02, 0x37, 0x26, 0xDC, 0x20, 0x20, 0x00, 0x23,
            0x3F, 0xF5, 0x52, 0x68, 0x61, 0x89, 0x0A, 0x60, 0x00, 0x2E, 0xA4, 0xA1, 0x29, 0x61,
            0x66, 0x59, 0x6D, 0xC4, 0x2E, 0xA5, 0x9D, 0xA9, 0x0E, 0x36, 0xFC, 0x5D, 0xC9, 0x14,
            0xE1, 0x42, 0x41, 0x1B, 0x45, 0x20, 0x9C, 0x42, 0x5A, 0x68, 0x39, 0x31, 0x41, 0x59,
            0x26, 0x53, 0x59, 0xC2, 0x33, 0xC2, 0x93, 0x00, 0x00, 0x1A, 0x91, 0x80, 0x00, 0x02,
            0x3E, 0x03, 0x9C, 0x40, 0x20, 0x00, 0x23, 0x3F, 0xF5, 0x54, 0x66, 0x86, 0xA7, 0xEA,
            0x82, 0x01, 0xA0, 0x0B, 0x53, 0x08, 0xF1, 0xC7, 0x16, 0xDA, 0x32, 0xB5, 0x32, 0x88,
            0xC3, 0xE4, 0x76, 0xA7, 0xE2, 0xEE, 0x48, 0xA7, 0x0A, 0x12, 0x18, 0x46, 0x78, 0x52,
            0x60,
        ];
        assert_eq!(decode_via_reader(MULTI_BZ2, Compression::Bzip2, "bz2"), multi_expected());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn multi_stream_xz_decodes_in_full() {
        const MULTI_XZ: &[u8] = &[
            0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00, 0x00, 0x04, 0xE6, 0xD6, 0xB4, 0x46, 0x02, 0x00,
            0x21, 0x01, 0x16, 0x00, 0x00, 0x00, 0x74, 0x2F, 0xE5, 0xA3, 0xE0, 0x00, 0x3E, 0x00,
            0x1C, 0x5D, 0x00, 0x33, 0x1A, 0x4A, 0xAC, 0x0C, 0x73, 0x21, 0xCF, 0xC4, 0x00, 0xA8,
            0xFE, 0x14, 0x3A, 0x51, 0x6E, 0x84, 0x22, 0x0C, 0x33, 0x18, 0xF6, 0xA4, 0x72, 0xE2,
            0x11, 0x00, 0x00, 0x00, 0x6E, 0x62, 0x74, 0xD8, 0x48, 0x04, 0xC7, 0x6B, 0x00, 0x01,
            0x38, 0x3F, 0xED, 0x24, 0x7F, 0x81, 0x1F, 0xB6, 0xF3, 0x7D, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x04, 0x59, 0x5A, 0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00, 0x00, 0x04, 0xE6, 0xD6,
            0xB4, 0x46, 0x02, 0x00, 0x21, 0x01, 0x16, 0x00, 0x00, 0x00, 0x74, 0x2F, 0xE5, 0xA3,
            0xE0, 0x00, 0x3E, 0x00, 0x1C, 0x5D, 0x00, 0x39, 0x99, 0x48, 0x91, 0xB1, 0x69, 0x96,
            0x5A, 0xF6, 0x3F, 0x96, 0x26, 0x25, 0x4C, 0xCE, 0xB6, 0xE3, 0x19, 0xF9, 0x6D, 0xAD,
            0xFA, 0x6E, 0xBB, 0x85, 0xCC, 0x00, 0x00, 0x00, 0x34, 0xAC, 0xE7, 0x18, 0x02, 0x15,
            0xB2, 0xEE, 0x00, 0x01, 0x38, 0x3F, 0xED, 0x24, 0x7F, 0x81, 0x1F, 0xB6, 0xF3, 0x7D,
            0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5A,
        ];
        assert_eq!(decode_via_reader(MULTI_XZ, Compression::Xz, "xz"), multi_expected());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn multi_member_zst_decodes_in_full() {
        const MULTI_ZST: &[u8] = &[
            0x28, 0xB5, 0x2F, 0xFD, 0x24, 0x3F, 0xDD, 0x00, 0x00, 0xA8, 0x66, 0x69, 0x72, 0x73,
            0x74, 0x2D, 0x6D, 0x65, 0x6D, 0x62, 0x65, 0x72, 0x2D, 0x70, 0x61, 0x79, 0x6C, 0x6F,
            0x61, 0x64, 0x2D, 0x01, 0x00, 0x23, 0x34, 0x99, 0x40, 0x9A, 0xBE, 0x15, 0x28, 0xB5,
            0x2F, 0xFD, 0x24, 0x3F, 0xDD, 0x00, 0x00, 0xA8, 0x73, 0x65, 0x63, 0x6F, 0x6E, 0x64,
            0x2D, 0x6D, 0x65, 0x6D, 0x62, 0x65, 0x72, 0x2D, 0x64, 0x61, 0x74, 0x61, 0x2D, 0x78,
            0x78, 0x01, 0x00, 0x23, 0x34, 0x99, 0xC7, 0x1F, 0x76, 0x1C,
        ];
        assert_eq!(decode_via_reader(MULTI_ZST, Compression::Zstd, "zst"), multi_expected());
    }
}
