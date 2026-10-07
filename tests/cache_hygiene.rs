//! What the two caches promise when many processes share one directory: no
//! torn entry is ever read, a killed writer leaves the store usable, a
//! corrupt entry or store goes instead of being repaired, and the sweep
//! removes only what the TTL says. Shapes follow nix-tarmac's
//! tests/{crash,concurrent,corrupt,gc}_test.cpp. See docs/explanation/caching.md.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, File, Permissions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use std::{env, thread};

use iets::cachefs;
use iets::fetch::spool;
use iets::trace::key::Key;
use iets::trace::run::Cache;
use iets::trace::store::{ENTRY, Opened, Part, Store};
use iets::trace::{Obs, wire};
use std::os::unix::fs::{PermissionsExt, symlink};

const KEYS: usize = 4;
const OBS: usize = 400;

fn tmp(tag: &str) -> PathBuf {
    let d = env::temp_dir().join(format!("iets-hygiene-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).expect("temp dir");
    d
}

fn key_of(i: usize) -> String {
    format!("{i:064x}")
}

/// One writer's read set for `key`: every hash is the writer's own byte, so a
/// reader that sees two writers' bytes in one entry has read a torn file.
fn payload(key: usize, writer: u8) -> Vec<u8> {
    let obs: Vec<Obs> = (0..OBS)
        .map(|i| Obs::Env {
            name: format!("IETS_TEST_{key}_{i}").into_bytes(),
            value: Some([writer; 32]),
        })
        .collect();
    wire::encode_readset(&obs)
}

/// One writer's whole entry. The parts have the writer's byte and a length
/// only that writer uses, so a reader that mixes generations sees it.
fn publish(store: &Store, key: usize, writer: u8) -> bool {
    let readset = payload(key, writer);
    let result = vec![writer; 700 + usize::from(writer) * 137];
    let warnings = vec![writer; 64 + usize::from(writer)];
    store
        .publish(
            &key_of(key),
            &[
                (Part::Warnings, &warnings),
                (Part::Result, &result),
                (Part::Readset, &readset),
            ],
        )
        .unwrap_or(false)
}

/// The writer of a part whose bytes are all its own.
fn writer_of(bytes: &[u8], what: &str) -> u8 {
    assert!(!bytes.is_empty(), "{what} is not empty");
    assert!(
        bytes.iter().all(|b| *b == bytes[0]),
        "one writer per {what}"
    );
    bytes[0]
}

/// `None` when the entry is absent; panics when it is present but incomplete,
/// or when its parts do not all come from one writer.
fn check(store: &Store, key: usize) -> Option<usize> {
    let e = match store.open(&key_of(key)) {
        Opened::None => return None,
        Opened::Bad => panic!("a published entry parses"),
        Opened::Entry(e) => e,
    };
    let bytes = e
        .part(Part::Readset)
        .expect("a published entry has a read set");
    let obs = wire::decode_readset(&bytes).expect("a published entry decodes");
    assert_eq!(obs.len(), OBS, "an entry holds every observation");
    let mut writers = HashSet::new();
    for (i, o) in obs.iter().enumerate() {
        let Obs::Env { name, value } = o else {
            panic!("wrong observation kind")
        };
        assert_eq!(name, format!("IETS_TEST_{key}_{i}").as_bytes());
        let v = value.as_ref().expect("a value");
        writers.insert(writer_of(v, "value"));
    }
    // a new result beside an old read set, or two
    // generations mixed by a sibling that published in between
    let result = e
        .part(Part::Result)
        .expect("a published entry has a result");
    writers.insert(writer_of(&result, "result"));
    let warnings = e
        .part(Part::Warnings)
        .expect("a published entry has warnings");
    writers.insert(writer_of(&warnings, "warnings"));
    assert_eq!(writers.len(), 1, "one writer per entry, never a mix");
    let w = usize::from(writers.into_iter().next().expect("a writer"));
    assert_eq!(result.len(), 700 + w * 137, "the result of that writer");
    assert_eq!(warnings.len(), 64 + w, "the warnings of that writer");
    Some(obs.len())
}

#[test]
fn concurrent_readers_and_writers_never_see_a_torn_entry() {
    let dir = tmp("concurrent");
    let store = Store::locate(Some(dir.to_str().expect("utf-8 path"))).expect("store");
    assert!(cachefs::prepare(store.root(), cachefs::Shape::Dirs, false));
    thread::scope(|s| {
        for w in 0..8u8 {
            let base = dir.clone();
            s.spawn(move || {
                let store = Store::locate(Some(base.to_str().expect("utf-8 path"))).expect("store");
                for round in 0..60 {
                    let key = (round + w as usize) % KEYS;
                    publish(&store, key, w + 1);
                    check(&store, (key + 1) % KEYS);
                }
            });
        }
    });
    for key in 0..KEYS {
        assert!(check(&store, key).is_some(), "every key ends published");
    }
}

/// The child of `a_killed_writer_leaves_the_store_usable`: writes entries as
/// fast as it can until SIGKILL. A no-op in a normal test run.
#[test]
fn crash_writer_child() {
    let Some(dir) = env::var_os("IETS_CRASH_DIR") else {
        return;
    };
    let store = Store::locate(Some(&dir.to_string_lossy())).expect("store");
    for i in 0..usize::MAX {
        let key = i % 64;
        publish(&store, key, u8::try_from(i % 8).expect("a writer") + 1);
    }
}

fn spawn_and_kill(dir: &Path, ms: u64) {
    let mut child = Command::new(env::current_exe().expect("this test binary"))
        .args(["crash_writer_child", "--exact", "--nocapture"])
        .env("IETS_CRASH_DIR", dir)
        .spawn()
        .expect("spawn the writer");
    thread::sleep(Duration::from_millis(ms));
    child.kill().expect("kill the writer");
    let status = child.wait().expect("reap the writer");
    assert!(!status.success(), "the writer died on the signal");
}

#[test]
fn a_killed_writer_leaves_the_store_usable() {
    let dir = tmp("crash");
    let store = Store::locate(Some(dir.to_str().expect("utf-8 path"))).expect("store");
    assert!(cachefs::prepare(store.root(), cachefs::Shape::Dirs, false));
    for round in 0..6 {
        spawn_and_kill(&dir, 20 + round * 10);
    }
    let mut published = 0;
    for key in 0..64 {
        published += usize::from(check(&store, key).is_some());
    }
    assert!(published > 0, "the writer published before it died");
    assert!(
        publish(&store, 1000, 9),
        "the next writer takes the lock a killed one held"
    );
}

fn hit_entry() -> Vec<u8> {
    wire::encode_readset(&[Obs::Env {
        name: b"IETS_NO_SUCH_VARIABLE".to_vec(),
        value: None,
    }])
}

/// A cache opened as a new process opens one: a lookup holds the generation
/// it opened, so a second lookup of changed bytes needs a new [`Cache`].
fn cached(path: &str) -> Cache {
    Cache::open(Some(path), Key::new("eval"), false).expect("cache")
}

fn put_hit(c: &Cache, readset: &[u8]) {
    assert!(
        c.store
            .publish(&c.key, &[(Part::Readset, readset)])
            .expect("publish")
    );
}

#[test]
fn a_corrupt_entry_is_dropped_and_a_corrupt_store_is_wiped() {
    let dir = tmp("corrupt");
    let path = dir.to_str().expect("utf-8 path");

    let c = cached(path);
    put_hit(&c, &hit_entry());
    assert!(c.lookup(), "a read set that validates is a hit");

    let entry = c.store.entry_dir(&c.key).join(ENTRY);
    let good = fs::read(&entry).expect("entry");
    fs::write(&entry, &good[..good.len() / 2]).expect("truncate");
    let c = cached(path);
    assert!(!c.lookup(), "a truncated entry misses");
    assert!(
        !c.store.entry_dir(&c.key).exists(),
        "and the entry is dropped, never repaired"
    );

    put_hit(&c, b"garbage");
    let c = cached(path);
    assert!(!c.lookup());
    assert!(!c.store.entry_dir(&c.key).exists());

    fs::create_dir_all(c.store.entry_dir(&c.key)).expect("entry dir");
    fs::write(&entry, b"not an entry at all").expect("entry");
    let c = cached(path);
    assert!(!c.lookup(), "bytes that are not an entry miss");
    assert!(!c.store.entry_dir(&c.key).exists());

    put_hit(&c, &hit_entry());
    fs::write(c.store.root().join("gc-stamp"), b"\x01not a time").expect("stamp");
    let c = cached(path);
    assert!(
        !c.store.entry_dir(&c.key).exists(),
        "an unreadable store-level file wipes the store"
    );
    put_hit(&c, &hit_entry());
    let c = cached(path);
    assert!(c.lookup(), "and the store works again");
}

/// The entry is checked against its own key before any part is read, so a
/// changed byte and an entry copied to another key are both a miss.
#[test]
fn a_changed_byte_or_a_copied_entry_is_a_miss() {
    let dir = tmp("integrity");
    let path = dir.to_str().expect("utf-8 path");
    let c = cached(path);
    put_hit(&c, &hit_entry());
    assert!(c.lookup(), "the entry it wrote is a hit");
    let entry = c.store.entry_dir(&c.key).join(ENTRY);
    let good = fs::read(&entry).expect("entry");

    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0xff;
        fs::create_dir_all(c.store.entry_dir(&c.key)).expect("entry dir");
        fs::write(&entry, &bad).expect("entry");
        let c = cached(path);
        assert!(!c.lookup(), "byte {i} changed");
        assert!(!c.store.entry_dir(&c.key).exists(), "and it is poisoned");
    }

    let other = key_of(7);
    fs::create_dir_all(c.store.entry_dir(&other)).expect("entry dir");
    fs::write(c.store.entry_dir(&other).join(ENTRY), &good).expect("entry");
    assert!(
        matches!(c.store.open(&other), Opened::Bad),
        "an entry under another key is not an entry there"
    );
}

/// Whoever runs iets can write the cache directory, so an entry name that is
/// a symbolic link, a FIFO or a length no entry can have is refused before
/// anything is read: an entry made by `truncate -s 1T` would abort the run on
/// the allocation.
#[test]
fn an_entry_that_is_not_a_plain_file_is_refused() {
    let dir = tmp("openrules");
    let good = dir.join("good");
    fs::write(&good, b"x").expect("file");
    assert!(cachefs::open_entry(&good).is_some());

    let link = dir.join("link");
    symlink(&good, &link).expect("link");
    assert!(cachefs::open_entry(&link).is_none(), "a symbolic link");

    let fifo = dir.join("fifo");
    let c = CString::new(fifo.as_os_str().as_encoded_bytes()).expect("path");
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(cachefs::open_entry(&fifo).is_none(), "a FIFO");

    let huge = dir.join("huge");
    let f = File::create(&huge).expect("file");
    f.set_len(cachefs::MAX_ENTRY + 1).expect("sparse length");
    drop(f);
    assert!(
        cachefs::open_entry(&huge).is_none(),
        "a length no entry has"
    );
    assert!(dir.join("huge").exists(), "and nothing was allocated");
}

/// The cache holds what an evaluation read, so its directories and files are
/// the user's whatever the umask, and an older cache root is tightened.
#[test]
fn a_cache_is_private_to_its_owner() {
    let dir = tmp("modes");
    let root = dir.join("trace");
    fs::create_dir_all(&root).expect("root");
    fs::set_permissions(&root, Permissions::from_mode(0o755)).expect("wide");
    assert!(cachefs::prepare(&root, cachefs::Shape::Dirs, false));
    let mode = |p: &Path| fs::metadata(p).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&root), 0o700, "an older root is tightened");
    assert_eq!(mode(&root.join("version")), 0o600);

    let store = Store::locate(Some(dir.to_str().expect("utf-8 path"))).expect("store");
    assert!(
        store
            .publish(&key_of(0), &[(Part::Readset, &hit_entry())])
            .expect("publish")
    );
    assert_eq!(mode(&store.entry_dir(&key_of(0))), 0o700);
    assert_eq!(mode(&store.entry_dir(&key_of(0)).join(ENTRY)), 0o600);
    assert_eq!(mode(&root.join("lock")), 0o600);
}

/// The child of `an_input_observation_marks_its_entry_as_used`: the input
/// store is under `XDG_CACHE_HOME`, so this runs in a process of its own.
#[test]
fn input_touch_child() {
    let Some(name) = env::var_os("IETS_INPUT_NAME") else {
        return;
    };
    let name = name.to_string_lossy().into_owned();
    // prepared first: a store with an entry and no version file is wiped
    let entry = iets::treestore::root().expect("input store").join(&name);
    fs::write(&entry, b"").expect("entry");
    let old = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
    File::options()
        .write(true)
        .open(&entry)
        .expect("entry")
        .set_modified(old)
        .expect("back-date");

    let mut obs = vec![Obs::Input {
        entry: name.into_bytes(),
        version: spool::INDEX_VERSION,
    }];
    iets::trace::validate::validate(&mut obs, 1).expect("the entry is there");
    let now = fs::metadata(&entry)
        .expect("stat")
        .modified()
        .expect("mtime");
    assert!(now > old, "a validated input observation marks the entry");
}

/// A hit reads nothing out of an input-store entry, so validating its
/// observation is what keeps the sweep from removing an input that is in use.
#[test]
fn an_input_observation_marks_its_entry_as_used() {
    let dir = tmp("inputtouch");
    let out = Command::new(env::current_exe().expect("this test binary"))
        .args(["input_touch_child", "--exact", "--nocapture"])
        .env("XDG_CACHE_HOME", &dir)
        .env("IETS_INPUT_NAME", "a3ck4d9zzk4l1r76rz5zv2wgycmpsy1y-source")
        .output()
        .expect("run the child");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn cache_gc_removes_entries_past_the_ttl() {
    let dir = tmp("gc");
    let path = dir.to_str().expect("utf-8 path");
    let store = Store::locate(Some(path)).expect("store");
    assert!(cachefs::prepare(store.root(), cachefs::Shape::Dirs, false));
    for key in 0..3 {
        assert!(
            store
                .publish(&key_of(key), &[(Part::Readset, &hit_entry())])
                .expect("publish")
        );
    }
    let ir_key = key_of(0);
    let ir_cache = iets::compile::cache::IrCache::locate(Some(path), false).expect("ir cache");
    ir_cache.write(&ir_key, b"ir entry");
    let ir = dir.join("ir").join(&ir_key[..2]);
    assert!(ir.join(&ir_key[2..]).exists(), "an IR entry to sweep");

    let gc = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_iets"))
            .args(["cache", "gc", "--cache-dir"])
            .arg(&dir)
            .args(args)
            .env("XDG_CONFIG_HOME", "/nonexistent")
            .output()
            .expect("run iets cache gc");
        assert!(out.status.success(), "cache gc exits 0");
        String::from_utf8(out.stdout).expect("utf-8 output")
    };

    let fresh = gc(&["--ttl", "30"]);
    assert!(fresh.contains("0 of 3 entries"), "{fresh}");
    assert!(fresh.contains("0 of 1 entries"), "{fresh}");

    let dry = gc(&["--ttl", "0", "--dry-run"]);
    assert!(
        dry.contains("3 of 3 entries") && dry.contains("(dry run)"),
        "{dry}"
    );
    assert!(
        matches!(store.open(&key_of(0)), Opened::Entry(_)),
        "a dry run keeps everything"
    );

    let done = gc(&["--ttl", "0"]);
    assert!(
        done.contains("3 of 3 entries") && done.contains("1 of 1 entries"),
        "{done}"
    );
    for key in 0..3 {
        assert!(
            matches!(store.open(&key_of(key)), Opened::None),
            "the entry is gone"
        );
    }
    assert!(!ir.join(&ir_key[2..]).exists(), "the IR store is swept too");
    assert!(
        store.root().join("version").exists(),
        "the store stays usable"
    );
}
