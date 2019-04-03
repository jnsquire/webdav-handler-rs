use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{PathBuf,Path};
use std::sync::Arc;

use lru::LruCache;
use parking_lot::Mutex;

lazy_static! {
    static ref CACHE: Arc<Cache> = Arc::new(Cache::new(1024));
}

// Do a case-insensitive path lookup.
pub(crate) fn resolve<'a>(base: impl Into<PathBuf>, path: &[u8], case_insensitive: bool) -> PathBuf {
    let base = base.into();
    let mut path = Path::new(OsStr::from_bytes(path));

    // deref in advance: first lazy_static, then Arc.
    let cache = &*(&*CACHE);

    // make "path" relative.
    while path.starts_with("/") {
        path = match path.strip_prefix("/") {
            Ok(p) => p,
            Err(_) => break,
        };
    }

    // if not case-mangling, return now.
    if !case_insensitive {
        let mut newpath = base;
        newpath.push(&path);
        return newpath;
    }

    // must be rooted, and valid UTF-8.
    let mut fullpath = base.clone();
    fullpath.push(&path);
    if !fullpath.has_root() || fullpath.to_str().is_none() {
        return fullpath;
    }

    // must have a parent.
    let parent = match fullpath.parent() {
        Some(p) => p,
        None => return fullpath,
    };

    // In the cache?
    if let Some((path, _)) = cache.get(&fullpath) {
        return path;
    }

    // if the file exists, fine.
    if fullpath.metadata().is_ok() {
        return fullpath;
    }

    // we need the path as a list of segments.
    let segs = path.iter().collect::<Vec<_>>();
    if segs.len() == 0 {
        return fullpath;
    }

    // if the parent exists, do a lookup there straight away
    // instead of starting from the root.
    let (parent, parent_exists) = if segs.len() > 1 {
        match cache.get(parent) {
            Some((path, _)) => (path, true),
            None => {
                let exists = parent.exists();
                if exists {
                    cache.insert(parent);
                }
                (parent.to_path_buf(), exists)
            },
        }
    } else {
        (parent.to_path_buf(), true)
    };
    if parent_exists {
        let (newpath, stop) = lookup(parent, segs[segs.len() - 1], true);
        if !stop {
            cache.insert(&newpath);
        }
        return newpath;
    }

    // start from the root, then add segments one by one.
    let mut stop = false;
    let mut newpath = base;
    let lastseg = segs.len() - 1;
    for (idx, seg) in segs.into_iter().enumerate() {
        if !stop {
            if idx == lastseg {
                // Save the path leading up to this file or dir.
                cache.insert(&newpath);
            }
            let (n, s) = lookup(newpath, seg, false);
            newpath = n;
            stop = s;
        } else {
            newpath.push(seg);
        }
    }
    if !stop {
        // resolved succesfully. save in cache.
        cache.insert(&newpath);
    }
    newpath
}

// lookup a filename in a directory in a case insensitive way.
fn lookup(mut path: PathBuf, seg: &OsStr, no_init_check: bool) -> (PathBuf, bool) {

    // does it exist as-is?
    let mut path2 = path.clone();
    path2.push(seg);
    if !no_init_check {
        match path2.metadata() {
            Ok(_) => return (path2, false),
            Err(ref e) if e.kind() != ErrorKind::NotFound => {
                // stop on errors other than "NotFound".
                return (path2, true)
            },
            Err(_) => {},
        }
    }

    // first, lowercase filename.
    let filename = match seg.to_str() {
        Some(s) => s.to_lowercase(),
        None => return (path2, true),
    };

    // we have to read the entire directory.
    let dir = match path.read_dir() {
        Ok(dir) => dir,
        Err(_) => return (path2, true),
    };
    for entry in dir.into_iter() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let entry_name = entry.file_name();
        let name = match entry_name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if name.to_lowercase() == filename {
            path.push(&name);
            return (path, false);
        }
    }
    (path2, true)
}

// The cache stores a mapping of lowercased path -> actual path.
pub struct Cache {
    cache:      Mutex<LruCache<PathBuf, Entry>>,
}

#[derive(Clone)]
struct Entry {
    // Full case-sensitive pathname.
    path:   PathBuf,
}

// helper
fn pathbuf_to_lowercase(path: PathBuf) -> PathBuf {
    let s = match OsString::from(path).into_string() {
        Ok(s) => OsString::from(s.to_lowercase()),
        Err(s) => s,
    };
    PathBuf::from(s)
}

impl Cache {
    pub fn new(size: usize) -> Cache {
        Cache{ cache: Mutex::new(LruCache::new(size)) }
    }

    // Insert an entry into the cache.
    pub fn insert(&self, path: &Path) {
        let lc_path = pathbuf_to_lowercase(PathBuf::from(path));
        let e = Entry {
            path:   PathBuf::from(path),
        };
        let mut cache = self.cache.lock();
        cache.put(lc_path, e);
    }

    // Get an entry from the cache, and validate it. If it's valid
    // return the actual pathname and metadata. If it's invalid remove
    // it from the cache and return None.
    pub fn get(&self, path: &Path) -> Option<(PathBuf, fs::Metadata)> {
        // First lowercase the entire path.
        let lc_path = pathbuf_to_lowercase(PathBuf::from(path));
        // Lookup.
        let e = {
            let mut cache = self.cache.lock();
            cache.get(&lc_path)?.clone()
        };
        // Found, validate.
        match fs::metadata(&e.path) {
            Err(_) => {
                let mut cache = self.cache.lock();
                cache.pop(&lc_path);
                None
            }
            Ok(m) => Some((e.path, m))
        }
    }
}
