//! Disposable, persistent filename and IPTC search for Lightbox.
//!
//! This is deliberately not a photo catalogue. The only durable facts are the
//! locations the user chose and a rebuildable list of relative paths beneath them.
//! No photo is moved, imported, or made dependent on this file.

use crate::decode::Queue;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::settings;

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

const CONFIG: &str = "search-locations.toml";
const INDEX: &str = "search-index.bin";
// Version 2 adds effective IPTC text to every path record. An older index is simply
// discarded and rebuilt from the locations file, which is why this is a disposable
// cache rather than a migration surface. Version 4 is the IPTC field set and order
// the Metadata pane was given, keywords included (3 was an unreleased step toward
// it): a record names its field by position, so every stored position changed
// meaning.
const MAGIC: &[u8; 8] = b"MPSRCH04";
const RESULT_LIMIT: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Location {
    pub id: u64,
    pub path: PathBuf,
    pub enabled: bool,
    #[serde(skip)]
    pub count: usize,
}

impl Default for Location {
    fn default() -> Self {
        Self {
            id: 0,
            path: PathBuf::new(),
            enabled: true,
            count: 0,
        }
    }
}

impl Location {
    pub fn online(&self) -> bool {
        self.path.is_dir()
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct Config {
    locations: Vec<Location>,
}

#[derive(Debug, Clone)]
struct Record {
    location: u64,
    relative: PathBuf,
    folded_path: String,
    folded_name: String,
    metadata: Vec<MetadataText>,
}

impl Record {
    #[cfg(test)]
    fn new(location: u64, relative: PathBuf) -> Self {
        Self::with_metadata(location, relative, Vec::new())
    }

    fn with_metadata(location: u64, relative: PathBuf, metadata: Vec<(u8, String)>) -> Self {
        let folded_path = fold(&relative.to_string_lossy());
        let folded_name = fold(
            relative
                .file_name()
                .map(|n| n.to_string_lossy())
                .as_deref()
                .unwrap_or(""),
        );
        Self {
            location,
            relative,
            folded_path,
            folded_name,
            metadata: metadata
                .into_iter()
                .filter(|(_, value)| !value.trim().is_empty())
                .map(|(field, value)| MetadataText {
                    field,
                    folded: fold(&value),
                    value,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
struct MetadataText {
    field: u8,
    value: String,
    folded: String,
}

#[derive(Debug, Clone)]
pub struct Match {
    pub path: PathBuf,
    pub online: bool,
    /// The IPTC field and value responsible for the match, when the path itself was
    /// not the whole answer. This is display context, not another search result.
    pub metadata: Option<String>,
}

#[derive(Debug)]
pub struct Results {
    pub generation: u64,
    pub matches: Vec<Match>,
    pub total: usize,
}

struct ScanDone {
    generation: u64,
    records: Vec<Record>,
}

/// The UI-facing half of the index. Filesystem walking and matching both happen on
/// workers; `poll` is intentionally cheap enough to call once per frame.
pub struct SearchIndex {
    locations: Vec<Location>,
    records: Arc<Vec<Record>>,
    scan_generation: u64,
    query_generation: u64,
    scanning: bool,
    querying: bool,
    scans: Queue<u8, Option<ScanDone>>,
    queries: Queue<u8, Option<Results>>,
    writes: Queue<u8, std::io::Result<()>>,
    pending_edits: HashMap<PathBuf, HashMap<u8, String>>,
    error: Option<String>,
    scan_token: Arc<AtomicU64>,
    query_token: Arc<AtomicU64>,
    last_query: String,
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchIndex {
    pub fn new() -> Self {
        // Unit tests construct hundreds of Lightboxes. They must not turn that into
        // hundreds of background walks over a user's real photo drives.
        let (locations, records) = if cfg!(test) {
            (Vec::new(), Vec::new())
        } else {
            (read_config(), read_index().unwrap_or_default())
        };
        let mut this = Self {
            locations,
            records: Arc::new(records),
            scan_generation: 0,
            query_generation: 0,
            scanning: false,
            querying: false,
            scans: Queue::new(1),
            queries: Queue::new(1),
            writes: Queue::new(1),
            pending_edits: HashMap::new(),
            error: None,
            scan_token: Arc::new(AtomicU64::new(0)),
            query_token: Arc::new(AtomicU64::new(0)),
            last_query: String::new(),
        };
        this.refresh_counts();
        // A configuration without an index is normally the first launch after the
        // feature was added. Begin filling it without making Lightbox wait.
        if !this.locations.is_empty() {
            this.reindex();
        }
        this
    }

    pub fn locations(&self) -> &[Location] {
        &self.locations
    }

    pub fn scanning(&self) -> bool {
        self.scanning
    }

    pub fn querying(&self) -> bool {
        self.querying
    }

    pub fn indexed_count(&self) -> usize {
        self.records.len()
    }

    pub fn add_location(&mut self, path: PathBuf) {
        if self.locations.iter().any(|l| l.path == path) {
            return;
        }
        self.locations.push(Location {
            id: location_id(&path),
            path,
            enabled: true,
            count: 0,
        });
        write_config(&self.locations);
        self.reindex();
    }

    pub fn remove_location(&mut self, id: u64) {
        self.locations.retain(|l| l.id != id);
        self.records = Arc::new(
            self.records
                .iter()
                .filter(|r| r.location != id)
                .cloned()
                .collect(),
        );
        self.refresh_counts();
        write_config(&self.locations);
        self.reindex();
        self.repeat_query();
    }

    pub fn set_enabled(&mut self, id: u64, enabled: bool) {
        if let Some(location) = self.locations.iter_mut().find(|l| l.id == id) {
            location.enabled = enabled;
            write_config(&self.locations);
            self.repeat_query();
        }
    }

    /// Bring IPTC edits made in Lightbox into the live index without walking an
    /// entire drive again. Unmentioned fields stay exactly as the last scan found
    /// them, which is what makes a one-field edit safe for embedded metadata too.
    pub fn update_iptc(
        &mut self,
        paths: &[PathBuf],
        updates: &[(raw_core::sidecar::IptcField, String)],
    ) {
        if paths.is_empty() || updates.is_empty() {
            return;
        }
        if self.scanning {
            for path in paths {
                let fields = self.pending_edits.entry(path.clone()).or_default();
                for (field, value) in updates {
                    fields.insert(*field as u8, value.clone());
                }
            }
        }
        let wanted: std::collections::HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let roots: HashMap<u64, &Path> = self
            .locations
            .iter()
            .map(|location| (location.id, location.path.as_path()))
            .collect();
        let mut records = self.records.as_ref().clone();
        let mut changed = false;
        for record in &mut records {
            let Some(root) = roots.get(&record.location) else {
                continue;
            };
            if !wanted.contains(root.join(&record.relative).as_path()) {
                continue;
            }
            for (field, value) in updates {
                let key = *field as u8;
                record.metadata.retain(|text| text.field != key);
                if !value.trim().is_empty() {
                    record.metadata.push(MetadataText {
                        field: key,
                        value: value.clone(),
                        folded: fold(value),
                    });
                }
            }
            record.metadata.sort_by_key(|text| text.field);
            changed = true;
        }
        if changed {
            self.records = Arc::new(records);
            self.persist();
            self.repeat_query();
        }
    }

    pub fn reindex(&mut self) {
        self.scan_generation = self.scan_generation.wrapping_add(1);
        let generation = self.scan_generation;
        self.scan_token.store(generation, Ordering::Relaxed);
        self.scanning = true;
        let locations = self.locations.clone();
        let old = Arc::clone(&self.records);
        let token = Arc::clone(&self.scan_token);
        self.scans.submit(0, 0, move || {
            let cancelled = || token.load(Ordering::Relaxed) != generation;
            let mut by_location: HashMap<u64, Vec<Record>> = HashMap::new();
            for record in old.iter() {
                if cancelled() {
                    return None;
                }
                by_location
                    .entry(record.location)
                    .or_default()
                    .push(record.clone());
            }
            for location in &locations {
                if cancelled() {
                    return None;
                }
                // An unplugged drive keeps its previous records. That is what
                // allows a search to say where an offline photograph lives.
                if location.online() {
                    by_location.insert(location.id, scan(location, &cancelled)?);
                }
            }
            let valid: std::collections::HashSet<u64> = locations.iter().map(|l| l.id).collect();
            let mut records: Vec<Record> = by_location
                .into_iter()
                .filter(|(id, _)| valid.contains(id))
                .flat_map(|(_, records)| records)
                .collect();
            records.sort_by(|a, b| (a.location, &a.folded_path).cmp(&(b.location, &b.folded_path)));
            if token.load(Ordering::Relaxed) != generation {
                return None;
            }
            Some(ScanDone {
                generation,
                records,
            })
        });
    }

    pub fn query(&mut self, query: &str) {
        self.last_query = query.trim().to_owned();
        self.query_generation = self.query_generation.wrapping_add(1);
        let generation = self.query_generation;
        self.query_token.store(generation, Ordering::Relaxed);
        if self.last_query.is_empty() {
            self.querying = false;
            self.queries.cancel(0);
            return;
        }
        self.querying = true;
        let query = fold(&self.last_query);
        let enabled: HashMap<u64, (PathBuf, bool)> = self
            .locations
            .iter()
            .filter(|l| l.enabled)
            .map(|l| (l.id, (l.path.clone(), l.online())))
            .collect();
        let records = Arc::clone(&self.records);
        let token = Arc::clone(&self.query_token);
        self.queries.submit(0, 0, move || {
            let mut scored = Vec::new();
            for (at, record) in records.iter().enumerate() {
                if at % 512 == 0 && token.load(Ordering::Relaxed) != generation {
                    return None;
                }
                if let Some((root, online)) = enabled.get(&record.location)
                    && let Some(score) = score(record, &query)
                {
                    scored.push((
                        score.value,
                        Match {
                            path: root.join(&record.relative),
                            online: *online,
                            metadata: score.metadata.map(|at| {
                                let text = &record.metadata[at];
                                format!("{} · {}", metadata_label(text.field), text.value)
                            }),
                        },
                    ));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
            let total = scored.len();
            scored.truncate(RESULT_LIMIT);
            let matches = scored.into_iter().map(|(_, result)| result).collect();
            if token.load(Ordering::Relaxed) != generation {
                return None;
            }
            Some(Results {
                generation,
                matches,
                total,
            })
        });
    }

    pub fn busy(&self) -> bool {
        self.scanning || self.querying || self.writes.in_flight() != 0
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    fn persist(&mut self) {
        let Some(path) = index_path() else { return };
        let records = Arc::clone(&self.records);
        self.writes
            .submit(0, 0, move || write_index_to(&path, &records));
    }

    fn accept_scan(&mut self, mut done: ScanDone) {
        if done.generation != self.scan_generation {
            return;
        }
        // Sidecars can change after the walker has read them. Apply only the
        // authored fields so unrelated embedded metadata survives the merge.
        for record in &mut done.records {
            let Some(root) = self.locations.iter().find(|l| l.id == record.location) else {
                continue;
            };
            if let Some(fields) = self.pending_edits.get(&root.path.join(&record.relative)) {
                for (&field, value) in fields {
                    record.metadata.retain(|text| text.field != field);
                    if !value.trim().is_empty() {
                        record.metadata.push(MetadataText {
                            field,
                            value: value.clone(),
                            folded: fold(value),
                        });
                    }
                }
                record.metadata.sort_by_key(|text| text.field);
            }
        }
        self.pending_edits.clear();
        self.records = Arc::new(done.records);
        self.scanning = false;
        self.refresh_counts();
        self.persist();
        self.repeat_query();
    }

    pub fn poll(&mut self) -> Option<Results> {
        while let Some((_, result)) = self.scans.poll() {
            self.scanning = false;
            match result {
                Ok(Some(done)) => self.accept_scan(done),
                Ok(None) => {}
                Err(error) => {
                    self.pending_edits.clear();
                    self.error = Some(format!("Search scan failed: {error}"));
                }
            }
        }
        while let Some((_, result)) = self.writes.poll() {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self.error = Some(format!("Search cache write failed: {error}")),
                Err(error) => self.error = Some(format!("Search cache worker failed: {error}")),
            }
        }
        let mut newest = None;
        while let Some((_, result)) = self.queries.poll() {
            self.querying = false;
            match result {
                Ok(Some(result)) if result.generation == self.query_generation => {
                    newest = Some(result)
                }
                Ok(_) => {}
                Err(error) => self.error = Some(format!("Search query failed: {error}")),
            }
        }
        newest
    }

    fn repeat_query(&mut self) {
        let query = self.last_query.clone();
        if !query.is_empty() {
            self.query(&query);
        }
    }

    fn refresh_counts(&mut self) {
        let mut counts: HashMap<u64, usize> = HashMap::new();
        for record in self.records.iter() {
            *counts.entry(record.location).or_default() += 1;
        }
        for location in &mut self.locations {
            location.count = counts.get(&location.id).copied().unwrap_or(0);
        }
    }
}

impl Drop for SearchIndex {
    fn drop(&mut self) {
        self.scan_token.fetch_add(1, Ordering::Relaxed);
        self.query_token.fetch_add(1, Ordering::Relaxed);
    }
}

fn scan(location: &Location, cancelled: &impl Fn() -> bool) -> Option<Vec<Record>> {
    let mut out = Vec::new();
    let mut stack = vec![location.path.clone()];
    while let Some(dir) = stack.pop() {
        if cancelled() {
            return None;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if cancelled() {
                return None;
            }
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                // Do not follow directory symlinks: photo drives often contain
                // aliases back into an ancestor and an index must never loop.
                if !kind.is_symlink() {
                    stack.push(path);
                }
            } else if kind.is_file()
                && crate::lightbox::is_searchable(&path)
                && let Ok(relative) = path.strip_prefix(&location.path)
            {
                out.push(Record::with_metadata(
                    location.id,
                    relative.to_path_buf(),
                    metadata_text(&path),
                ));
            }
        }
    }
    if cancelled() { None } else { Some(out) }
}

fn metadata_text(path: &Path) -> Vec<(u8, String)> {
    // Do not call `effective_metadata` here. Its raw decoder intentionally faults a
    // whole raw into memory, which is fine for the one selected frame and disastrous
    // for a drive-wide index. TIFF/DNG and JPEG expose XMP in header structures we can
    // seek to without reading pixels; every format can still contribute monopro's
    // tiny authored sidecar.
    let embedded = quick_xmp_packet(path)
        .and_then(|packet| raw_core::sidecar::from_xml(&packet).ok())
        .map(|sidecar| sidecar.metadata)
        .unwrap_or_default();
    let metadata = match raw_core::sidecar::read(path) {
        raw_core::sidecar::Loaded::Ok(sidecar) => {
            raw_core::sidecar::Metadata::merged(&embedded, &sidecar.metadata)
        }
        raw_core::sidecar::Loaded::Absent | raw_core::sidecar::Loaded::Corrupt(_) => embedded,
    };

    raw_core::sidecar::IptcField::ALL
        .into_iter()
        .enumerate()
        .filter_map(|(index, field)| {
            metadata
                .iptc(field)
                .filter(|value| !value.trim().is_empty())
                .map(|value| (index as u8, value.into_owned()))
        })
        .collect()
}

fn quick_xmp_packet(path: &Path) -> Option<String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "dng" | "tif" | "tiff" => tiff_xmp_packet(path),
        "jpg" | "jpeg" => jpeg_xmp_packet(path),
        _ => None,
    }
}

fn tiff_xmp_packet(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = tiff::decoder::Decoder::new(file).ok()?;
    let bytes = decoder.get_tag_u8_vec(tiff::tags::Tag::Unknown(700)).ok()?;
    String::from_utf8(bytes).ok()
}

fn jpeg_xmp_packet(path: &Path) -> Option<String> {
    const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    let mut file = std::fs::File::open(path).ok()?;
    let mut soi = [0u8; 2];
    file.read_exact(&mut soi).ok()?;
    if soi != [0xff, 0xd8] {
        return None;
    }

    loop {
        let mut marker = [0u8; 2];
        file.read_exact(&mut marker).ok()?;
        while marker[0] != 0xff {
            marker[0] = marker[1];
            file.read_exact(&mut marker[1..]).ok()?;
        }
        while marker[1] == 0xff {
            file.read_exact(&mut marker[1..]).ok()?;
        }
        if marker[1] == 0xda || marker[1] == 0xd9 {
            return None;
        }
        if matches!(marker[1], 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let mut length = [0u8; 2];
        file.read_exact(&mut length).ok()?;
        let payload = usize::from(u16::from_be_bytes(length)).checked_sub(2)?;
        if marker[1] == 0xe1 && payload >= XMP_HEADER.len() {
            let mut bytes = vec![0u8; payload];
            file.read_exact(&mut bytes).ok()?;
            if bytes.starts_with(XMP_HEADER) {
                return String::from_utf8(bytes[XMP_HEADER.len()..].to_vec()).ok();
            }
        } else {
            file.seek(SeekFrom::Current(payload as i64)).ok()?;
        }
    }
}

fn metadata_label(field: u8) -> &'static str {
    raw_core::sidecar::IptcField::ALL
        .get(field as usize)
        .map(|field| field.label())
        .unwrap_or("Keywords")
}

fn fold(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

struct Score {
    value: i64,
    metadata: Option<usize>,
}

fn score(record: &Record, query: &str) -> Option<Score> {
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.is_empty() {
        return None;
    }
    let mut total = 0i64;
    let mut metadata = None;
    let mut metadata_strength = i64::MIN;
    for term in terms {
        let name = &record.folded_name;
        let path = &record.folded_path;
        let direct = if name == term {
            Some(20_000)
        } else if name.starts_with(term) {
            Some(12_000 - name.len() as i64)
        } else if let Some(at) = name.find(term) {
            Some(9_000 - at as i64 * 4 - name.len() as i64)
        } else {
            path.find(term).map(|at| 5_000 - at as i64)
        };

        let mut best = direct.map(|value| (value, None));
        for (index, text) in record.metadata.iter().enumerate() {
            let value = metadata_term_score(&text.folded, term);
            if value.is_some_and(|value| best.is_none_or(|(have, _)| value > have)) {
                best = value.map(|value| (value, Some(index)));
            }
        }
        if direct.is_none()
            && let Some(value) = subsequence_score(path, term)
            && best.is_none_or(|(have, _)| value > have)
        {
            best = Some((value, None));
        }
        let (value, field) = best?;
        total += value;
        if field.is_some() && value > metadata_strength {
            metadata = field;
            metadata_strength = value;
        }
    }
    Some(Score {
        value: total - record.folded_path.len() as i64,
        metadata,
    })
}

fn metadata_term_score(text: &str, term: &str) -> Option<i64> {
    if text == term {
        Some(8_500)
    } else if text.split_whitespace().any(|word| word == term) {
        Some(7_500)
    } else if text.starts_with(term) {
        Some(7_000 - text.len().min(1_000) as i64)
    } else if let Some(at) = text.find(term) {
        Some(6_000 - at.min(1_000) as i64)
    } else {
        subsequence_score(text, term).map(|score| score - 200)
    }
}

fn subsequence_score(haystack: &str, needle: &str) -> Option<i64> {
    let mut wanted = needle.chars();
    let mut current = wanted.next()?;
    let mut first = None;
    let mut last = 0usize;
    let mut run = 0i64;
    let mut best_run = 0i64;
    for (i, ch) in haystack.char_indices() {
        if ch == current {
            first.get_or_insert(i);
            if i == last + 1 {
                run += 1;
            } else {
                run = 1;
            }
            best_run = best_run.max(run);
            last = i;
            match wanted.next() {
                Some(next) => current = next,
                None => {
                    let span = i.saturating_sub(first.unwrap_or(i)) as i64;
                    return Some(1_000 + best_run * 40 - span);
                }
            }
        }
    }
    None
}

fn location_id(path: &Path) -> u64 {
    // Stable across launches; changing a mount path intentionally creates a new
    // location rather than silently assigning an old drive's records to it.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn config_path() -> Option<PathBuf> {
    settings::dir().map(|d| d.join(CONFIG))
}

fn index_path() -> Option<PathBuf> {
    settings::dir().map(|d| d.join(INDEX))
}

fn read_config() -> Vec<Location> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut config: Config = toml::from_str(&text).unwrap_or_default();
    for location in &mut config.locations {
        if location.id == 0 {
            location.id = location_id(&location.path);
        }
    }
    config.locations
}

fn write_config(locations: &[Location]) {
    let Some(path) = config_path() else { return };
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let Ok(text) = toml::to_string_pretty(&Config {
        locations: locations.to_vec(),
    }) else {
        return;
    };
    let _ = raw_core::atomic_file::write(&path, |file| file.write_all(text.as_bytes()));
}

fn read_index() -> std::io::Result<Vec<Record>> {
    let path = index_path().ok_or_else(|| std::io::Error::other("no storage directory"))?;
    read_index_from(&path)
}

fn read_index_from(path: &Path) -> std::io::Result<Vec<Record>> {
    let mut file = std::fs::File::open(path)?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unknown search index",
        ));
    }
    let count = read_u64(&mut file)?;
    let mut records = Vec::with_capacity(count.min(10_000_000) as usize);
    for _ in 0..count {
        let location = read_u64(&mut file)?;
        let len = read_u32(&mut file)? as usize;
        if len > 16 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "search path is too long",
            ));
        }
        let mut bytes = vec![0u8; len];
        file.read_exact(&mut bytes)?;
        let relative = path_from_bytes(bytes);
        let fields = read_u32(&mut file)? as usize;
        if fields > 256 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "search record has too many metadata fields",
            ));
        }
        let mut metadata = Vec::with_capacity(fields);
        for _ in 0..fields {
            let field = read_u8(&mut file)?;
            let len = read_u32(&mut file)? as usize;
            if len > 4 * 1024 * 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "search metadata value is too long",
                ));
            }
            let mut bytes = vec![0u8; len];
            file.read_exact(&mut bytes)?;
            let value = String::from_utf8(bytes).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "search metadata is not UTF-8",
                )
            })?;
            metadata.push((field, value));
        }
        records.push(Record::with_metadata(location, relative, metadata));
    }
    Ok(records)
}

fn write_index_to(path: &Path, records: &[Record]) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other(
            "search index has no parent directory",
        ));
    };
    std::fs::create_dir_all(dir)?;
    raw_core::atomic_file::write(path, |file| {
        file.write_all(MAGIC)?;
        file.write_all(&(records.len() as u64).to_le_bytes())?;
        for record in records {
            let bytes = path_bytes(&record.relative);
            file.write_all(&record.location.to_le_bytes())?;
            file.write_all(&(bytes.len() as u32).to_le_bytes())?;
            file.write_all(&bytes)?;
            file.write_all(&(record.metadata.len() as u32).to_le_bytes())?;
            for text in &record.metadata {
                let bytes = text.value.as_bytes();
                file.write_all(&[text.field])?;
                file.write_all(&(bytes.len() as u32).to_le_bytes())?;
                file.write_all(bytes)?;
            }
        }
        file.flush()
    })
}

fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut bytes = [0u8; 8];
    r.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut bytes = [0u8; 4];
    r.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u8(r: &mut impl Read) -> std::io::Result<u8> {
    let mut byte = [0u8; 1];
    r.read_exact(&mut byte)?;
    Ok(byte[0])
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_queries_keep_one_worker_and_deliver_only_the_latest_request() {
        let mut index = SearchIndex::new();
        index.locations = vec![Location {
            id: 1,
            path: crate::settings::dir().unwrap(),
            ..Default::default()
        }];
        index.records = Arc::new(vec![Record::with_metadata(1, "latest.jpg".into(), vec![])]);
        for i in 0..100 {
            index.query(&format!("obsolete{i}"));
        }
        index.query("latest");
        assert_eq!(index.queries.in_flight(), 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(result) = index.poll() {
                assert_eq!(result.generation, index.query_generation);
                assert_eq!(result.total, 1);
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(index.queries.peak_concurrency(), 1);
        index.query("latest");
        index.query("");
        assert_eq!(index.queries.in_flight(), 0);
        assert!(!index.querying());
    }

    #[test]
    fn cancelled_walks_discard_partial_records() {
        let root = crate::settings::dir().unwrap().join("cancel-scan");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..20 {
            std::fs::write(root.join(format!("{i}.jpg")), b"jpg").unwrap();
        }
        let calls = std::cell::Cell::new(0);
        let result = scan(
            &Location {
                path: root,
                ..Default::default()
            },
            &|| {
                calls.set(calls.get() + 1);
                calls.get() >= 4
            },
        );
        assert!(result.is_none());
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn edits_to_newly_discovered_records_survive_scan_replacement() {
        let mut index = SearchIndex::new();
        let root = crate::settings::dir().unwrap();
        index.locations = vec![Location {
            id: 1,
            path: root.clone(),
            ..Default::default()
        }];
        index.scanning = true;
        index.scan_generation = 2;
        index.update_iptc(
            &[root.join("new.jpg")],
            &[(raw_core::sidecar::IptcField::Creator, "Current".into())],
        );
        index.accept_scan(ScanDone {
            generation: 1,
            records: vec![],
        });
        assert!(index.scanning);
        index.accept_scan(ScanDone {
            generation: 2,
            records: vec![Record::with_metadata(1, "new.jpg".into(), vec![])],
        });
        assert!(score(&index.records[0], "current").is_some());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while index.busy() {
            index.poll();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(index.take_error().is_none());
        assert!(score(&read_index().unwrap()[0], "current").is_some());
    }

    #[test]
    fn a_scan_cannot_restore_iptc_fields_changed_after_it_started() {
        for replacement in ["New Creator", ""] {
            let mut index = SearchIndex::new();
            let root = crate::settings::dir().unwrap();
            index.locations = vec![Location {
                id: 1,
                path: root.clone(),
                ..Default::default()
            }];
            let old = Record::with_metadata(
                1,
                "photo.dng".into(),
                vec![
                    (
                        raw_core::sidecar::IptcField::Creator as u8,
                        "Old Creator".into(),
                    ),
                    (raw_core::sidecar::IptcField::City as u8, "Lisbon".into()),
                ],
            );
            index.records = Arc::new(vec![old.clone()]);
            index.scan_generation = 1;
            index.scanning = true;
            index.update_iptc(
                &[root.join("photo.dng")],
                &[(raw_core::sidecar::IptcField::Creator, replacement.into())],
            );
            index.accept_scan(ScanDone {
                generation: 1,
                records: vec![old],
            });
            index.poll();
            assert!(score(&index.records[0], "old creator").is_none());
            assert!(score(&index.records[0], "lisbon").is_some());
            if !replacement.is_empty() {
                assert!(score(&index.records[0], &fold(replacement)).is_some());
            }
        }
    }

    #[test]
    fn exact_filename_beats_a_path_match_and_a_fuzzy_match() {
        let exact = Record::new(1, "portraits/alice.dng".into());
        let path = Record::new(1, "alice/session/frame.dng".into());
        let fuzzy = Record::new(1, "portraits/a_long_image_capture.dng".into());
        let value = |record: &Record| score(record, "alice").unwrap().value;
        assert!(value(&exact) > value(&path));
        assert!(value(&path) > value(&fuzzy));
    }

    #[test]
    fn every_word_must_match_but_need_not_be_contiguous() {
        let record = Record::new(1, "New York/2026/park_avenue.dng".into());
        assert!(score(&record, "york park").is_some());
        assert!(score(&record, "ny pave").is_some());
        assert!(score(&record, "york london").is_none());
    }

    #[test]
    fn iptc_text_is_fuzzy_searchable_and_reports_the_field_that_matched() {
        let record = Record::with_metadata(
            1,
            "2026/frame.dng".into(),
            vec![
                (
                    raw_core::sidecar::IptcField::Creator as u8,
                    "Example Photographer".into(),
                ),
                (raw_core::sidecar::IptcField::City as u8, "New York".into()),
            ],
        );
        let exact = score(&record, "photographer").expect("creator is searchable");
        assert_eq!(exact.metadata, Some(0));
        let fuzzy = score(&record, "exmpl").expect("metadata search is fuzzy");
        assert_eq!(fuzzy.metadata, Some(0));
        assert!(score(&record, "photographer london").is_none());
    }

    #[test]
    fn the_disposable_index_round_trips_metadata_and_its_field_identity() {
        let path = std::env::temp_dir().join(format!(
            "monopro-search-index-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let records = vec![Record::with_metadata(
            41,
            "archive/frame.dng".into(),
            vec![
                (
                    raw_core::sidecar::IptcField::Headline as u8,
                    "Opening night".into(),
                ),
                (
                    raw_core::sidecar::IptcField::Keywords as u8,
                    "theatre, backstage".into(),
                ),
            ],
        )];

        write_index_to(&path, &records).unwrap();
        let read = read_index_from(&path).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].location, 41);
        assert_eq!(read[0].relative, Path::new("archive/frame.dng"));
        assert_eq!(metadata_label(read[0].metadata[0].field), "Headline");
        assert_eq!(read[0].metadata[0].value, "Opening night");
        assert_eq!(metadata_label(read[0].metadata[1].field), "Keywords");
        assert!(score(&read[0], "backstage").is_some());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_lightbox_iptc_edit_updates_only_that_field_in_the_live_index() {
        let root = PathBuf::from("/search-root");
        let image = root.join("frame.dng");
        let mut index = SearchIndex::new();
        index.locations = vec![Location {
            id: 9,
            path: root,
            enabled: true,
            count: 1,
        }];
        index.records = Arc::new(vec![Record::with_metadata(
            9,
            "frame.dng".into(),
            vec![
                (
                    raw_core::sidecar::IptcField::Creator as u8,
                    "Old Creator".into(),
                ),
                (raw_core::sidecar::IptcField::City as u8, "New York".into()),
            ],
        )]);

        index.update_iptc(
            std::slice::from_ref(&image),
            &[(raw_core::sidecar::IptcField::Creator, "New Creator".into())],
        );
        let record = &index.records[0];
        assert!(score(record, "new creator").is_some());
        assert!(score(record, "old creator").is_none());
        assert!(score(record, "new york").is_some(), "city was flattened");

        index.update_iptc(
            &[image],
            &[(raw_core::sidecar::IptcField::Creator, String::new())],
        );
        assert!(score(&index.records[0], "new creator").is_none());
        assert!(score(&index.records[0], "new york").is_some());
    }

    #[test]
    fn jpeg_embedded_xmp_is_indexed_without_decoding_the_picture() {
        let path = std::env::temp_dir().join(format!(
            "monopro-search-xmp-{}-{:?}.jpg",
            std::process::id(),
            std::thread::current().id()
        ));
        let metadata = raw_core::sidecar::Metadata {
            headline: Some("Harbor at dawn".into()),
            ..Default::default()
        };
        let packet = raw_core::sidecar::to_xml(&Default::default(), &metadata, "search-test.jpg");
        let mut payload = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
        payload.extend_from_slice(packet.as_bytes());
        let length = u16::try_from(payload.len() + 2).unwrap().to_be_bytes();
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1, length[0], length[1]];
        jpeg.extend_from_slice(&payload);
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        std::fs::write(&path, jpeg).unwrap();

        let text = metadata_text(&path);
        let record = Record::with_metadata(1, "frame.jpg".into(), text);
        let hit = score(&record, "harbor dawn").expect("embedded headline was indexed");
        assert_eq!(
            hit.metadata
                .map(|index| metadata_label(record.metadata[index].field)),
            Some("Headline")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn indexing_collects_photos_recursively_and_skips_hidden_trees() {
        let root = std::env::temp_dir().join(format!(
            "monopro-search-scan-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("shoot/day-two")).unwrap();
        std::fs::create_dir_all(root.join(".cache")).unwrap();
        std::fs::write(root.join("shoot/one.dng"), b"raw").unwrap();
        std::fs::write(root.join("shoot/day-two/two.jpg"), b"jpg").unwrap();
        std::fs::write(root.join("shoot/master.psd"), b"psd").unwrap();
        std::fs::write(root.join("shoot/large-master.psb"), b"psb").unwrap();
        std::fs::write(root.join("shoot/notes.txt"), b"notes").unwrap();
        std::fs::write(root.join(".cache/hidden.dng"), b"raw").unwrap();
        raw_core::sidecar::write(
            &root.join("shoot/one.dng"),
            &Default::default(),
            &raw_core::sidecar::Metadata {
                title: Some("Apollo contact sheet".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let location = Location {
            id: 7,
            path: root.clone(),
            enabled: true,
            count: 0,
        };
        let records = scan(&location, &|| false).unwrap();
        let one = records
            .iter()
            .find(|record| record.relative == Path::new("shoot/one.dng"))
            .expect("the raw was indexed");
        let hit = score(one, "apollo").expect("sidecar IPTC was indexed");
        assert_eq!(
            hit.metadata
                .map(|index| metadata_label(one.metadata[index].field)),
            Some("Title")
        );

        let mut paths: Vec<PathBuf> = records.into_iter().map(|r| r.relative).collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                PathBuf::from("shoot/day-two/two.jpg"),
                PathBuf::from("shoot/large-master.psb"),
                PathBuf::from("shoot/master.psd"),
                PathBuf::from("shoot/one.dng")
            ]
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
