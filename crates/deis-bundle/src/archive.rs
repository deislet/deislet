//! Reading members out of a bundle archive, with a ceiling on every one.
//!
//! A zip says how large a member is, and that number is a claim made by the
//! file: deflate carries a run of one byte at about a thousand to one, so half
//! a megabyte on the wire can name half a gigabyte of member. Whoever uploads
//! a bundle is the least trusted party the platform has, and what they hand
//! over is expanded twice - once in the control plane at upload, and again on
//! every node that fetches the version.
//!
//! So the ceiling belongs here, in the reader, and not at the place that
//! happens to want a member. It used to be at the call sites: the static
//! assets were read against a running total (`deis-runtime`'s
//! `AssetBundle::from_archive`) while `manifest.json` and the entry module
//! were read with `read_to_string` and no ceiling at all. That is the shape of
//! the defect rather than the two members it was noticed on - a third caller
//! reading a third member would have had to remember, and remembering is what
//! nobody does. [`BundleArchive::read`] is now the only way to get bytes out
//! of a bundle, and it cannot be called without naming the [`Limit`] it is
//! held to.
//!
//! # Two checks, because a size is a claim and a claim can be a lie
//!
//! [`BundleArchive::open`] walks the central directory once - no member is
//! unpacked - and refuses an archive whose *declared* sizes add up past
//! [`Limit::TotalBytes`], which catches the ordinary bomb for the price of
//! reading a few thousand headers. A header that lies is caught by the second
//! check: every read stops one byte past its ceiling, so what a member
//! actually costs is bounded whatever its header said.
//!
//! # What is still linear in the bytes received
//!
//! `zip::ZipArchive::new` parses the whole central directory before anything
//! here can look at it, so an archive that is nothing but member headers costs
//! memory in proportion to its own size - a few times over, not a thousand
//! times over. The zip crate already refuses to pre-allocate for a file count
//! larger than the archive itself, and the control plane caps an upload body
//! (`deis_control::api::catalog::MAX_BUNDLE_BYTES`). Linear amplification with
//! a small constant is a cost; exponential amplification is an outage, and
//! that is what the ceilings below are for.

use std::collections::HashSet;
use std::io::{Cursor, Read};

use crate::{BundleError, Result};

/// How many members one bundle may hold.
///
/// The archive carries `manifest.json`, the entry module and one member per
/// static file, and the manifest has to list every one of those files by name,
/// so this is in practice the ceiling on a site's file count. Twenty thousand
/// is where the comparable hosted platforms put it, it is far past any site a
/// person maintains by hand, and it keeps the one-pass shape check below to a
/// few thousand header reads.
pub const MAX_MEMBERS: usize = 20_000;

/// Longest member name, in bytes.
///
/// A member name is an asset path, which becomes a URL path and a key in the
/// node's in-memory asset index. A kilobyte is longer than any file a build
/// produces and shorter than anything that could be used to make the index
/// itself expensive.
pub const MAX_MEMBER_NAME_BYTES: u64 = 1024;

/// Largest `manifest.json` this platform will unpack, in bytes.
///
/// The manifest lists every asset with its path, digest, content type and
/// size - on the order of two hundred bytes each - so [`MAX_MEMBERS`] files
/// come to roughly four megabytes. Eight leaves room for long paths, routes,
/// `_redirects` and `_headers` rules, and refuses a manifest no build could
/// have written.
pub const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

/// Largest entry module this platform will unpack, in bytes.
///
/// One bundled ESM file, which a node hands to V8 as source text and which has
/// to fit inside an isolate's heap several times over once compiled; that heap
/// is 256 MB by default (`deis-runtime`'s `default_memory_limit_mb`). Sixteen
/// megabytes is comfortably above the ten a comparable platform allows for a
/// worker script - room for an inline source map in a debug build - and a
/// sixteenth of the heap the module has to live in.
pub const MAX_ENTRY_BYTES: u64 = 16 * 1024 * 1024;

/// Largest total the members of one archive may declare, unpacked, in bytes.
///
/// Twice the assets a node holds in memory for one version by default
/// (`deis-runtime`'s `default_max_asset_bytes`, 128 MB), so no bundle a
/// default node could actually load is refused here, and the manifest and the
/// entry module fit in what is left. A node configured to hold more assets
/// than that is still held to this: the ceiling belongs to the bundle format,
/// which is one thing across the platform, rather than to whichever node
/// happens to fetch the version first.
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// One of the ceilings a bundle is held to.
///
/// An enum rather than a number per call site, so that a refusal can say which
/// ceiling was hit and what it is, and so that the numbers and the reasoning
/// for them live in one place. Everything that reads a member names one of
/// these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// [`MAX_MEMBERS`].
    Members,
    /// [`MAX_MEMBER_NAME_BYTES`].
    NameBytes,
    /// [`MAX_MANIFEST_BYTES`].
    ManifestBytes,
    /// [`MAX_ENTRY_BYTES`].
    EntryBytes,
    /// [`MAX_TOTAL_BYTES`].
    TotalBytes,
}

impl Limit {
    /// The number itself.
    pub fn ceiling(self) -> u64 {
        match self {
            Self::Members => MAX_MEMBERS as u64,
            Self::NameBytes => MAX_MEMBER_NAME_BYTES,
            Self::ManifestBytes => MAX_MANIFEST_BYTES,
            Self::EntryBytes => MAX_ENTRY_BYTES,
            Self::TotalBytes => MAX_TOTAL_BYTES,
        }
    }

    /// What the number counts, as a refusal says it out loud.
    pub fn measures(self) -> &'static str {
        match self {
            Self::Members => "the number of members in the archive",
            Self::NameBytes => "the length of a member name in bytes",
            Self::ManifestBytes => "the unpacked size of manifest.json in bytes",
            Self::EntryBytes => "the unpacked size of the entry module in bytes",
            Self::TotalBytes => "the unpacked size of everything in the archive, in bytes",
        }
    }
}

/// A bundle archive, opened and checked, and the only way to read a member.
///
/// Borrowed rather than owned bytes because every caller already holds the
/// whole bundle in memory - the control plane has just received it, a node has
/// just downloaded it - and a copy of a 64 MB upload per read would be the
/// cost this module exists to avoid.
pub struct BundleArchive<'a> {
    zip: zip::ZipArchive<Cursor<&'a [u8]>>,
}

/// Only what an error message would want: the bytes themselves are the
/// untrusted input this module exists to hold at arm's length, and printing
/// them would put a bundle in a log.
impl std::fmt::Debug for BundleArchive<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundleArchive")
            .field("members", &self.zip.len())
            .finish()
    }
}

impl<'a> BundleArchive<'a> {
    /// Open an archive and check its shape.
    ///
    /// The shape check is one pass over the central directory and unpacks
    /// nothing, so an archive that could never be honest is refused before a
    /// single member is decompressed.
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        let zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| BundleError::NotAnArchive(e.to_string()))?;
        let mut archive = Self { zip };
        archive.check_shape()?;
        Ok(archive)
    }

    /// Read one member, refusing it the moment it passes `limit`.
    ///
    /// `Ok(None)` means there is no member of that name, which every caller
    /// has its own thing to say about: a missing manifest is an archive that
    /// no deislet build produced, a missing entry module is a manifest that
    /// points at nothing.
    ///
    /// Nothing is allocated for what the header claims. The read stops one
    /// byte past the ceiling, which is enough to know the member is over it
    /// and is the most it can ever cost in memory.
    pub fn read(&mut self, name: &str, limit: Limit) -> Result<Option<Vec<u8>>> {
        let ceiling = limit.ceiling();
        let entry = match self.zip.by_name(name) {
            Ok(entry) => entry,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(e) => {
                return Err(BundleError::Malformed(format!(
                    "member {name:?} cannot be read out of the archive: {e}"
                )));
            }
        };

        let mut bytes = Vec::with_capacity(allocation_for(entry.size(), ceiling));
        let read = entry
            .take(ceiling.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| {
                BundleError::Malformed(format!("member {name:?} could not be unpacked: {e}"))
            })? as u64;

        if read > ceiling {
            // The true size is deliberately unknown: learning it would mean
            // unpacking the rest, which is exactly what a bomb wants.
            return Err(BundleError::TooLarge {
                limit,
                member: Some(name.to_string()),
                found: None,
            });
        }
        Ok(Some(bytes))
    }

    /// The same read for a member that has to be text.
    ///
    /// Both members this crate reads - the manifest and the entry module - are
    /// text by definition, and a caller that got bytes back would have to
    /// decide what to do about invalid UTF-8 on its own; two callers deciding
    /// separately is how one of them ends up not deciding.
    pub fn read_text(&mut self, name: &str, limit: Limit) -> Result<Option<String>> {
        let Some(bytes) = self.read(name, limit)? else {
            return Ok(None);
        };
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| BundleError::Malformed(format!("member {name:?} is not UTF-8: {e}")))
    }

    /// Refuse an archive that is out of shape before anything is unpacked.
    ///
    /// Three questions, all answered from the central directory: how many
    /// members there are, how long their names are, and how much they claim to
    /// unpack to in total. A fourth - whether two members share a name - is
    /// answered here as well, because the two would be one member to whoever
    /// asks for it by name and *which* one is a question with two answers. A
    /// bundle whose manifest the control plane checked and whose second
    /// manifest a node loaded would be the worst possible spelling of that.
    fn check_shape(&mut self) -> Result<()> {
        let members = self.zip.len();
        if members > MAX_MEMBERS {
            return Err(BundleError::TooLarge {
                limit: Limit::Members,
                member: None,
                found: Some(members as u64),
            });
        }

        let mut names: HashSet<String> = HashSet::with_capacity(members);
        let mut declared_total: u64 = 0;
        for index in 0..members {
            // `by_index_raw` reads the header and sets up no decompressor, so
            // this loop costs the directory and nothing else.
            let entry = self.zip.by_index_raw(index).map_err(|e| {
                BundleError::Malformed(format!("member {index} has an unreadable header: {e}"))
            })?;
            let name = entry.name().to_string();
            let declared = entry.size();
            drop(entry);

            if name.len() as u64 > MAX_MEMBER_NAME_BYTES {
                return Err(BundleError::TooLarge {
                    limit: Limit::NameBytes,
                    member: Some(truncate_for_message(&name)),
                    found: Some(name.len() as u64),
                });
            }

            declared_total = declared_total.saturating_add(declared);
            if declared_total > MAX_TOTAL_BYTES {
                return Err(BundleError::TooLarge {
                    limit: Limit::TotalBytes,
                    member: None,
                    found: Some(declared_total),
                });
            }

            if !names.insert(name.clone()) {
                return Err(BundleError::Malformed(format!(
                    "two members are both named {name:?}; which one a reader gets when it asks \
                     for that name is not decided anywhere, so the control plane could check one \
                     of them and a node load the other"
                )));
            }
        }
        Ok(())
    }
}

/// How much to reserve for one member up front.
///
/// The declared size is a hint from an untrusted file, so it is capped by the
/// ceiling - nothing past that is ever kept - and by a modest chunk, so that a
/// header claiming 20 GB costs one small allocation and a growing read rather
/// than 20 GB of address space on the first line. The same reasoning, and the
/// same number, as `deis-runtime`'s asset reader.
fn allocation_for(declared: u64, ceiling: u64) -> usize {
    /// Enough that an ordinary manifest or module is read in one or two
    /// allocations.
    const RESERVE_CEILING: u64 = 1024 * 1024;

    declared.min(ceiling).min(RESERVE_CEILING) as usize
}

/// A member name, cut down to something an error message can carry.
///
/// The name is what was too long, so it cannot be quoted whole into the very
/// message that says so.
fn truncate_for_message(name: &str) -> String {
    const KEEP: usize = 80;
    if name.len() <= KEEP {
        return name.to_string();
    }
    let mut cut = KEEP;
    while cut > 0 && !name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &name[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// An archive whose one member is `size` bytes of the same byte, which
    /// deflate carries in about a thousandth of the space. The bomb of the
    /// finding, in miniature.
    fn compressible_member(name: &str, size: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            w.start_file(
                name,
                zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
            w.write_all(&vec![b'a'; size]).unwrap();
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn a_member_over_its_ceiling_is_refused_without_being_unpacked() {
        let bytes = compressible_member("manifest.json", (MAX_MANIFEST_BYTES + 1) as usize);
        assert!(
            (bytes.len() as u64) < MAX_MANIFEST_BYTES / 100,
            "the fixture has to be small on the wire, or it proves nothing: {} bytes",
            bytes.len()
        );

        let mut archive = BundleArchive::open(&bytes).unwrap();
        let err = archive
            .read("manifest.json", Limit::ManifestBytes)
            .expect_err("a member over its ceiling must be refused");

        match err {
            BundleError::TooLarge {
                limit,
                ref member,
                found,
            } => {
                assert_eq!(limit, Limit::ManifestBytes);
                assert_eq!(member.as_deref(), Some("manifest.json"));
                assert_eq!(
                    found, None,
                    "learning the true size means unpacking the rest, which is what a bomb wants"
                );
            }
            other => panic!("{other}"),
        }
        // The refusal has to name the ceiling, because whoever reads it is
        // looking at a failed upload and cannot see this source.
        let message = err.to_string();
        assert!(
            message.contains(&MAX_MANIFEST_BYTES.to_string()),
            "{message}"
        );
        assert!(message.contains("manifest.json"), "{message}");
    }

    /// Rewrite the uncompressed size a zip declares for a member, in the
    /// local header and in the central directory, which is everywhere it is
    /// written down. Returns how many fields were rewritten so a test cannot
    /// pass because it patched nothing.
    fn declare_size(archive: &mut [u8], real: u32, lie: u32) -> usize {
        const LOCAL_HEADER: &[u8; 4] = b"PK\x03\x04";
        const DIRECTORY_HEADER: &[u8; 4] = b"PK\x01\x02";
        // Offset of the uncompressed-size field inside each of the two
        // headers, from the appendix of the zip specification.
        const IN_LOCAL: usize = 22;
        const IN_DIRECTORY: usize = 24;

        let mut patched = 0;
        for index in 0..archive.len().saturating_sub(IN_DIRECTORY + 4) {
            let offset = match &archive[index..index + 4] {
                header if header == LOCAL_HEADER => IN_LOCAL,
                header if header == DIRECTORY_HEADER => IN_DIRECTORY,
                _ => continue,
            };
            let field = index + offset;
            if u32::from_le_bytes(archive[field..field + 4].try_into().unwrap()) == real {
                archive[field..field + 4].copy_from_slice(&lie.to_le_bytes());
                patched += 1;
            }
        }
        patched
    }

    #[test]
    fn a_header_that_lies_about_its_size_is_still_stopped_at_the_ceiling() {
        // The one an archive's own numbers can never catch. The shape check
        // adds up what the members declare, so a member that declares a
        // hundred bytes and unpacks to sixteen megabytes walks straight past
        // it - and is stopped by the read, which does not believe the header
        // either.
        let real = (MAX_ENTRY_BYTES + 1) as usize;
        let mut bytes = compressible_member("worker/index.js", real);
        assert_eq!(
            declare_size(&mut bytes, real as u32, 100),
            2,
            "the fixture must actually rewrite both places a size is written"
        );

        let mut archive = BundleArchive::open(&bytes).expect("its own numbers look modest");
        let err = archive
            .read("worker/index.js", Limit::EntryBytes)
            .expect_err("what it unpacks to is what counts");
        assert!(
            matches!(
                err,
                BundleError::TooLarge {
                    limit: Limit::EntryBytes,
                    found: None,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_member_at_its_ceiling_is_read_whole() {
        // One byte under, so the boundary is a real boundary and not an
        // off-by-one that refuses honest bundles.
        let bytes = compressible_member("worker/index.js", (MAX_ENTRY_BYTES - 1) as usize);
        let mut archive = BundleArchive::open(&bytes).unwrap();
        let read = archive
            .read("worker/index.js", Limit::EntryBytes)
            .unwrap()
            .expect("the member is present");
        assert_eq!(read.len() as u64, MAX_ENTRY_BYTES - 1);
    }

    #[test]
    fn a_declared_total_over_the_ceiling_is_refused_before_anything_is_unpacked() {
        // Every member is honest here; there are simply too many of them.
        // Nothing is decompressed to find that out.
        let per_member = (MAX_TOTAL_BYTES / 8) as usize;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let body = vec![b'a'; per_member];
            for index in 0..9 {
                w.start_file(
                    format!("assets/{index}.bin"),
                    zip::write::FileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
                w.write_all(&body).unwrap();
            }
            w.finish().unwrap();
        }

        let err = BundleArchive::open(&buf).expect_err("nine eighths of the ceiling is over it");
        match err {
            BundleError::TooLarge {
                limit,
                member,
                found,
            } => {
                assert_eq!(limit, Limit::TotalBytes);
                assert_eq!(member, None, "no one member is at fault");
                assert!(found.unwrap() > MAX_TOTAL_BYTES);
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn too_many_members_are_refused() {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            for index in 0..=MAX_MEMBERS {
                w.start_file(
                    format!("assets/{index}"),
                    zip::write::FileOptions::default(),
                )
                .unwrap();
            }
            w.finish().unwrap();
        }
        let err = BundleArchive::open(&buf).expect_err("one past the ceiling");
        let message = err.to_string();
        assert!(message.contains(&MAX_MEMBERS.to_string()), "{message}");
        assert!(message.contains("members"), "{message}");
    }

    #[test]
    fn a_member_name_past_the_ceiling_is_refused_and_the_message_does_not_repeat_it() {
        let name = "a".repeat((MAX_MEMBER_NAME_BYTES + 1) as usize);
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            w.start_file(&name, zip::write::FileOptions::default())
                .unwrap();
            w.finish().unwrap();
        }
        let err = BundleArchive::open(&buf).expect_err("a name nothing could have produced");
        let message = err.to_string();
        assert!(
            message.len() < 400,
            "the name is what was too long, so it must not be quoted whole: {} chars",
            message.len()
        );
        assert!(
            message.contains(&MAX_MEMBER_NAME_BYTES.to_string()),
            "{message}"
        );
    }

    #[test]
    fn two_members_of_one_name_are_refused() {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            for body in ["first", "second"] {
                w.start_file("manifest.json", zip::write::FileOptions::default())
                    .unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        let err = BundleArchive::open(&buf).expect_err("one name, two members");
        assert!(err.to_string().contains("manifest.json"), "{err}");
    }

    #[test]
    fn a_missing_member_is_not_an_error_here() {
        let bytes = compressible_member("manifest.json", 16);
        let mut archive = BundleArchive::open(&bytes).unwrap();
        assert!(
            archive
                .read("worker/index.js", Limit::EntryBytes)
                .unwrap()
                .is_none(),
            "what to say about an absent member belongs to the caller that wanted it"
        );
    }
}
