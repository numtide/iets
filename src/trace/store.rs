//! The on-disk cache: one directory per key, holding one file that carries
//! every part of the entry. The file is written under a temporary name and
//! renamed into place, so one rename publishes the whole entry and a reader
//! that opened it holds one generation of it. `crate::cachefs` holds the
//! store-level structure (version, writer lock, sweeps) this shares with the
//! IR cache.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::cachefs;
use crate::trace::wire;

/// The one file of an entry directory.
pub const ENTRY: &str = "entry";
/// Leads the file; the key holds the build id, so an older binary's entries
/// are never read and this only has to catch bytes iets did not write.
const VERSION: u8 = 2;
/// The version byte, the number of parts, and the integrity check over the
/// key and the rest of the file.
const HEADER: usize = 2 + cachefs::SUM;
/// One part's place in the file: tag, offset, length.
const SLOT: usize = 17;

/// What an entry holds. Which parts exist depends on the subcommand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    /// The observations a hit validates.
    Readset,
    /// The stdout bytes of an `eval`, or the roots of a `build`.
    Result,
    /// Every derivation a `build` evaluation produced.
    Drvs,
    /// Where each source's bytes were.
    Sources,
    /// The lint warnings the run printed; a hit prints them again.
    Warnings,
}

impl Part {
    fn tag(self) -> u8 {
        match self {
            Part::Readset => 0,
            Part::Result => 1,
            Part::Drvs => 2,
            Part::Sources => 3,
            Part::Warnings => 4,
        }
    }
}

/// One generation of one entry: the bytes read from the file that was open
/// and where its parts are. Reading the whole file through one descriptor is
/// what makes the snapshot — a publish renames a new file over the name, and
/// this keeps the bytes it read — and the integrity check needs all of them
/// before any part is decoded.
pub struct Entry {
    data: Vec<u8>,
    slots: Vec<(u8, u64, u64)>,
}

impl Entry {
    /// `None` for any bytes that are not the entry this binary wrote for
    /// `key`: the caller discards it, as it discards a part that does not
    /// decode. The check covers the key, so an entry copied from another one
    /// is not an entry here either.
    fn parse(file: fs::File, len: u64, key: &str) -> Option<Entry> {
        let mut data = Vec::with_capacity(len as usize + 1);
        (&file).read_to_end(&mut data).ok()?;
        if data.len() < HEADER || data[0] != VERSION {
            return None;
        }
        if data[2..HEADER] != cachefs::checksum(key, &[&data[..2], &data[HEADER..]]) {
            return None;
        }
        let count = usize::from(data[1]);
        let payload = HEADER.checked_add(count.checked_mul(SLOT)?)?;
        let table = data.get(HEADER..payload)?;
        let mut slots = Vec::with_capacity(count);
        for s in table.as_chunks::<SLOT>().0 {
            let at = u64::from_le_bytes(s[1..9].try_into().ok()?);
            let n = u64::from_le_bytes(s[9..17].try_into().ok()?);
            if at < payload as u64 || at.checked_add(n)? > data.len() as u64 {
                return None;
            }
            slots.push((s[0], at, n));
        }
        Some(Entry { data, slots })
    }

    /// The entry file at `path`, for `iets cache dump`. The key is the name
    /// of the directory the entry lives in, which is how a store names it.
    pub fn open(path: &Path) -> Option<Entry> {
        let key = path.parent()?.file_name()?.to_str()?.to_string();
        let (f, md) = cachefs::open_entry(path)?;
        Entry::parse(f, md.len(), &key)
    }

    /// The bytes of one part, from this generation of the entry.
    pub fn part(&self, part: Part) -> Option<Vec<u8>> {
        let (_, at, n) = *self.slots.iter().find(|(t, ..)| *t == part.tag())?;
        let at = usize::try_from(at).ok()?;
        let n = usize::try_from(n).ok()?;
        Some(self.data.get(at..at.checked_add(n)?)?.to_vec())
    }
}

/// What a key names on disk.
pub enum Opened {
    /// No entry: a cold run, and nothing to report.
    None,
    /// An entry whose bytes are not one: poison and wipe.
    Bad,
    Entry(Entry),
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// `--cache-dir DIR`, else `IETS_CACHE_DIR`, else `$XDG_CACHE_HOME/iets`,
    /// else `~/.cache/iets`; the entries live in its `trace/` subdirectory,
    /// so the directory the user names is never a removal target. `None` when
    /// no home is known.
    pub fn locate(override_dir: Option<&str>) -> Option<Store> {
        Some(Store {
            root: cachefs::root(override_dir)?.join("trace"),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entry_dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Opens the entry once. Every part a run reads comes from the returned
    /// [`Entry`], so a sibling that publishes a new generation meanwhile
    /// cannot be read half.
    pub fn open(&self, key: &str) -> Opened {
        let path = self.entry_dir(key).join(ENTRY);
        let Some((f, md)) = cachefs::open_entry(&path) else {
            // no name is a cold run; a name that is there and is not one this
            // store can read is poison
            return match path.symlink_metadata() {
                Ok(_) => Opened::Bad,
                Err(_) => Opened::None,
            };
        };
        match Entry::parse(f, md.len(), key) {
            Some(e) => Opened::Entry(e),
            None => Opened::Bad,
        }
    }

    /// Publishes `parts` as the whole entry, in one rename under one hold of
    /// the store lock. `false` when a sibling writer held that lock: the
    /// entry stays the generation it was and the next run misses.
    pub fn publish(&self, key: &str, parts: &[(Part, &[u8])]) -> io::Result<bool> {
        let count = u8::try_from(parts.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let head = [VERSION, count];
        // an entry over the cap is one no reader will open (cachefs::MAX_ENTRY),
        // so write nothing rather than seconds of bytes that are dead on arrival
        let total = (HEADER + parts.len() * SLOT) as u64
            + parts.iter().map(|(_, b)| b.len() as u64).sum::<u64>();
        if total > cachefs::MAX_ENTRY {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!(
                    "cache entry is {total} bytes, over the {} byte limit",
                    cachefs::MAX_ENTRY
                ),
            ));
        }
        let mut table = Vec::with_capacity(parts.len() * SLOT);
        let mut at = (HEADER + parts.len() * SLOT) as u64;
        for (p, bytes) in parts {
            table.push(p.tag());
            table.extend_from_slice(&at.to_le_bytes());
            table.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            at += bytes.len() as u64;
        }
        let mut covered: Vec<&[u8]> = Vec::with_capacity(parts.len() + 2);
        covered.push(&head);
        covered.push(&table);
        covered.extend(parts.iter().map(|(_, b)| *b));
        let sum = cachefs::checksum(key, &covered);
        let mut chunks: Vec<&[u8]> = Vec::with_capacity(parts.len() + 3);
        chunks.push(&head);
        chunks.push(&sum);
        chunks.push(&table);
        chunks.extend(parts.iter().map(|(_, b)| *b));
        let dir = self.entry_dir(key);
        cachefs::create_dir(&dir)?;
        let Some(_lock) = cachefs::try_write_lock(&self.root)? else {
            return Ok(false);
        };
        cachefs::write_atomic(&dir, ENTRY, cachefs::tag(), &chunks).map(|()| true)
    }

    /// An entry that holds an answer and an empty read set: the answer
    /// follows from the key, so nothing can make it stale.
    pub fn keep(&self, key: &str, answer: &[u8]) {
        let none = wire::encode_readset(&[]);
        let _ = self.publish(key, &[(Part::Result, answer), (Part::Readset, &none)]);
    }

    /// The answer that `keep` put under `key`.
    pub fn kept(&self, key: &str) -> Option<Vec<u8>> {
        match self.open(key) {
            Opened::None => None,
            Opened::Bad => {
                self.discard(key);
                None
            }
            Opened::Entry(e) => {
                self.touch(key);
                e.part(Part::Result)
            }
        }
    }

    /// A hit marks the entry as used; renaming a file into the directory
    /// already does that, so a republished entry needs no touch.
    pub fn touch(&self, key: &str) {
        cachefs::touch(&self.entry_dir(key));
    }

    /// Poison and wipe, never repair: an entry that does not decode goes.
    pub fn discard(&self, key: &str) {
        let _ = fs::remove_dir_all(self.entry_dir(key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tempfile::tempdir;

    fn store(d: &Path) -> Store {
        assert!(cachefs::prepare(d, cachefs::Shape::Dirs, false));
        Store {
            root: d.to_path_buf(),
        }
    }

    const KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn one_rename_publishes_every_part() {
        let d = tempdir().unwrap();
        let s = store(d.path());
        assert!(matches!(s.open(KEY), Opened::None));
        assert!(
            s.publish(
                KEY,
                &[
                    (Part::Result, b"out".as_slice()),
                    (Part::Readset, b"obs".as_slice()),
                ],
            )
            .expect("publish")
        );
        let Opened::Entry(e) = s.open(KEY) else {
            panic!("the entry is there")
        };
        assert_eq!(e.part(Part::Readset).as_deref(), Some(b"obs".as_slice()));
        assert_eq!(e.part(Part::Result).as_deref(), Some(b"out".as_slice()));
        assert_eq!(e.part(Part::Drvs), None);
        assert_eq!(fs::read_dir(s.entry_dir(KEY)).expect("dir").count(), 1);
    }

    #[test]
    fn a_reader_of_one_generation_never_sees_the_next() {
        let d = tempdir().unwrap();
        let s = store(d.path());
        assert!(
            s.publish(
                KEY,
                &[
                    (Part::Readset, b"old obs".as_slice()),
                    (Part::Result, b"old out".as_slice()),
                ],
            )
            .expect("publish")
        );
        let Opened::Entry(e) = s.open(KEY) else {
            panic!("the entry is there")
        };
        assert_eq!(
            e.part(Part::Readset).as_deref(),
            Some(b"old obs".as_slice())
        );
        assert!(
            s.publish(
                KEY,
                &[
                    (Part::Readset, b"new observations".as_slice()),
                    (Part::Result, b"new out".as_slice()),
                    (Part::Warnings, b"w".as_slice()),
                ],
            )
            .expect("publish")
        );
        // the parts this run goes on to read are the ones it validated
        assert_eq!(e.part(Part::Result).as_deref(), Some(b"old out".as_slice()));
        assert_eq!(e.part(Part::Warnings), None);
        let Opened::Entry(next) = s.open(KEY) else {
            panic!("the entry is there")
        };
        assert_eq!(
            next.part(Part::Result).as_deref(),
            Some(b"new out".as_slice())
        );
    }

    #[rstest]
    #[case(b"")]
    #[case(b"\x01")]
    #[case(b"\x09\x01")]
    #[case(b"\x01\x04")]
    #[case(&[
        1, 1, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    ])]
    fn bytes_that_are_not_an_entry_are_bad_not_a_panic(#[case] bytes: &[u8]) {
        let d = tempdir().unwrap();
        let s = store(d.path());
        let dir = s.entry_dir(KEY);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(ENTRY), bytes).expect("entry");
        assert!(matches!(s.open(KEY), Opened::Bad));
    }
}
