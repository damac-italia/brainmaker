// SPDX-License-Identifier: GPL-3.0-or-later

//! Zip extraction with the checks that a network-supplied archive requires.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{MAX_ENTRY_BYTES, MAX_TOTAL_BYTES};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
    /// Entries that we refused: an unsafe path, or a symbolic link.
    pub skipped: usize,
}

/// Extracts `archive` into `dest`.
///
/// `dest` must not exist, or must be empty. The function applies four checks
/// against a hostile archive:
///
/// 1. It rejects any entry whose path escapes `dest`, including `..` segments
///    and absolute paths.
/// 2. It rejects symbolic links, which can point outside `dest` after the
///    extraction.
/// 3. It stops any single entry at [`MAX_ENTRY_BYTES`].
/// 4. It stops the whole archive at [`MAX_TOTAL_BYTES`].
pub fn extract(archive: &Path, dest: &Path) -> Result<Stats> {
    let file = File::open(archive)
        .with_context(|| format!("cannot open the archive {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("{} is not a valid zip archive", archive.display()))?;

    fs::create_dir_all(dest)
        .with_context(|| format!("cannot create the directory {}", dest.display()))?;

    let mut stats = Stats::default();

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .with_context(|| format!("cannot read entry {index} of {}", archive.display()))?;

        // `enclosed_name` returns None when the path escapes the destination.
        let Some(relative) = entry.enclosed_name() else {
            stats.skipped += 1;
            continue;
        };

        if entry.is_symlink() {
            stats.skipped += 1;
            continue;
        }

        let target = dest.join(&relative);

        if entry.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("cannot create the directory {}", target.display()))?;
            stats.directories += 1;
            continue;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create the directory {}", parent.display()))?;
        }

        let remaining = MAX_TOTAL_BYTES.saturating_sub(stats.bytes);
        if remaining == 0 {
            bail!(
                "the archive {} expands past the limit of {MAX_TOTAL_BYTES} bytes",
                archive.display()
            );
        }
        let cap = MAX_ENTRY_BYTES.min(remaining);

        let out =
            File::create(&target).with_context(|| format!("cannot create {}", target.display()))?;
        let mut writer = BufWriter::new(out);
        // Read one byte past the cap so that an oversized entry fails instead
        // of being silently truncated.
        let mut reader = (&mut entry).take(cap + 1);
        let written = io::copy(&mut reader, &mut writer)
            .with_context(|| format!("cannot write {}", target.display()))?;
        writer
            .into_inner()
            .with_context(|| format!("cannot flush {}", target.display()))?;

        if written > cap {
            bail!(
                "the entry {} of {} is larger than the remaining limit of {cap} bytes",
                relative.display(),
                archive.display()
            );
        }

        stats.bytes += written;
        stats.files += 1;

        apply_mode(&target, entry.unix_mode())?;
    }

    Ok(stats)
}

/// Copies the archived permission bits, after it removes the bits that a
/// hostile archive can abuse.
///
/// The function discards setuid, setgid, and the sticky bit. It also discards
/// the group-write bit and the other-write bit, so that another local account
/// cannot alter the content after the extraction. It always keeps the owner
/// able to read and write the file.
#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let Some(mode) = mode else {
        return Ok(());
    };
    let mode = (mode & 0o755) | 0o600;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("cannot set the permissions of {}", path.display()))
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brainmaker-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes a zip whose entries are the given (name, contents) pairs.
    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        for (name, contents) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn extracts_files_and_nested_directories() {
        let dir = temp_dir("extract-ok");
        let archive = dir.join("content.zip");
        write_zip(
            &archive,
            &[("notes.md", b"# notes"), ("sub/deep/file.txt", b"hello")],
        );

        let dest = dir.join("out");
        let stats = extract(&archive, &dest).unwrap();

        assert_eq!(stats.files, 2);
        assert_eq!(stats.skipped, 0);
        assert_eq!(
            fs::read_to_string(dest.join("notes.md")).unwrap(),
            "# notes"
        );
        assert_eq!(
            fs::read_to_string(dest.join("sub/deep/file.txt")).unwrap(),
            "hello"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skips_an_entry_that_escapes_the_destination() {
        let dir = temp_dir("extract-escape");
        let archive = dir.join("content.zip");
        write_zip(
            &archive,
            &[
                ("../escaped.txt", b"no"),
                ("../../escaped-twice.txt", b"no"),
                ("safe.txt", b"yes"),
            ],
        );

        let dest = dir.join("out");
        let stats = extract(&archive, &dest).unwrap();

        assert_eq!(stats.files, 1);
        assert_eq!(stats.skipped, 2);
        assert!(dest.join("safe.txt").exists());
        assert!(!dir.join("escaped.txt").exists());
        assert!(!dir.parent().unwrap().join("escaped-twice.txt").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reports_an_empty_archive_as_zero_files() {
        let dir = temp_dir("extract-empty");
        let archive = dir.join("content.zip");
        write_zip(&archive, &[]);

        let dest = dir.join("out");
        let stats = extract(&archive, &dest).unwrap();

        assert_eq!(stats.files, 0);
        assert!(dest.is_dir());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn strips_the_group_write_and_other_write_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("extract-mode");
        let archive = dir.join("content.zip");

        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file(
                "wide-open.sh",
                SimpleFileOptions::default().unix_permissions(0o777),
            )
            .unwrap();
        writer.write_all(b"#!/bin/sh\n").unwrap();
        writer.finish().unwrap();

        let dest = dir.join("out");
        extract(&archive, &dest).unwrap();

        let mode = fs::metadata(dest.join("wide-open.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755, "got {mode:o}, expected 755");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fails_on_a_file_that_is_not_a_zip() {
        let dir = temp_dir("extract-bad");
        let archive = dir.join("content.zip");
        fs::write(&archive, b"this is not a zip archive").unwrap();

        let dest = dir.join("out");
        assert!(extract(&archive, &dest).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }
}
