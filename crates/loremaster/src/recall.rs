//! Chooses what to inject: a profile at session start, relevant memories on each prompt.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock};

use loremaster_core::{Kind, PullResponse, Record, RecordType, Scope};

use crate::embed::{EmbedError, Embedder, MODEL_ID};
use crate::index::{DimMismatch, Index};
use crate::store::{Store, StoreError, StoredMemory};

const PROFILE_HEADER: &str =
    "Loremaster memory from past sessions. Newer user instructions override it.";
const PROMPT_HEADER: &str = "Loremaster memories relevant to this prompt:";
const PROFILE_MAX_CHARS: usize = 300 * 3;
const PROFILE_USER_FIRST_CHARS: usize = 150 * 3;
const PROMPT_MAX_CHARS: usize = 500 * 3;
const RELEVANCE_THRESHOLD: f32 = 0.6;
const MAX_QUERY_CHARS: usize = 500;
/// Far more than fit in the cap. Each one costs store lookups inside the hook's deadline.
const MAX_PROMPT_CANDIDATES: usize = 200;
const PROFILE_HALF_LIFE_DAYS: f64 = 90.0;
const EPISODE_HALF_LIFE_DAYS: f64 = 30.0;
const MS_PER_DAY: f64 = 86_400_000.0;

/// Why recall could not answer.
#[derive(Debug, thiserror::Error)]
pub enum RecallError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error(transparent)]
    Index(#[from] DimMismatch),
}

fn kind_weight(kind: Kind) -> f64 {
    match kind {
        Kind::Correction => 1.0,
        Kind::Preference => 0.9,
        Kind::Decision => 0.8,
        Kind::Environment => 0.7,
        Kind::Pitfall => 0.6,
    }
}

fn age_days(created_at: Option<i64>, now_ms: i64) -> f64 {
    (now_ms - created_at.unwrap_or(now_ms)).max(0) as f64 / MS_PER_DAY
}

fn half_life_decay(days: f64, half_life: f64) -> f64 {
    0.5_f64.powf(days / half_life)
}

fn profile_score(record: &Record, now_ms: i64) -> f64 {
    record.kind.map_or(0.0, kind_weight)
        * half_life_decay(age_days(record.created_at, now_ms), PROFILE_HALF_LIFE_DAYS)
}

fn fact_line(record: &Record) -> Option<String> {
    Some(format!(
        "- [{}] {}",
        record.kind?.as_str(),
        record.text.as_deref()?
    ))
}

fn memory_line(record: &Record, now_ms: i64) -> Option<String> {
    if record.record_type == Some(RecordType::Episode) {
        let days = age_days(record.created_at, now_ms) as i64;
        let unit = if days == 1 { "day" } else { "days" };
        return Some(format!(
            "- [episode, {days} {unit} ago] {}",
            record.text.as_deref()?
        ));
    }
    fact_line(record)
}

/// Adds lines in score order to a section, within a budget of characters of finished text.
struct ProfileSection {
    title: String,
    lines: Vec<(usize, String)>,
}

impl ProfileSection {
    fn new(title: String) -> Self {
        Self {
            title,
            lines: Vec::new(),
        }
    }

    fn try_add(&mut self, rank: usize, line: &str, used: &mut usize, limit: usize) -> bool {
        let heading = if self.lines.is_empty() {
            self.title.chars().count() + 1
        } else {
            0
        };
        let cost = heading + line.chars().count() + 1;
        if *used + cost > limit {
            return false;
        }
        *used += cost;
        self.lines.push((rank, line.to_owned()));
        true
    }

    fn render_into(mut self, output: &mut Vec<String>) {
        if self.lines.is_empty() {
            return;
        }
        self.lines.sort_by_key(|(rank, _)| *rank);
        output.push(self.title);
        output.extend(self.lines.into_iter().map(|(_, line)| line));
    }
}

/// Memories chosen for injection: the text, and the ids to mark as injected once it has reached the
/// agent.
#[derive(Debug, PartialEq, Eq)]
pub struct Recalled {
    pub text: String,
    pub memory_ids: Vec<String>,
}

/// Holds local memory and answers recall from it.
pub struct Recall {
    store: Mutex<Store>,
    index: RwLock<Index>,
    embedder: OnceLock<Embedder>,
    data_version: AtomicI64,
}

impl Recall {
    /// Builds the search index from `store`.
    pub fn new(store: Store) -> Result<Self, RecallError> {
        let index = load_index(&store)?;
        let data_version = AtomicI64::new(store.data_version()?);
        Ok(Self {
            store: Mutex::new(store),
            index: RwLock::new(index),
            embedder: OnceLock::new(),
            data_version,
        })
    }

    /// Locks the store. Hold it only briefly: recall waits on it.
    pub fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Makes the embedding model available once it has loaded, then brings memories embedded with
    /// another model up to date.
    pub fn set_embedder(&self, embedder: Embedder) -> Result<(), RecallError> {
        let embedder = self.embedder.get_or_init(|| embedder);
        let mut stale = Vec::new();
        self.store().for_each_memory(|memory| {
            if memory.model_id.as_deref() != Some(MODEL_ID) {
                stale.push(memory);
            }
        })?;
        self.add_vectors(embedder, stale)
    }

    /// Stores the memories of a pulled page that are newer than the local copies, saves the
    /// cursor after it, and updates the index in place. Returns how many it stored.
    pub fn apply_pulled(&self, page: PullResponse, writer: &str) -> Result<usize, RecallError> {
        let embedder = self.embedder.get();
        let mut newer = Vec::new();
        {
            let store = self.store();
            for record in page.records {
                // This device's own pushes come back too; they are already here.
                if store
                    .hlc_of(&record.memory_id)?
                    .is_none_or(|local| record.hlc > local)
                {
                    newer.push(record);
                }
            }
        }
        // Embedding is slow, so it runs with the store unlocked. Each call locks the model, and a
        // prompt needs it too, so one text per call lets recall run between them.
        let mut vectors = Vec::new();
        if let Some(embedder) = embedder {
            for text in newer.iter().filter_map(embeddable_text) {
                vectors.extend(embedder.embed(&[text])?);
            }
        }
        let mut vectors = vectors.into_iter();
        let memories = newer
            .into_iter()
            .map(|record| {
                let vector = embeddable_text(&record).and_then(|_| vectors.next());
                StoredMemory {
                    model_id: vector.as_ref().map(|_| MODEL_ID.to_owned()),
                    vector,
                    record: Record {
                        user_id: None,
                        evict_key: None,
                        ..record
                    },
                }
            })
            .collect();
        let stored = self
            .store()
            .apply_pulled(memories, page.next_cursor, writer)?;
        {
            let mut index = self.index.write().unwrap_or_else(|e| e.into_inner());
            for memory in &stored {
                if memory.vector.is_some() {
                    index.insert(memory)?;
                } else {
                    index.mark_not_live(&memory.record.memory_id);
                }
            }
        }
        let count = stored.len();
        // The model finished loading during this page, after its backfill read the store.
        if embedder.is_none()
            && let Some(embedder) = self.embedder.get()
        {
            self.add_vectors(embedder, stored)?;
        }
        Ok(count)
    }

    /// Embeds memories that have text and adds them to the index. A memory changed since it was
    /// read keeps its new version.
    fn add_vectors(
        &self,
        embedder: &Embedder,
        memories: Vec<StoredMemory>,
    ) -> Result<(), RecallError> {
        for mut memory in memories {
            let Some(text) = embeddable_text(&memory.record) else {
                continue;
            };
            let Some(vector) = embedder.embed(&[text])?.pop() else {
                continue;
            };
            let record = &memory.record;
            if self
                .store()
                .set_vector(&record.memory_id, &record.hlc, &vector, MODEL_ID)?
            {
                memory.vector = Some(vector);
                memory.model_id = Some(MODEL_ID.to_owned());
                self.index
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(&memory)?;
            }
        }
        Ok(())
    }

    /// Builds the session-start profile. Needs no embeddings, so it works before the model loads.
    pub fn profile(&self, project_id: &str, now_ms: i64) -> Result<Option<Recalled>, RecallError> {
        let mut facts = self.store().live_facts(project_id)?;
        facts.sort_by(|a, b| {
            profile_score(b, now_ms)
                .partial_cmp(&profile_score(a, now_ms))
                .unwrap_or(Ordering::Equal)
        });

        let is_user = |record: &Record| record.scope == Some(Scope::User);
        let mut used = PROFILE_HEADER.chars().count();
        let mut user = ProfileSection::new("User:".to_owned());
        let mut project = ProfileSection::new(format!("Project {project_id}:"));
        let mut deferred = Vec::new();
        for (rank, fact) in facts.iter().enumerate().filter(|(_, f)| is_user(f)) {
            if let Some(line) = fact_line(fact)
                && !user.try_add(rank, &line, &mut used, PROFILE_USER_FIRST_CHARS)
            {
                deferred.push((rank, line));
            }
        }
        for (rank, fact) in facts.iter().enumerate().filter(|(_, f)| !is_user(f)) {
            if let Some(line) = fact_line(fact) {
                project.try_add(rank, &line, &mut used, PROFILE_MAX_CHARS);
            }
        }
        for (rank, line) in deferred {
            user.try_add(rank, &line, &mut used, PROFILE_MAX_CHARS);
        }

        let memory_ids: Vec<String> = [&user, &project]
            .iter()
            .flat_map(|section| &section.lines)
            .map(|(rank, _)| facts[*rank].memory_id.clone())
            .collect();
        let mut output = vec![PROFILE_HEADER.to_owned()];
        user.render_into(&mut output);
        project.render_into(&mut output);
        Ok((!memory_ids.is_empty()).then(|| Recalled {
            text: output.join("\n"),
            memory_ids,
        }))
    }

    /// Picks memories relevant to a prompt that this session has not been given yet.
    pub fn for_prompt(
        &self,
        session_id: &str,
        project_id: &str,
        prompt: &str,
        now_ms: i64,
    ) -> Result<Option<Recalled>, RecallError> {
        let Some(embedder) = self.embedder.get() else {
            return Ok(None);
        };
        if prompt.trim().is_empty() {
            return Ok(None);
        }
        let query = embedder
            .embed(&[query_text(prompt)])?
            .pop()
            .unwrap_or_default();
        self.refresh_index()?;

        let mut hits: Vec<(String, f32)> = {
            let index = self.index.read().unwrap_or_else(|e| e.into_inner());
            index
                .search(&query, project_id)
                .into_iter()
                .filter_map(|hit| {
                    let score = if hit.record_type == Some(RecordType::Episode) {
                        let days = age_days(hit.created_at, now_ms);
                        hit.score * half_life_decay(days, EPISODE_HALF_LIFE_DAYS) as f32
                    } else {
                        hit.score
                    };
                    (score >= RELEVANCE_THRESHOLD).then(|| (hit.memory_id.to_owned(), score))
                })
                .collect()
        };
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        hits.truncate(MAX_PROMPT_CANDIDATES);

        let store = self.store();
        let mut used = PROMPT_HEADER.chars().count();
        let mut lines = vec![PROMPT_HEADER.to_owned()];
        let mut memory_ids = Vec::new();
        for (memory_id, _) in hits {
            if store.was_injected(session_id, &memory_id)? {
                continue;
            }
            let memory = store.get_memory(&memory_id)?;
            let Some(line) = memory.and_then(|m| memory_line(&m.record, now_ms)) else {
                continue;
            };
            let cost = line.chars().count() + 1;
            if used + cost > PROMPT_MAX_CHARS {
                continue;
            }
            used += cost;
            lines.push(line);
            memory_ids.push(memory_id);
        }
        Ok((!memory_ids.is_empty()).then(|| Recalled {
            text: lines.join("\n"),
            memory_ids,
        }))
    }

    /// Records memories as delivered, so this session does not get them again.
    pub fn mark_injected(
        &self,
        session_id: &str,
        memory_ids: &[String],
    ) -> Result<(), RecallError> {
        let store = self.store();
        for memory_id in memory_ids {
            store.mark_injected(session_id, memory_id)?;
        }
        Ok(())
    }

    /// Rebuilds the index when another process, such as a seeding tool, has changed the store.
    fn refresh_index(&self) -> Result<(), RecallError> {
        let store = self.store();
        let version = store.data_version()?;
        if self.data_version.swap(version, AtomicOrdering::Relaxed) != version {
            let index = load_index(&store)?;
            *self.index.write().unwrap_or_else(|e| e.into_inner()) = index;
        }
        Ok(())
    }
}

/// The text to embed: a memory that is not deleted and has text.
fn embeddable_text(record: &Record) -> Option<&str> {
    record.text.as_deref().filter(|_| !record.deleted)
}

/// The part of a prompt to embed. Embedding time grows with length and must fit the hook's
/// deadline, and the end of a long prompt is usually what is being asked.
pub fn query_text(prompt: &str) -> &str {
    let start = prompt
        .char_indices()
        .rev()
        .nth(MAX_QUERY_CHARS - 1)
        .map_or(0, |(index, _)| index);
    &prompt[start..]
}

fn load_index(store: &Store) -> Result<Index, RecallError> {
    let mut index = Index::new(MODEL_ID);
    store.for_each_memory(|memory| {
        if let Err(e) = index.insert(&memory) {
            tracing::error!(memory = %memory.record.memory_id, error = %e, "skipped memory");
        }
    })?;
    Ok(index)
}
