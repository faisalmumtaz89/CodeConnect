//! What a release has to look like before any of it is trusted.
//!
//! `codeconnect update` downloads a published release and replaces the
//! installed binaries with it. Everything in this module runs *before* a single
//! byte is written to `~/.codeconnect/bin`, and every function here is pure: it
//! takes bytes and returns either a fact or a refusal.
//!
//! **Three checks, and they answer different questions.**
//!
//!   * The **checksum** answers "did this arrive intact". It comes from the
//!     same origin as the archive, so it catches a truncated download and
//!     nothing else. It is a precheck, not a trust boundary.
//!   * The **signature** answers "did CodeConnect build this" — see
//!     `update_install`. It is the only check that survives a hostile origin,
//!     and it is pinned to one Apple Team ID.
//!   * The **version** answers "is this the release we asked for". Without it a
//!     correctly signed *older* release could be served in place of the newest
//!     one, and every other check would pass.
//!
//! A release that fails any of the three is not installed, and nothing on disk
//! moves.

/// One file attached to a GitHub release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// The two files a release must carry to be installable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAssets {
    pub archive: Asset,
    pub checksum: Asset,
}

/// The largest archive worth downloading.
///
/// The three binaries are a few megabytes; the bound exists so a hostile or
/// broken origin cannot stream indefinitely into a temporary directory. Stated
/// generously — this is a backstop, not a size assertion.
pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;

/// The largest any single file in the archive may expand to, and the largest
/// they may come to together.
///
/// **A compressed bound is not an expanded bound.** A few megabytes of archive
/// can hold an enormous file, and extraction happens before any signature is
/// examined — so without this, four correctly named members could fill the disk
/// while every other check was still waiting its turn.
pub const MAX_MEMBER_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// What the archive is called for a given version.
pub fn archive_name(version: &str) -> String {
    format!("codeconnect-{version}-macos-universal.tar.gz")
}

/// What the checksum file is called for a given version.
pub fn checksum_name(version: &str) -> String {
    format!("{}.sha256", archive_name(version))
}

/// The binaries a release installs, in the order a reader would name them.
pub const SHIPPED_BINARIES: [&str; 3] = ["codeconnect", "ccd", "cc-hook"];

/// Pick this version's two assets out of a release, or say what is missing.
///
/// **Exactly the expected names, from the release's own asset list.** The
/// alternative — building a `releases/latest/download/…` URL — resolves at
/// download time rather than at selection time, so the bytes fetched could
/// belong to a release published between the two steps. Reading the URL out of
/// the same response that named the version binds them together.
pub fn select_assets(body: &[u8], version: &str) -> Result<ReleaseAssets, String> {
    #[derive(serde::Deserialize)]
    struct Release {
        assets: Vec<ReleaseAsset>,
    }
    #[derive(serde::Deserialize)]
    struct ReleaseAsset {
        name: String,
        browser_download_url: String,
        size: u64,
    }

    let release: Release = serde_json::from_slice(body)
        .map_err(|err| format!("the release listing could not be read: {err}"))?;

    let wanted = |name: &str| -> Result<Asset, String> {
        let mut matches = release.assets.iter().filter(|asset| asset.name == name);
        let found = matches
            .next()
            .ok_or_else(|| format!("the release does not carry {name}"))?;
        // Two assets of one name is a release nobody should install: the name
        // no longer says which bytes are meant.
        if matches.next().is_some() {
            return Err(format!("the release carries more than one {name}"));
        }
        if !found.browser_download_url.starts_with("https://") {
            return Err(format!("{name} is not offered over https"));
        }
        Ok(Asset {
            name: found.name.clone(),
            url: found.browser_download_url.clone(),
            size: found.size,
        })
    };

    let archive = wanted(&archive_name(version))?;
    if archive.size > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "{} is {} bytes, past the {MAX_ARCHIVE_BYTES}-byte ceiling",
            archive.name, archive.size
        ));
    }
    let checksum = wanted(&checksum_name(version))?;
    Ok(ReleaseAssets { archive, checksum })
}

/// Read the one digest a `shasum -a 256` file carries, for the file we asked
/// for.
///
/// **The name is checked, not just the digest.** A checksum file listing some
/// other archive's hash is not a checksum for this one, and a reader that took
/// the first 64 hex characters it saw would accept it.
pub fn parse_checksum(text: &str, expected_name: &str) -> Result<[u8; 32], String> {
    let mut found: Option<[u8; 32]> = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(name)) = (parts.next(), parts.next()) else {
            return Err("the checksum file is not `<digest>  <name>`".into());
        };
        if parts.next().is_some() {
            return Err("a checksum line carries more than a digest and a name".into());
        }
        // `shasum` writes `*name` for a binary-mode digest; both spellings name
        // the same file.
        let name = name.strip_prefix('*').unwrap_or(name);
        if name != expected_name {
            continue;
        }
        if found.is_some() {
            return Err(format!("the checksum file names {expected_name} twice"));
        }
        found = Some(decode_digest(digest)?);
    }
    found.ok_or_else(|| format!("the checksum file does not name {expected_name}"))
}

fn decode_digest(raw: &str) -> Result<[u8; 32], String> {
    if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the digest is not 64 hex characters".into());
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(raw.as_bytes().chunks_exact(2)) {
        let text = std::str::from_utf8(pair).map_err(|_| "the digest is not text".to_string())?;
        *slot = u8::from_str_radix(text, 16).map_err(|_| "the digest is not hex".to_string())?;
    }
    Ok(out)
}

/// One entry as the archive describes itself, before anything is unpacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub path: String,
    pub is_regular_file: bool,
    pub size: u64,
}

/// Every path an installable archive is allowed to contain.
pub fn expected_members(version: &str) -> Vec<String> {
    let root = format!("codeconnect-{version}");
    let mut names: Vec<String> = SHIPPED_BINARIES
        .iter()
        .map(|binary| format!("{root}/{binary}"))
        .collect();
    // The licence travels with the binaries because the licence says it must:
    // a binaries-only archive is a distribution without its notice.
    names.push(format!("{root}/LICENSE"));
    names
}

/// Whether an archive contains exactly what a release is allowed to contain.
///
/// **Judged whole, before extraction.** An implementation that validated each
/// entry as it wrote it would already have written the entries before the one
/// that fails — and the last entry is where anything hostile would be put.
pub fn validate_members(members: &[Member], version: &str) -> Result<(), String> {
    let expected = expected_members(version);
    let root = format!("codeconnect-{version}/");

    for member in members {
        // A path with a traversal, a root, or a backslash is not a path this
        // archive is allowed to name, whatever the extractor would do with it.
        if member.path.starts_with('/')
            || member.path.split('/').any(|part| part == "..")
            || member.path.contains('\\')
            || member.path.contains('\0')
        {
            return Err(format!("the archive names an unsafe path: {}", member.path));
        }
        if !member.path.starts_with(&root) {
            return Err(format!(
                "the archive holds {} outside codeconnect-{version}/",
                member.path
            ));
        }
        // Anything that is not a plain file is refused rather than skipped: a
        // symlink or a device node is not something a release needs, so its
        // presence says the archive is not the one we mean.
        if !member.is_regular_file {
            return Err(format!("{} is not a regular file", member.path));
        }
        if !expected.contains(&member.path) {
            return Err(format!(
                "the archive holds an unexpected file: {}",
                member.path
            ));
        }
    }

    let mut total: u64 = 0;
    for member in members {
        if member.size > MAX_MEMBER_BYTES {
            return Err(format!(
                "{} expands to {} bytes, past the per-file ceiling",
                member.path, member.size
            ));
        }
        total = total.saturating_add(member.size);
    }
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "the archive expands to {total} bytes, past the {MAX_TOTAL_BYTES}-byte ceiling"
        ));
    }

    for name in &expected {
        let seen = members.iter().filter(|m| &m.path == name).count();
        if seen == 0 {
            return Err(format!("the archive is missing {name}"));
        }
        // A duplicated entry means the file that lands is whichever was
        // written last, which is not a thing anyone verified.
        if seen > 1 {
            return Err(format!("the archive holds {name} more than once"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(assets: &[(&str, &str, u64)]) -> Vec<u8> {
        let rendered: Vec<String> = assets
            .iter()
            .map(|(name, url, size)| {
                format!(r#"{{"name":"{name}","browser_download_url":"{url}","size":{size}}}"#)
            })
            .collect();
        format!(
            r#"{{"tag_name":"v1.2.3","assets":[{}]}}"#,
            rendered.join(",")
        )
        .into_bytes()
    }

    fn both(version: &str) -> Vec<u8> {
        listing(&[
            (
                &archive_name(version),
                "https://example.invalid/a.tar.gz",
                4096,
            ),
            (
                &checksum_name(version),
                "https://example.invalid/a.sha256",
                90,
            ),
        ])
    }

    #[test]
    fn a_release_carrying_both_assets_yields_their_urls() {
        let picked = select_assets(&both("1.2.3"), "1.2.3").expect("both are present");
        assert_eq!(
            picked.archive.name,
            "codeconnect-1.2.3-macos-universal.tar.gz"
        );
        assert_eq!(picked.archive.url, "https://example.invalid/a.tar.gz");
        assert_eq!(
            picked.checksum.name,
            "codeconnect-1.2.3-macos-universal.tar.gz.sha256"
        );
    }

    /// The version is part of the name, so a release's assets cannot satisfy a
    /// different version's request.
    #[test]
    fn assets_for_another_version_do_not_satisfy_this_one() {
        let err = select_assets(&both("1.2.3"), "1.2.4").unwrap_err();
        assert!(
            err.contains("codeconnect-1.2.4-macos-universal.tar.gz"),
            "{err}"
        );
    }

    #[test]
    fn a_release_missing_the_checksum_is_refused_by_name() {
        let only_archive = listing(&[(
            &archive_name("1.2.3"),
            "https://example.invalid/a.tar.gz",
            4096,
        )]);
        let err = select_assets(&only_archive, "1.2.3").unwrap_err();
        assert!(err.contains(".sha256"), "{err}");
    }

    /// Two assets of one name is a release that no longer says which bytes it
    /// means.
    #[test]
    fn a_duplicated_asset_name_is_refused() {
        let doubled = listing(&[
            (&archive_name("1.2.3"), "https://example.invalid/a", 10),
            (&archive_name("1.2.3"), "https://example.invalid/b", 10),
            (&checksum_name("1.2.3"), "https://example.invalid/c", 90),
        ]);
        assert!(select_assets(&doubled, "1.2.3")
            .unwrap_err()
            .contains("more than one"));
    }

    #[test]
    fn a_plaintext_download_url_is_refused() {
        let insecure = listing(&[
            (
                &archive_name("1.2.3"),
                "http://example.invalid/a.tar.gz",
                10,
            ),
            (&checksum_name("1.2.3"), "https://example.invalid/c", 90),
        ]);
        assert!(select_assets(&insecure, "1.2.3")
            .unwrap_err()
            .contains("https"));
    }

    #[test]
    fn an_archive_past_the_ceiling_is_refused_before_it_is_fetched() {
        let huge = listing(&[
            (
                &archive_name("1.2.3"),
                "https://example.invalid/a.tar.gz",
                MAX_ARCHIVE_BYTES + 1,
            ),
            (&checksum_name("1.2.3"), "https://example.invalid/c", 90),
        ]);
        assert!(select_assets(&huge, "1.2.3")
            .unwrap_err()
            .contains("ceiling"));
    }

    // ------------------------------------------------------------- checksum

    const DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn the_digest_for_the_named_file_is_the_one_returned() {
        let file = format!("{DIGEST}  codeconnect-1.2.3-macos-universal.tar.gz\n");
        let parsed = parse_checksum(&file, "codeconnect-1.2.3-macos-universal.tar.gz").unwrap();
        assert_eq!(parsed[0], 0xe3);
        assert_eq!(parsed[31], 0x55);
    }

    /// A checksum file that names some other archive is not a checksum for
    /// this one, however well formed it is.
    #[test]
    fn a_digest_for_another_file_is_not_accepted() {
        let file = format!("{DIGEST}  some-other-archive.tar.gz\n");
        assert!(parse_checksum(&file, "codeconnect-1.2.3-macos-universal.tar.gz").is_err());
    }

    #[test]
    fn binary_mode_and_several_lines_are_read_correctly() {
        let file =
            format!("{DIGEST}  other.tar.gz\n{DIGEST} *codeconnect-1.2.3-macos-universal.tar.gz\n");
        assert!(parse_checksum(&file, "codeconnect-1.2.3-macos-universal.tar.gz").is_ok());
    }

    #[test]
    fn a_malformed_digest_is_refused() {
        for bad in [
            "not-a-digest  codeconnect-1.2.3-macos-universal.tar.gz",
            "e3b0  codeconnect-1.2.3-macos-universal.tar.gz",
            "zzzzc44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  codeconnect-1.2.3-macos-universal.tar.gz",
        ] {
            assert!(
                parse_checksum(bad, "codeconnect-1.2.3-macos-universal.tar.gz").is_err(),
                "accepted {bad}"
            );
        }
    }

    #[test]
    fn one_file_named_twice_is_refused_rather_than_resolved() {
        let file = format!(
            "{DIGEST}  codeconnect-1.2.3-macos-universal.tar.gz\n\
             {DIGEST}  codeconnect-1.2.3-macos-universal.tar.gz\n"
        );
        assert!(
            parse_checksum(&file, "codeconnect-1.2.3-macos-universal.tar.gz")
                .unwrap_err()
                .contains("twice")
        );
    }

    // -------------------------------------------------------------- archive

    fn good(version: &str) -> Vec<Member> {
        expected_members(version)
            .into_iter()
            .map(|path| Member {
                path,
                is_regular_file: true,
                size: 1024,
            })
            .collect()
    }

    #[test]
    fn an_archive_holding_exactly_the_expected_files_is_accepted() {
        assert!(validate_members(&good("1.2.3"), "1.2.3").is_ok());
    }

    /// The licence ships with the binaries because the licence requires it.
    #[test]
    fn an_archive_without_the_licence_is_refused() {
        let mut members = good("1.2.3");
        members.retain(|m| !m.path.ends_with("/LICENSE"));
        assert!(validate_members(&members, "1.2.3")
            .unwrap_err()
            .contains("LICENSE"));
    }

    #[test]
    fn a_missing_binary_is_refused() {
        for binary in SHIPPED_BINARIES {
            let mut members = good("1.2.3");
            members.retain(|m| !m.path.ends_with(&format!("/{binary}")));
            assert!(
                validate_members(&members, "1.2.3").is_err(),
                "accepted an archive with no {binary}"
            );
        }
    }

    /// **The hostile entry goes last on purpose.** An extractor that validated
    /// as it wrote would already have written the three good files.
    #[test]
    fn an_unsafe_path_is_refused_wherever_it_sits() {
        for path in [
            "../escape",
            "/etc/passwd",
            "codeconnect-1.2.3/../../escape",
            "codeconnect-1.2.3/sub\\dir",
        ] {
            let mut members = good("1.2.3");
            members.push(Member {
                path: path.to_string(),
                is_regular_file: true,
                size: 1,
            });
            assert!(
                validate_members(&members, "1.2.3").is_err(),
                "accepted {path}"
            );
        }
    }

    #[test]
    fn anything_that_is_not_a_plain_file_is_refused() {
        let mut members = good("1.2.3");
        members[0].is_regular_file = false;
        assert!(validate_members(&members, "1.2.3")
            .unwrap_err()
            .contains("not a regular file"));
    }

    /// A compressed bound is not an expanded bound, and extraction happens
    /// before any signature is examined.
    #[test]
    fn an_archive_that_expands_past_the_ceiling_is_refused() {
        let mut members = good("1.2.3");
        members[0].size = MAX_MEMBER_BYTES + 1;
        assert!(validate_members(&members, "1.2.3")
            .unwrap_err()
            .contains("per-file ceiling"));

        let mut members = good("1.2.3");
        for member in members.iter_mut() {
            member.size = MAX_TOTAL_BYTES / 3;
        }
        assert!(validate_members(&members, "1.2.3")
            .unwrap_err()
            .contains("past the"));
    }

    #[test]
    fn an_extra_file_is_refused_rather_than_ignored() {
        let mut members = good("1.2.3");
        members.push(Member {
            path: "codeconnect-1.2.3/install.sh".into(),
            is_regular_file: true,
            size: 12,
        });
        assert!(validate_members(&members, "1.2.3")
            .unwrap_err()
            .contains("unexpected"));
    }

    /// Whichever copy lands is whichever was written last, and that is not the
    /// copy anyone verified.
    #[test]
    fn a_duplicated_member_is_refused() {
        let mut members = good("1.2.3");
        members.push(members[0].clone());
        assert!(validate_members(&members, "1.2.3")
            .unwrap_err()
            .contains("more than once"));
    }

    #[test]
    fn files_outside_the_versioned_directory_are_refused() {
        let mut members = good("1.2.3");
        members.push(Member {
            path: "codeconnect-9.9.9/ccd".into(),
            is_regular_file: true,
            size: 1,
        });
        assert!(validate_members(&members, "1.2.3").is_err());
    }
}
