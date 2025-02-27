//! The content index (PLAN.md §6.4): which files contain which words.
//!
//! One Tantivy document per file. A file is identified by its volume and MFT
//! record number (`key`), not by its path: renaming or moving a file inside
//! the indexed folders costs nothing, the name index gives the current path.
//!
//! The text itself is **not stored** — only the word positions — so the index
//! stays small; previews re-extract the file. Next to each document we keep
//! the file's size and last-write time as indexed, to know when it changed,
//! and how the extraction went (`Status`), so failures are not retried until
//! the file changes.

pub mod tokenizer;

use std::collections::HashMap;
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
const FORMAT: u32 = 1;
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
    pub fn open(dir: &Path) -> tantivy::Result<ContentIndex> {
        std::fs::create_dir_all(dir)?;
        let marker = dir.join("whereti-format.txt");
        let current = std::fs::read_to_string(&marker).ok().and_then(|s| s.trim().parse::<u32>().ok());
        let (schema, f) = schema();
        let open = || -> tantivy::Result<Index> {
            if current == Some(FORMAT) {
                Index::open_in_dir(dir)
            } else {
                Err(tantivy::TantivyError::SchemaError("old format".into()))
            }
        };
        let index = match open() {
            Ok(i) if i.schema() == schema => i,
            _ => {
                // Start over: remove whatever is there.
                for e in std::fs::read_dir(dir)?.flatten() {
                    let p = e.path();
                    let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                }
                let i = Index::create_in_dir(dir, schema)?;
                std::fs::write(&marker, FORMAT.to_string())?;
                i
            }
        };
        Self::with_index(index, f)
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
        let words: Vec<String> = tokenize(text, false).into_iter().map(|t| t.text).collect();
        let (last, rest) = words.split_last()?;
        let term = |w: &str| Term::from_field_text(self.f.body, w);
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
        if words.len() > 1 {
            let phrase = PhraseQuery::new(words.iter().map(|w| term(w)).collect());
            must.push((Occur::Should, Box::new(BoostQuery::new(Box::new(phrase), 3.0))));
        }
        Some(Box::new(BooleanQuery::new(must)))
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
        let idx = ContentIndex::open(dir.path()).unwrap();
        assert_eq!(idx.docs(), 0, "another format: rebuilt from scratch");
    }

    #[test]
    fn keys_round_trip() {
        assert_eq!(split_key(key('D', 123_456_789)), ('D', 123_456_789));
    }
}
