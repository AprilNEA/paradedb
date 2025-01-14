use std::collections::HashMap;
use tantivy::collector::TopDocs;
use tantivy::{
    query::{Query, QueryParser},
    DocAddress, Score, Searcher,
};
use tantivy::{DocId, Document, SegmentReader};

use super::score::SearchIndexScore;
use super::SearchIndex;
use crate::schema::{SearchConfig, SearchIndexSchema};

pub struct SearchState {
    pub schema: SearchIndexSchema,
    pub query: Box<dyn Query>,
    pub parser: QueryParser,
    pub searcher: Searcher,
    pub iterator: *mut std::vec::IntoIter<(SearchIndexScore, DocAddress)>,
    pub config: SearchConfig,
    pub key_field_name: String,
}

impl SearchState {
    pub fn new(search_index: &SearchIndex, config: &SearchConfig) -> Self {
        let schema = search_index.schema.clone();
        let mut parser = search_index.query_parser();
        let query = config
            .query
            .clone()
            .into_tantivy_query(&schema, &mut parser)
            .unwrap_or_else(|err| panic!("could not parse query: {err}"));
        let key_field_name = schema.key_field().name.0;
        SearchState {
            schema,
            query,
            parser,
            config: config.clone(),
            searcher: search_index.searcher(),
            iterator: std::ptr::null_mut(),
            key_field_name,
        }
    }

    pub fn key_field_value(&mut self, doc_address: DocAddress) -> i64 {
        let retrieved_doc = self.searcher.doc(doc_address).expect("could not find doc");

        let key_field = self
            .schema
            .schema
            .get_field(&self.key_field_name)
            .expect("field '{key_field_name}' not found in schema");

        if let tantivy::schema::Value::I64(key_field_value) =
            retrieved_doc.get_first(key_field).unwrap_or_else(|| {
                panic!(
                    "value for key_field '{}' not found in doc",
                    &self.key_field_name,
                )
            })
        {
            *key_field_value
        } else {
            panic!("error unwrapping ctid value")
        }
    }

    /// Search the Tantivy index for matching documents. If used outside of Postgres
    /// index access methods, this may return deleted rows until a VACUUM. If you need to scan
    /// the Tantivy index without a Postgres deduplication, you should use the `search_dedup`
    /// method instead.
    pub fn search(&mut self) -> Vec<(SearchIndexScore, DocAddress)> {
        // Extract limit and offset from the query config or set defaults.
        let limit = self.config.limit_rows.unwrap_or_else(|| {
            // We use unwrap_or_else here so this block doesn't run unless
            // we actually need the default value. This is important, because there can
            // be some cost to Tantivy API calls.
            let num_docs = self.searcher.num_docs() as usize;
            if num_docs > 0 {
                num_docs // The collector will panic if it's passed a limit of 0.
            } else {
                1 // Since there's no docs to return anyways, just use 1.
            }
        });

        let offset = self.config.offset_rows.unwrap_or(0);
        let key_field_name = self.key_field_name.clone();
        let top_docs_by_custom_score = TopDocs::with_limit(limit).and_offset(offset).tweak_score(
            // tweak_score expects a function that will return a function. A little unusual for
            // Rust, but not too much of a problem as long as you don't need to reference
            // many variables outside the function scope.
            move |segment_reader: &SegmentReader| {
                let key_field_reader = segment_reader
                    .fast_fields()
                    .i64(&key_field_name)
                    .unwrap_or_else(|err| {
                        panic!("key field {} is not a u64: {err:?}", &key_field_name)
                    })
                    .first_or_default_col(0);

                move |doc: DocId, original_score: Score| SearchIndexScore {
                    bm25: original_score,
                    key: key_field_reader.get_val(doc),
                }
            },
        );

        self.searcher
            .search(&self.query, &top_docs_by_custom_score)
            .expect("failed to search")
    }

    /// A search method that deduplicates results based on key field. This is important for
    /// searches into the Tantivy index outside of Postgres index access methods. Postgres will
    /// filter out stale rows when using the index scan, but when scanning Tantivy directly,
    /// we risk returning deleted documents if a VACUUM hasn't been performed yet.
    pub fn search_dedup(&mut self) -> impl Iterator<Item = (SearchIndexScore, DocAddress)> {
        let search_results = self.search();
        let mut dedup_map: HashMap<i64, (SearchIndexScore, DocAddress)> = HashMap::new();
        let mut order_vec: Vec<i64> = Vec::new();

        for (score, doc_addr) in search_results {
            let key = score.key;
            let is_new_or_higher = match dedup_map.get(&key) {
                Some((_, existing_doc_addr)) => doc_addr > *existing_doc_addr,
                None => true,
            };
            if is_new_or_higher && dedup_map.insert(key, (score, doc_addr)).is_none() {
                // Key was not already present, remember the order of this key
                order_vec.push(key);
            }
        }

        order_vec
            .into_iter()
            .filter_map(move |key| dedup_map.remove(&key))
    }

    #[allow(unused)]
    pub fn doc(&self, doc_address: DocAddress) -> tantivy::Result<Document> {
        self.searcher.doc(doc_address)
    }
}
