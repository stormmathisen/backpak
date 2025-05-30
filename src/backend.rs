//! Places where we can make a backup repository - the local filesystem,
//! (eventually) cloud hosts, etc.

use std::fs::File;
use std::io::{self, prelude::*};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail, ensure};
use byte_unit::Byte;
use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use tracing::*;

use crate::{
    counters::{Op, bump},
    file_util::{move_opened, nice_size},
    hashing::ObjectId,
    pack, progress,
};

pub mod backblaze;
pub mod cache;
mod filter;
pub mod fs;
mod memory;
mod semaphored;

use cache::Cache;

#[inline]
fn defsize() -> Byte {
    pack::DEFAULT_PACK_SIZE
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Kind {
    Filesystem {
        force_cache: bool,
    },
    Backblaze {
        key_id: String,
        application_key: String,
        bucket: String,
        concurrent_connections: u32,
    }, // ...?
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default = "defsize")]
    pack_size: Byte,
    #[serde(rename = "backend")]
    kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    unfilter: Option<String>,
}

/// Normalized version of [`ConfigFile`] where `filter` and `unfilter` must both be Some or None.
#[derive(Debug)]
pub struct Configuration {
    pub pack_size: Byte,
    pub kind: Kind,
    pub filter: Option<(String, String)>,
}

pub fn read_config(p: &Utf8Path) -> Result<Configuration> {
    let s = std::fs::read_to_string(p).with_context(|| format!("Couldn't read config from {p}"))?;
    let cf: ConfigFile =
        toml::from_str(&s).with_context(|| format!("Couldn't parse config in {p}"))?;
    let filter = match (cf.filter, cf.unfilter) {
        (Some(f), Some(u)) => Some((f, u)),
        (None, None) => None,
        _ => bail!("{p} config should set `filter` and `unfilter` or neither."),
    };
    Ok(Configuration {
        pack_size: cf.pack_size,
        kind: cf.kind,
        filter,
    })
}

pub fn write_config<W: Write>(mut w: W, c: Configuration) -> Result<()> {
    let (filter, unfilter) = match c.filter {
        Some((f, u)) => (Some(f), Some(u)),
        None => (None, None),
    };
    let cf = ConfigFile {
        pack_size: c.pack_size,
        kind: c.kind,
        filter,
        unfilter,
    };
    w.write_all(toml::to_string(&cf)?.as_bytes())?;
    Ok(())
}

/// A backend is anything we can read from, write to, list, and remove items from.
pub trait Backend {
    /// Read from the given key
    fn read(&self, from: &str) -> Result<Box<dyn Read + Send + 'static>>;

    /// Write the given read stream to the given key
    fn write(&self, len: u64, from: &mut (dyn Read + Send), to: &str) -> Result<()>;

    fn remove(&self, which: &str) -> Result<()>;

    /// Lists all keys and their sizes with the given prefix
    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum CacheBehavior {
    /// Always write through to the backend,
    /// but backend reads are skipped if the entry is in-cache.
    Normal,
    /// Always write through to the backend **and**
    /// always read from the backend (and insert in the cache).
    /// Useful for commands like `check` where we want to ensure what's actually there.
    AlwaysRead,
}

/// Cached backends do what they say on the tin,
/// _or_ for the narrow case when we're writing unfiltered content to the filesystem,
/// a direct passthrough for that.
///
/// The backend is also responsible for unlinking the files
/// it's given once they're safely backed up.
/// (Bad separation of concerns? Perhaps. Convenient API? Yes.)
enum CachedBackendKind {
    /// Since a filesystem backend is, well, on the file system,
    /// we don't win anything by caching.
    /// Just read and write files directly. Nice.
    File {
        backend: fs::FilesystemBackend,
    },
    // The usual case: the backend is some remotely-hosted storage,
    // or local but the files are filtered first.
    // Here we can benefit from a write-through cache.
    Cached {
        cache: Cache,
        behavior: CacheBehavior,
        backend: Box<dyn Backend + Send + Sync>,
    },
    // Test backend please ignore
    Memory {
        backend: memory::MemoryBackend,
    },
}

pub struct CachedBackend {
    inner: CachedBackendKind,
    pub bytes_downloaded: AtomicU64,
    pub bytes_uploaded: AtomicU64,
}

impl CachedBackend {
    fn new(inner: CachedBackendKind) -> Self {
        Self {
            inner,
            bytes_downloaded: AtomicU64::new(0),
            bytes_uploaded: AtomicU64::new(0),
        }
    }
}

pub trait SeekableRead: Read + Seek + Send + 'static {}
impl<T> SeekableRead for T where T: Read + Seek + Send + 'static {}

// NB: We use a flat cache structure (where every file is just <hash>.pack/index/etc)
// but prepend prefixes with `destination()` prior to giving the path to the backend.
// (This allows prefix-based listing, which can save us a bunch on a big cloud store.)
impl CachedBackend {
    /// Read the object at the given key and return its file.
    fn read(&self, name: &str) -> Result<Box<dyn SeekableRead>> {
        match &self.inner {
            CachedBackendKind::File { backend } => {
                debug!("Loading {name}");
                bump(Op::BackendRead);
                let from = backend.path_of(&destination(name));
                let fd = File::open(&from).with_context(|| format!("Couldn't open {from}"))?;

                // Sorta - wrapping the file in AtomicCountRead would give us weird stuff
                // when seeking (or add a shitton of bookeeping to avoid said weird stuff).
                // Just add the file length on read.
                let len = fd.metadata()?.len();
                self.bytes_downloaded.fetch_add(len, Ordering::Relaxed); // sorta

                Ok(Box::new(fd))
            }
            CachedBackendKind::Cached {
                cache,
                behavior,
                backend,
            } => {
                let tr = if *behavior == CacheBehavior::AlwaysRead {
                    None
                } else {
                    cache.try_read(name)?
                };
                if let Some(hit) = tr {
                    debug!("Found {name} in the backend cache");
                    bump(Op::BackendCacheHit);
                    Ok(Box::new(hit))
                } else {
                    debug!("Downloading {name}");
                    bump(Op::BackendRead);
                    // NB: See backend::filter - we need this to drop _inside_
                    // cache.insert() lest its hokey "waiting on a process inside drop()"
                    // breaks things.
                    let counter = progress::AtomicCountRead::new(
                        backend.read(&destination(name))?,
                        &self.bytes_downloaded,
                    );
                    let mut inserted = cache.insert(name, counter)?;
                    cache.prune()?;
                    inserted.seek(io::SeekFrom::Start(0))?;
                    Ok(Box::new(inserted))
                }
            }
            CachedBackendKind::Memory { backend } => {
                debug!("Loading {name} (in-memory)");
                bump(Op::BackendRead);
                Ok(Box::new(backend.read_cursor(&destination(name))?))
            }
        }
    }

    /// Take the completed file and its `<id>.<type>` name and
    /// store it to an object with the appropriate key per
    /// `destination()`
    pub fn write(&self, name: &str, mut fh: File) -> Result<()> {
        bump(Op::BackendWrite);
        let len = fh.metadata()?.len();
        match &self.inner {
            CachedBackendKind::File { backend } => {
                debug!("Saving {name} ({})", nice_size(len));
                let to = backend.path_of(&destination(name));
                move_opened(name, fh, to)?;
                self.bytes_uploaded.fetch_add(len, Ordering::Relaxed);
            }
            CachedBackendKind::Cached { cache, backend, .. } => {
                // Write through!
                fh.seek(std::io::SeekFrom::Start(0))?;
                // Write it through to the backend.
                debug!("Uploading {name} ({})", nice_size(len));
                let mut counter = progress::AtomicCountRead::new(fh, &self.bytes_uploaded);
                backend.write(len, &mut counter, &destination(name))?;
                // Insert it into the cache.
                cache.insert_file(name, counter.into_inner())?;
                // Prune the cache.
                cache.prune()?;
            }
            CachedBackendKind::Memory { backend } => {
                debug!("Saving {name} ({}, in-memory)", nice_size(len));
                fh.seek(std::io::SeekFrom::Start(0))?;
                backend.write(len, &mut fh, &destination(name))?;
                self.bytes_uploaded.fetch_add(len, Ordering::Relaxed);
                std::fs::remove_file(name)?;
            }
        }
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<()> {
        debug!("Deleting {name}");
        bump(Op::BackendDelete);
        match &self.inner {
            CachedBackendKind::File { backend } => backend.remove(&destination(name)),
            CachedBackendKind::Cached { cache, backend, .. } => {
                // Remove it from the cache too.
                // No worries if it isn't there, no need to prune.
                cache.evict(name)?;
                backend.remove(&destination(name))?;
                Ok(())
            }
            CachedBackendKind::Memory { backend } => backend.remove(&destination(name)),
        }
    }

    // Let's put all the layout-specific stuff here so that we don't have paths
    // spread throughout the codebase.

    fn list(&self, which: &str) -> Result<Vec<(String, u64)>> {
        debug!("Querying backend for {which}*");
        match &self.inner {
            CachedBackendKind::File { backend } => backend.list(which),
            CachedBackendKind::Cached { backend, .. } => backend.list(which),
            CachedBackendKind::Memory { backend } => backend.list(which),
        }
    }

    pub fn list_indexes(&self) -> Result<Vec<(String, u64)>> {
        self.list("indexes/")
    }

    pub fn list_snapshots(&self) -> Result<Vec<(String, u64)>> {
        self.list("snapshots/")
    }

    pub fn list_packs(&self) -> Result<Vec<(String, u64)>> {
        self.list("packs/")
    }

    pub fn read_pack(&self, id: &ObjectId) -> Result<Box<dyn SeekableRead>> {
        let base32 = id.to_string();
        let pack_path = format!("{}.pack", base32);
        self.read(&pack_path)
            .with_context(|| format!("Couldn't open {}", pack_path))
    }

    pub fn read_index(&self, id: &ObjectId) -> Result<Box<dyn SeekableRead>> {
        let index_path = format!("{}.index", id);
        self.read(&index_path)
            .with_context(|| format!("Couldn't open {}", index_path))
    }

    pub fn read_snapshot(&self, id: &ObjectId) -> Result<Box<dyn SeekableRead>> {
        let snapshot_path = format!("{}.snapshot", id);
        self.read(&snapshot_path)
            .with_context(|| format!("Couldn't open {}", snapshot_path))
    }

    pub fn remove_pack(&self, id: &ObjectId) -> Result<()> {
        let base32 = id.to_string();
        let pack_path = format!("{}.pack", base32);
        self.remove(&pack_path)
    }

    pub fn remove_index(&self, id: &ObjectId) -> Result<()> {
        let index_path = format!("{}.index", id);
        self.remove(&index_path)
    }

    pub fn remove_snapshot(&self, id: &ObjectId) -> Result<()> {
        let snapshot_path = format!("{}.snapshot", id);
        self.remove(&snapshot_path)
    }
}

/// Given a list of packs, find one with the given ID or return an error.
pub fn probe_pack(packs: &[(String, u64)], id: &ObjectId) -> Result<()> {
    let base32 = id.to_string();
    let pack_path = format!("packs/{}.pack", base32);
    let found_packs: Vec<_> = packs
        .iter()
        .map(|(s, _len)| s)
        .filter(|s| **s == pack_path)
        .collect();
    match found_packs.len() {
        0 => bail!("Couldn't find pack {}", base32),
        1 => Ok(()),
        multiple => panic!(
            "Expected one pack at {}, found several! {:?}",
            pack_path, multiple
        ),
    }
}

/// Initializes an in-memory cache for testing purposes.
pub fn in_memory() -> CachedBackend {
    CachedBackend::new(CachedBackendKind::Memory {
        backend: memory::MemoryBackend::new(),
    })
}

/// Factory function to open the appropriate type of backend from the repository path
pub fn open(
    repository: &Utf8Path,
    cache_size: Byte,
    behavior: CacheBehavior,
) -> Result<(Configuration, CachedBackend)> {
    info!("Opening repository {repository}");
    let stat =
        std::fs::metadata(repository).with_context(|| format!("Couldn't stat {repository}"))?;
    let c = if stat.is_dir() {
        let cfg_file = repository.join("config.toml");
        read_config(&cfg_file)
    } else if stat.is_file() {
        read_config(repository)
    } else {
        bail!("{repository} is not a file or directory")
    }?;
    debug!("Read repository config: {c:?}");
    // Don't bother checking unfilter; we ensure both are set if one is above.
    let cached_backend = match &c.kind {
        Kind::Filesystem { force_cache: false } if c.filter.is_none() => {
            // Uncached filesystem backends are a special case
            // (they let us directly manipulate files.)
            CachedBackendKind::File {
                backend: fs::FilesystemBackend::open(repository)?,
            }
        }
        some_cached => {
            // It's not a filesystem backend, what is it?
            let mut backend: Box<dyn Backend + Send + Sync> = match some_cached {
                Kind::Filesystem { .. } => Box::new(fs::FilesystemBackend::open(repository)?),
                Kind::Backblaze {
                    key_id,
                    application_key,
                    bucket,
                    concurrent_connections,
                } => Box::new(semaphored::Semaphored::new(
                    backblaze::BackblazeBackend::open(key_id, application_key, bucket)?,
                    *concurrent_connections,
                )),
            };

            let cache = cache::setup(cache_size)?;

            if let Some((filter, unfilter)) = &c.filter {
                backend = Box::new(filter::BackendFilter {
                    filter: filter.clone(),
                    unfilter: unfilter.clone(),
                    raw: backend,
                });
            }

            CachedBackendKind::Cached {
                backend,
                behavior,
                cache,
            }
        }
    };
    let cached_backend = CachedBackend::new(cached_backend);
    Ok((c, cached_backend))
}

/// Returns the desitnation path for the given temp file based on its extension
fn destination(src: &str) -> String {
    match Utf8Path::new(src).extension() {
        Some("pack") => format!("packs/{}", src),
        Some("index") => format!("indexes/{}", src),
        Some("snapshot") => format!("snapshots/{}", src),
        _ => panic!("Unexpected extension on file: {}", src),
    }
}

/// Returns the ID of the object given its name
/// (assumed to be its `some/compontents/<Object ID>.<extension>`)
pub fn id_from_path<P: AsRef<Utf8Path>>(path: P) -> Result<ObjectId> {
    use std::str::FromStr;
    path.as_ref()
        .file_stem()
        .ok_or_else(|| anyhow!("Couldn't determine ID from {}", path.as_ref()))
        .and_then(ObjectId::from_str)
}
