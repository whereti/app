//! The content index (PLAN.md §6.4): which files contain which words.
//!
//! One Tantivy document per file. A file is identified by its volume and MFT
//! record number (`key`), not by its path: renaming or moving a file inside
//! the indexed folders costs nothing, the name index gives the current path.
//!
//! The text itself is **not stored** - only the word positions - so the index
//! stays small; previews re-extract the file. Next to each document we keep
//! the file's size and last-write time as indexed, to know when it changed,
//! and how the extraction went (`Status`), so failures are not retried until
//! the file changes.

pub mod tokenizer;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use parking_lot::Mutex;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, BoostQuery, Occur, PhraseQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, Term};

pub use tokenizer::{tokenize, WordTokenizer};

/// Bump when the schema or the tokenizer changes: the index is rebuilt.
const FORMAT: u32 = 2;
const TOKENIZER: &str = "wt_words";
const WRITER_HEAP: usize = 32 << 20;

/// How extracting a file went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    /// Extracted, but no text (scanned PDF, empty file).
    Empty = 1,
    Failed = 2,
    Protected = 3,
    TooLarge = 4,
}

impl Status {
    fn from_u64(v: u64) -> Status {
        match v {
            0 => Status::Ok,
            1 => Status::Empty,
            3 => Status::Protected,
            4 => Status::TooLarge,
            _ => Status::Failed,
        }
    }
}

/// What the index holds about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Indexed {
    pub size: u64,
    /// Last write as the name index has it (minutes since 1601 UTC).
    pub modified: u32,
    pub status: Status,
}

/// `(volume letter, MFT record)` → document key.
pub fn key(letter: char, record: u64) -> u64 {
    ((letter as u64) << 48) | (record & 0xFFFF_FFFF_FFFF)
}

pub fn split_key(key: u64) -> (char, u64) {
    (char::from_u32((key >> 48) as u32).unwrap_or('?'), key & 0xFFFF_FFFF_FFFF)
}

struct Fields {
    key: Field,
    size: Field,
    modified: Field,
    status: Field,
    body: Field,
}

pub struct ContentIndex {
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    f: Fields,
}

pub struct Hits {
    /// Best first.
    pub keys: Vec<(u64, f32)>,
    /// Documents matching, before the limit.
    pub total: usize,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let key = b.add_u64_field("key", NumericOptions::default().set_indexed().set_fast());
    let size = b.add_u64_field("size", NumericOptions::default().set_fast());
    let modified = b.add_u64_field("modified", NumericOptions::default().set_fast());
    let status = b.add_u64_field("status", NumericOptions::default().set_fast());
    let body = b.add_text_field(
        "body",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    (b.build(), Fields { key, size, modified, status, body })
}

impl ContentIndex {
    /// Opens the index in `dir`, creating it (or recreating it, if it was
    /// written by another format version or is damaged).
    /// The folder also holds other files (settings, properties, vectors):
    /// only the index's own files are ever removed. An error that may pass
    /// (a file locked for a moment) is returned, nothing is removed: the
    /// caller tries again later.
    pub fn open(dir: &Path) -> tantivy::Result<ContentIndex> {
        std::fs::create_dir_all(dir)?;
        let marker = dir.join("whereti-format.txt");
        let current = match std::fs::read_to_string(&marker) {
            Ok(s) => s.trim().parse::<u32>().ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let (schema, f) = schema();
        if current == Some(FORMAT) {
            match Index::open_in_dir(dir) {
                Ok(i) if i.schema() == schema => return Self::with_index(i, f),
                Ok(_) => {}
                Err(e) if damaged(&e) => {}
                Err(e) => return Err(e),
            }
        }
        // Start over: remove the index's own files.
        for e in std::fs::read_dir(dir)?.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_file() && is_index_file(&name) {
                let _ = std::fs::remove_file(&p);
            }
        }
        let i = Index::create_in_dir(dir, schema)?;
        std::fs::write(&marker, FORMAT.to_string())?;
        Self::with_index(i, f)
    }

    /// An index in memory, for tests.
    pub fn in_memory() -> tantivy::Result<ContentIndex> {
        let (schema, f) = schema();
        Self::with_index(Index::create_in_ram(schema), f)
    }

    fn with_index(index: Index, f: Fields) -> tantivy::Result<ContentIndex> {
        index.tokenizers().register(TOKENIZER, WordTokenizer { split_code: true });
        let writer = index.writer_with_num_threads(1, WRITER_HEAP)?;
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        Ok(ContentIndex { index, reader, writer: Mutex::new(writer), f })
    }

    /// Every file in the index, with what was indexed (committed state).
    pub fn known(&self) -> tantivy::Result<HashMap<u64, Indexed>> {
        let searcher = self.reader.searcher();
        let mut out = HashMap::new();
        for seg in searcher.segment_readers() {
            let ff = seg.fast_fields();
            let (key, size, modified, status) = (ff.u64("key")?, ff.u64("size")?, ff.u64("modified")?, ff.u64("status")?);
            for d in seg.doc_ids_alive() {
                if let Some(k) = key.first(d) {
                    out.insert(
                        k,
                        Indexed {
                            size: size.first(d).unwrap_or(0),
                            modified: modified.first(d).unwrap_or(0) as u32,
                            status: Status::from_u64(status.first(d).unwrap_or(2)),
                        },
                    );
                }
            }
        }
        Ok(out)
    }

    /// Adds or replaces a file.
    pub fn put(&self, key: u64, what: Indexed, text: &str) -> tantivy::Result<()> {
        let w = self.writer.lock();
        w.delete_term(Term::from_field_u64(self.f.key, key));
        w.add_document(doc!(
            self.f.key => key,
            self.f.size => what.size,
            self.f.modified => what.modified as u64,
            self.f.status => what.status as u64,
            self.f.body => text,
        ))?;
        Ok(())
    }

    pub fn remove(&self, key: u64) {
        self.writer.lock().delete_term(Term::from_field_u64(self.f.key, key));
    }

    /// Makes the changes durable and visible to searches.
    pub fn commit(&self) -> tantivy::Result<()> {
        self.writer.lock().commit()?;
        self.reader.reload()
    }

    /// Removes everything (e.g. the indexed folders changed completely).
    pub fn clear(&self) -> tantivy::Result<()> {
        self.writer.lock().delete_all_documents()?;
        self.commit()
    }

    pub fn docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Files whose text contains every word of `text`. The last word may be
    /// incomplete (search as you type): `factur` finds „factura”. Several
    /// words in this order rank higher.
    pub fn search(&self, text: &str, limit: usize) -> tantivy::Result<Hits> {
        let Some(query) = self.build_query(text) else {
            return Ok(Hits { keys: Vec::new(), total: 0 });
        };
        let searcher = self.reader.searcher();
        let (top, total) = searcher.search(&*query, &(TopDocs::with_limit(limit.max(1)).order_by_score(), Count))?;
        let mut keys = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let seg = searcher.segment_reader(addr.segment_ord);
            if let Some(k) = seg.fast_fields().u64("key")?.first(addr.doc_id) {
                keys.push((k, score));
            }
        }
        Ok(Hits { keys, total })
    }

    fn build_query(&self, text: &str) -> Option<Box<dyn Query>> {
        let tokens = tokenize(text, false);
        let words: Vec<String> = tokens.iter().map(|t| t.text.clone()).collect();
        let (last, rest) = words.split_last()?;
        let term = |w: &str| Term::from_field_text(self.f.body, w);
        // letters of a script without spaces, one after the other in the
        // query: they must stand in this order in the text too
        let mut runs: Vec<Vec<Term>> = Vec::new();
        let mut prev_end: Option<usize> = None;
        for t in &tokens {
            let single = text[t.offset_from..t.offset_to].chars().next().is_some_and(tokenizer::unspaced);
            if !single {
                prev_end = None;
                continue;
            }
            let joined = prev_end.is_some_and(|end| text[end..t.offset_from].chars().all(tokenizer::unspaced));
            match runs.last_mut() {
                Some(run) if joined => run.push(term(&t.text)),
                _ => runs.push(vec![term(&t.text)]),
            }
            prev_end = Some(t.offset_to);
        }
        let mut must: Vec<(Occur, Box<dyn Query>)> = rest
            .iter()
            .map(|w| (Occur::Must, Box::new(TermQuery::new(term(w), IndexRecordOption::WithFreqs)) as Box<dyn Query>))
            .collect();
        // The last word: exact, or as a prefix once it has 3 letters.
        let last_q: Box<dyn Query> = if last.chars().count() >= 3 {
            let exact = Box::new(BoostQuery::new(Box::new(TermQuery::new(term(last), IndexRecordOption::WithFreqs)), 2.0));
            let prefix = RegexQuery::from_pattern(&format!("{}.*", regex_escape(last)), self.f.body).ok()?;
            Box::new(BooleanQuery::new(vec![(Occur::Should, exact), (Occur::Should, Box::new(prefix))]))
        } else {
            Box::new(TermQuery::new(term(last), IndexRecordOption::WithFreqs))
        };
        must.push((Occur::Must, last_q));
        for run in runs.into_iter().filter(|r| r.len() > 1) {
            must.push((Occur::Must, Box::new(PhraseQuery::new(run))));
        }
        if words.len() > 1 {
            let phrase = PhraseQuery::new(words.iter().map(|w| term(w)).collect());
            must.push((Occur::Should, Box::new(BoostQuery::new(Box::new(phrase), 3.0))));
        }
        Some(Box::new(BooleanQuery::new(must)))
    }

    /// Keys of the documents indexed before 0.9.4, whose letters of scripts
    /// without spaces (Chinese, Japanese, Thai…) were kept in whole runs, so
    /// a search finds them only once they are read again: those with such a
    /// word of more than one letter (today each letter is a word). A run
    /// longer than `tokenizer::MAX_WORD` was dropped then: a document with
    /// only such runs cannot be told apart.
    pub fn keys_with_old_unspaced_words(&self) -> tantivy::Result<HashSet<u64>> {
        use tantivy::{DocSet, TERMINATED};
        let searcher = self.reader.searcher();
        let mut out = HashSet::new();
        for seg in searcher.segment_readers() {
            let words = seg.inverted_index(self.f.body)?;
            let keys = seg.fast_fields().u64("key")?;
            let alive = seg.alive_bitset();
            let mut terms = words.terms().stream()?;
            while terms.advance() {
                // such letters take three or four bytes in UTF-8
                if !terms.key().iter().any(|&b| b >= 0xE0) || !std::str::from_utf8(terms.key()).is_ok_and(old_unspaced) {
                    continue;
                }
                let mut docs = words.read_postings_from_terminfo(terms.value(), IndexRecordOption::Basic)?;
                let mut d = docs.doc();
                while d != TERMINATED {
                    if alive.is_none_or(|a| a.is_alive(d)) {
                        out.extend(keys.first(d));
                    }
                    d = docs.advance();
                }
            }
        }
        Ok(out)
    }

    /// Bytes on disk (all segment files).
    pub fn disk_bytes(&self) -> u64 {
        let searcher = self.reader.searcher();
        searcher.space_usage().map(|u| u.total().get_bytes()).unwrap_or(0)
    }

    pub fn segments(&self) -> usize {
        self.index.searchable_segment_ids().map(|s| s.len()).unwrap_or(0)
    }
}

/// The index cannot be used as it is (not a passing error): start over.
fn damaged(e: &tantivy::TantivyError) -> bool {
    use tantivy::directory::error::OpenReadError;
    use tantivy::TantivyError as E;
    matches!(
        e,
        E::DataCorruption(_)
            | E::IncompatibleIndex(_)
            | E::SchemaError(_)
            | E::DeserializeError(_)
            | E::FieldNotFound(_)
            | E::OpenReadError(OpenReadError::FileDoesNotExist(_) | OpenReadError::IncompatibleIndex(_))
    )
}

/// A file Tantivy writes (its metadata, locks and segments named by a
/// 32-digit id) or the format marker; never anything else in the folder.
fn is_index_file(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    matches!(name, "meta.json" | ".managed.json" | "whereti-format.txt")
        || (name.starts_with(".tantivy-") && name.ends_with(".lock"))
        || (stem.len() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A word only the tokenizer before 0.9.4 made: more than one letter, one
/// of them of a script without spaces. A letter folded into two (が into か
/// and its voicing mark) is still one.
fn old_unspaced(word: &str) -> bool {
    let letters = word.chars().filter(|c| !matches!(c, '\u{3099}' | '\u{309A}')).count();
    letters > 1 && word.chars().any(tokenizer::unspaced)
}

fn regex_escape(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        if !c.is_alphanumeric() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(size: u64) -> Indexed {
        Indexed { size, modified: 100, status: Status::Ok }
    }

    #[test]
    fn put_search_replace_remove() {
        let idx = ContentIndex::in_memory().unwrap();
        idx.put(key('C', 10), ok(1), "Factură fiscală nr. 12 — total de plată 1.250 lei").unwrap();
        idx.put(key('C', 11), ok(2), "Contract de prestări servicii; plata se face lunar").unwrap();
        idx.put(key('D', 10), ok(3), "fn getUserName() -> String { todo!() }").unwrap();
        idx.commit().unwrap();

        let keys = |q: &str| idx.search(q, 10).unwrap().keys.into_iter().map(|k| k.0).collect::<Vec<_>>();
        assert_eq!(keys("factura"), [key('C', 10)], "diacritics folded");
        assert_eq!(keys("FACTUR"), [key('C', 10)], "last word is a prefix");
        assert_eq!(keys("plata").len(), 2);
        assert_eq!(keys("total de plata")[0], key('C', 10));
        assert!(keys("plata contract").contains(&key('C', 11)));
        assert!(keys("factura contract").is_empty(), "every word must be present");
        assert_eq!(keys("user"), [key('D', 10)], "code words split");
        assert_eq!(keys("getusername"), [key('D', 10)]);
        assert!(keys("").is_empty() && keys("  ,. ").is_empty());

        // replace: the old text is gone
        idx.put(key('C', 10), ok(9), "Ofertă de preț").unwrap();
        idx.remove(key('D', 10));
        idx.commit().unwrap();
        assert!(keys("factura").is_empty());
        assert_eq!(keys("oferta"), [key('C', 10)]);
        assert!(keys("user").is_empty());
        let known = idx.known().unwrap();
        assert_eq!(known.len(), 2);
        assert_eq!(known[&key('C', 10)].size, 9);
    }

    #[test]
    fn reopens_from_disk_and_resets_on_format_change() {
        let dir = tempfile::tempdir().unwrap();
        {
            let idx = ContentIndex::open(dir.path()).unwrap();
            idx.put(key('C', 5), Indexed { size: 7, modified: 42, status: Status::Protected }, "").unwrap();
            idx.put(key('C', 6), ok(1), "raport anual").unwrap();
            idx.commit().unwrap();
        }
        let idx = ContentIndex::open(dir.path()).unwrap();
        assert_eq!(idx.docs(), 2);
        assert_eq!(idx.known().unwrap()[&key('C', 5)].status, Status::Protected);
        assert_eq!(idx.search("raport", 5).unwrap().total, 1);
        drop(idx);
        std::fs::write(dir.path().join("whereti-format.txt"), "0").unwrap();
        std::fs::write(dir.path().join("photo-tags.txt"), "on\n").unwrap();
        std::fs::write(dir.path().join("meaning.bin"), "x").unwrap();
        let idx = ContentIndex::open(dir.path()).unwrap();
        assert_eq!(idx.docs(), 0, "another format: rebuilt from scratch");
        // the settings and other stores beside the index stay
        assert_eq!(std::fs::read_to_string(dir.path().join("photo-tags.txt")).unwrap(), "on\n");
        assert!(dir.path().join("meaning.bin").exists());
    }

    #[test]
    fn only_the_index_files_are_its_own() {
        assert!(is_index_file("meta.json") && is_index_file(".managed.json") && is_index_file(".tantivy-writer.lock"));
        assert!(is_index_file("0123456789abcdef0123456789abcdef.idx"));
        assert!(!is_index_file("props.txt") && !is_index_file("crash.txt") && !is_index_file("ocr-1"));
    }

    #[test]
    fn text_without_spaces_is_found() {
        let idx = ContentIndex::in_memory().unwrap();
        idx.put(key('C', 1), ok(1), "東京都の天気予報は晴れです").unwrap();
        idx.put(key('C', 2), ok(1), "大阪の天気").unwrap();
        idx.commit().unwrap();
        let keys = |q: &str| idx.search(q, 10).unwrap().keys.into_iter().map(|k| k.0).collect::<Vec<_>>();
        assert_eq!(keys("東京"), [key('C', 1)]);
        assert_eq!(keys("天気")[..].len(), 2);
        assert_eq!(keys("天気予報"), [key('C', 1)]);
        assert!(keys("京東").is_empty(), "the letters in the query's order");
        assert_eq!(keys("東京 天気"), [key('C', 1)]);
    }

    #[test]
    fn documents_indexed_with_whole_runs_of_letters_are_found_once() {
        // written as before 0.9.4: words are runs of letters, CJK ones too
        let (schema, f) = schema();
        let index = Index::create_in_ram(schema);
        index.tokenizers().register(TOKENIZER, tantivy::tokenizer::SimpleTokenizer::default());
        {
            let mut w: IndexWriter = index.writer_with_num_threads(1, WRITER_HEAP).unwrap();
            for (k, text) in [(1, "東京都の天気予報"), (2, "raport anual 2024"), (3, "ราคา 100 บาท"), (4, "大阪の天気")] {
                w.add_document(doc!(f.key => key('C', k), f.size => 1u64, f.modified => 1u64, f.status => 0u64, f.body => text)).unwrap();
            }
            w.commit().unwrap();
        }
        let idx = ContentIndex::with_index(index, f).unwrap();
        // 4 read again now: letter by letter, no longer to read again
        idx.put(key('C', 4), ok(1), "大阪の天気").unwrap();
        idx.put(key('C', 5), ok(1), "が東京").unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.keys_with_old_unspaced_words().unwrap(), [key('C', 1), key('C', 3)].into());
        assert!(old_unspaced("東京") && old_unspaced("2024年") && !old_unspaced("東") && !old_unspaced("か\u{3099}") && !old_unspaced("raport"));
    }

    #[test]
    fn keys_round_trip() {
        assert_eq!(split_key(key('D', 123_456_789)), ('D', 123_456_789));
    }
}
