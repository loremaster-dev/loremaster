//! In-memory vector index. Exact search is fast enough at 50k memories; an ANN index would be the
//! next step if the limit grew by orders of magnitude.

use std::collections::HashMap;

use loremaster_core::{RecordType, Scope};
use thiserror::Error;

use crate::store::StoredMemory;

/// Embedding dimensions of the pinned model.
pub const DIMENSIONS: usize = 384;

/// Two facts at least this similar say the same thing.
pub const DUPLICATE_SIMILARITY: f32 = 0.95;

/// A vector of the wrong length.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("vector has {got} dimensions, expected {expected}")]
pub struct DimMismatch {
    pub expected: usize,
    pub got: usize,
}

struct Row {
    memory_id: String,
    record_type: Option<RecordType>,
    scope: Option<Scope>,
    project_id: Option<String>,
    created_at: Option<i64>,
    live: bool,
}

/// One eligible memory with its raw similarity to the query.
#[derive(Debug, PartialEq)]
pub struct Hit<'a> {
    pub memory_id: &'a str,
    pub record_type: Option<RecordType>,
    pub created_at: Option<i64>,
    pub score: f32,
}

/// Vectors of a user's memories, for one embedding model.
pub struct Index {
    model_id: String,
    vectors: Vec<f32>,
    rows: Vec<Row>,
    positions: HashMap<String, usize>,
}

impl Index {
    /// Creates an empty index for `model_id`. Vectors from other models are not comparable, so
    /// they stay out until re-embedded.
    pub fn new(model_id: &str) -> Self {
        Self {
            model_id: model_id.to_owned(),
            vectors: Vec::new(),
            rows: Vec::new(),
            positions: HashMap::new(),
        }
    }

    /// Adds or replaces a memory.
    pub fn insert(&mut self, memory: &StoredMemory) -> Result<(), DimMismatch> {
        let Some(vector) = memory.vector.as_deref() else {
            return Ok(());
        };
        if memory.model_id.as_deref() != Some(self.model_id.as_str()) {
            return Ok(());
        }
        if vector.len() != DIMENSIONS {
            return Err(DimMismatch {
                expected: DIMENSIONS,
                got: vector.len(),
            });
        }
        let record = &memory.record;
        let row = Row {
            memory_id: record.memory_id.clone(),
            record_type: record.record_type,
            scope: record.scope,
            project_id: record.project_id.clone(),
            created_at: record.created_at,
            live: !record.deleted && record.superseded_by.is_none(),
        };
        match self.positions.get(&record.memory_id) {
            Some(&position) => {
                self.vectors[position * DIMENSIONS..][..DIMENSIONS].copy_from_slice(vector);
                self.rows[position] = row;
            }
            None => {
                self.positions
                    .insert(record.memory_id.clone(), self.rows.len());
                self.vectors.extend_from_slice(vector);
                self.rows.push(row);
            }
        }
        Ok(())
    }

    /// Takes a memory out of search results.
    pub fn mark_not_live(&mut self, memory_id: &str) {
        if let Some(&position) = self.positions.get(memory_id) {
            self.rows[position].live = false;
        }
    }

    /// Scores every live memory eligible in `project_id`: user scope, or that project.
    pub fn search(&self, query: &[f32], project_id: &str) -> Vec<Hit<'_>> {
        self.rows
            .iter()
            .zip(self.vectors.as_chunks::<DIMENSIONS>().0)
            .filter(|(row, _)| {
                row.live
                    && match row.scope {
                        Some(Scope::User) => true,
                        Some(Scope::Project) => row.project_id.as_deref() == Some(project_id),
                        None => false,
                    }
            })
            .map(|(row, vector)| Hit {
                memory_id: &row.memory_id,
                record_type: row.record_type,
                created_at: row.created_at,
                score: dot(vector, query),
            })
            .collect()
    }

    /// The other live facts that say the same thing as the live fact `memory_id`: as similar as
    /// `DUPLICATE_SIMILARITY`, in the same scope and, for project scope, the same project.
    pub fn near_duplicates(&self, memory_id: &str) -> Vec<&str> {
        let Some(&position) = self.positions.get(memory_id) else {
            return Vec::new();
        };
        let fact = &self.rows[position];
        let Some(scope) = fact.scope else {
            return Vec::new();
        };
        let project_id = fact.project_id.as_deref();
        if !fact.is_live_fact() {
            return Vec::new();
        }
        let vectors = self.vectors.as_chunks::<DIMENSIONS>().0;
        let vector = &vectors[position];
        self.rows
            .iter()
            .zip(vectors)
            .enumerate()
            .filter(|&(other, (row, other_vector))| {
                other != position
                    && row.is_live_fact()
                    && row.in_scope(scope, project_id)
                    && dot(vector, other_vector) >= DUPLICATE_SIMILARITY
            })
            .map(|(_, (row, _))| row.memory_id.as_str())
            .collect()
    }

    /// The `k` live facts in `scope` most similar to `vector`, best first. For project scope, only
    /// facts of `project_id`.
    pub fn nearest_facts(
        &self,
        vector: &[f32],
        scope: Scope,
        project_id: &str,
        k: usize,
    ) -> Vec<(&str, f32)> {
        let mut nearest: Vec<(&str, f32)> = self
            .rows
            .iter()
            .zip(self.vectors.as_chunks::<DIMENSIONS>().0)
            .filter(|(row, _)| row.is_live_fact() && row.in_scope(scope, Some(project_id)))
            .map(|(row, other)| (row.memory_id.as_str(), dot(vector, other)))
            .collect();
        nearest.sort_by(|a, b| b.1.total_cmp(&a.1));
        nearest.truncate(k);
        nearest
    }
}

impl Row {
    fn is_live_fact(&self) -> bool {
        self.live && self.record_type == Some(RecordType::Fact)
    }

    fn in_scope(&self, scope: Scope, project_id: Option<&str>) -> bool {
        self.scope == Some(scope)
            && (scope == Scope::User || self.project_id.as_deref() == project_id)
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(a, b)| a * b).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loremaster_core::{Kind, Record};

    const MODEL: &str = "model-a";

    fn unit(hot: usize) -> Vec<f32> {
        let mut vector = vec![0.0; DIMENSIONS];
        vector[hot] = 1.0;
        vector
    }

    fn memory(id: &str, project: Option<&str>) -> StoredMemory {
        StoredMemory {
            record: Record {
                user_id: None,
                memory_id: id.into(),
                seq: None,
                hlc: "1761750000123-00002-w".into(),
                writer: "w".into(),
                record_type: Some(RecordType::Fact),
                kind: Some(Kind::Decision),
                scope: Some(project.map_or(Scope::User, |_| Scope::Project)),
                project_id: project.map(str::to_owned),
                text: Some("t".into()),
                created_at: Some(1),
                superseded_by: None,
                deleted: false,
                evict_key: None,
            },
            vector: Some(unit(0)),
            model_id: Some(MODEL.into()),
        }
    }

    fn sample_index() -> Index {
        let mut superseded = memory("D", None);
        superseded.record.superseded_by = Some("A".into());
        let mut deleted = memory("E", None);
        deleted.record.deleted = true;
        let mut index = Index::new(MODEL);
        for row in [
            memory("A", None),
            memory("B", Some("github.com/a/x")),
            memory("C", Some("github.com/a/y")),
            superseded,
            deleted,
        ] {
            index.insert(&row).unwrap();
        }
        index
    }

    fn ids(index: &Index, project: &str) -> Vec<String> {
        let mut ids: Vec<String> = index
            .search(&unit(0), project)
            .into_iter()
            .map(|hit| hit.memory_id.to_owned())
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn search_is_limited_to_user_scope_and_the_current_project() {
        let index = sample_index();
        assert_eq!(ids(&index, "github.com/a/x"), ["A", "B"]);
        assert_eq!(ids(&index, "github.com/a/y"), ["A", "C"]);
    }

    #[test]
    fn marking_not_live_removes_a_memory() {
        let mut index = sample_index();
        index.mark_not_live("A");
        index.mark_not_live("unknown");
        assert_eq!(ids(&index, "github.com/a/x"), ["B"]);
    }

    #[test]
    fn score_is_the_dot_product() {
        let mut index = Index::new(MODEL);
        index.insert(&memory("A", None)).unwrap();
        let hits = index.search(&unit(0), "p");
        assert_eq!(hits[0].score, 1.0);
        assert_eq!(index.search(&unit(1), "p")[0].score, 0.0);
    }

    #[test]
    fn inserting_an_id_again_replaces_it() {
        let mut index = Index::new(MODEL);
        index.insert(&memory("A", None)).unwrap();
        let mut newer = memory("A", None);
        newer.vector = Some(unit(1));
        index.insert(&newer).unwrap();
        let hits = index.search(&unit(1), "p");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].score, 1.0);
    }

    #[test]
    fn wrong_dimensions_are_rejected() {
        let mut row = memory("A", None);
        row.vector = Some(vec![0.0; 383]);
        assert_eq!(
            Index::new(MODEL).insert(&row),
            Err(DimMismatch {
                expected: 384,
                got: 383
            })
        );
    }

    #[test]
    fn empty_index_finds_nothing() {
        assert!(Index::new(MODEL).search(&unit(0), "p").is_empty());
    }

    #[test]
    fn other_models_and_missing_vectors_stay_out() {
        let mut index = Index::new(MODEL);
        let mut other_model = memory("A", None);
        other_model.model_id = Some("model-b".into());
        index.insert(&other_model).unwrap();
        let mut no_vector = memory("B", None);
        no_vector.vector = None;
        index.insert(&no_vector).unwrap();
        assert!(index.search(&unit(0), "p").is_empty());
    }

    #[test]
    fn row_without_a_scope_is_not_eligible() {
        let mut row = memory("A", None);
        row.record.scope = None;
        let mut index = Index::new(MODEL);
        index.insert(&row).unwrap();
        assert!(index.search(&unit(0), "p").is_empty());
    }

    /// A unit vector with `similarity` to `unit(0)`, the rest on axis `other`.
    fn toward(similarity: f32, other: usize) -> Vec<f32> {
        let mut vector = unit(0);
        vector[0] = similarity;
        vector[other] = (1.0 - similarity * similarity).sqrt();
        vector
    }

    fn duplicates(index: &Index, memory_id: &str) -> Vec<String> {
        let mut ids: Vec<String> = index
            .near_duplicates(memory_id)
            .into_iter()
            .map(str::to_owned)
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn near_duplicates_are_live_facts_at_the_threshold_in_the_same_scope() {
        let mut close = memory("close", None);
        close.vector = Some(toward(0.96, 1));
        let mut far = memory("far", None);
        far.vector = Some(toward(0.94, 2));
        let mut episode = memory("episode", None);
        episode.record.record_type = Some(RecordType::Episode);
        let mut unscoped = memory("unscoped", None);
        unscoped.record.scope = None;
        let mut index = sample_index();
        for row in [close, far, episode, unscoped, memory("gone", None)] {
            index.insert(&row).unwrap();
        }
        index.mark_not_live("gone");

        assert_eq!(duplicates(&index, "A"), ["close"]);
        assert_eq!(duplicates(&index, "close"), ["A"]);
        assert!(duplicates(&index, "B").is_empty());
        assert!(duplicates(&index, "unscoped").is_empty());
        assert!(duplicates(&index, "unknown").is_empty());
    }

    #[test]
    fn project_facts_are_duplicates_only_in_the_same_project() {
        let mut index = sample_index();
        index.insert(&memory("B2", Some("github.com/a/x"))).unwrap();
        assert_eq!(duplicates(&index, "B"), ["B2"]);
        assert!(duplicates(&index, "C").is_empty());
    }

    #[test]
    fn a_memory_that_is_not_a_live_fact_has_no_duplicates() {
        let mut episode = memory("episode", None);
        episode.record.record_type = Some(RecordType::Episode);
        let mut index = sample_index();
        index.insert(&episode).unwrap();
        for id in ["D", "E", "episode"] {
            assert!(duplicates(&index, id).is_empty(), "{id}");
        }
    }

    fn nearest(index: &Index, scope: Scope, project_id: &str, k: usize) -> Vec<(String, f32)> {
        index
            .nearest_facts(&unit(0), scope, project_id, k)
            .into_iter()
            .map(|(id, score)| (id.to_owned(), score))
            .collect()
    }

    #[test]
    fn nearest_facts_are_live_facts_in_the_scope_best_first() {
        let mut index = sample_index();
        for (id, similarity, axis) in [("near", 0.9, 1), ("mid", 0.5, 2), ("low", 0.1, 3)] {
            let mut row = memory(id, None);
            row.vector = Some(toward(similarity, axis));
            index.insert(&row).unwrap();
        }
        let mut episode = memory("episode", None);
        episode.record.record_type = Some(RecordType::Episode);
        index.insert(&episode).unwrap();
        index.insert(&memory("gone", None)).unwrap();
        index.mark_not_live("gone");

        let found = nearest(&index, Scope::User, "github.com/a/x", 10);
        let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["A", "near", "mid", "low"]);
        assert!((found[1].1 - 0.9).abs() < 1e-6);
        let top: Vec<String> = nearest(&index, Scope::User, "github.com/a/x", 2)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(top, ["A", "near"]);
        assert!(nearest(&index, Scope::User, "p", 0).is_empty());
    }

    #[test]
    fn nearest_project_facts_are_from_that_project_only() {
        let index = sample_index();
        let ids = |project: &str| -> Vec<String> {
            nearest(&index, Scope::Project, project, 5)
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        };
        assert_eq!(ids("github.com/a/x"), ["B"]);
        assert_eq!(ids("github.com/a/y"), ["C"]);
        assert!(ids("github.com/a/z").is_empty());
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn search_over_50k_memories_is_fast() {
        let mut index = Index::new(MODEL);
        let mut base = memory("base", None);
        for i in 0..50_000 {
            base.record.memory_id = format!("m{i}");
            let mut vector = unit(i % DIMENSIONS);
            vector[(i * 7) % DIMENSIONS] += 0.5;
            let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            vector.iter_mut().for_each(|v| *v /= norm);
            base.vector = Some(vector);
            index.insert(&base).unwrap();
        }
        let started = std::time::Instant::now();
        let hits = index.search(&unit(0), "p");
        let elapsed = started.elapsed();
        assert_eq!(hits.len(), 50_000);
        assert!(elapsed.as_millis() < 20, "{elapsed:?}");
    }
}
