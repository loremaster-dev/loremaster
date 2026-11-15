mod common;

use common::{DeliveredRecall, Sandbox, embedder, memory};
use loremaster::embed::MODEL_ID;
use loremaster::now_ms;
use loremaster::recall::Recall;
use loremaster::store::{Store, StoredMemory};
use loremaster_core::{Kind, estimate_tokens};

const WEB: &str = "github.com/acme/web";
const DEPLOY: &str = "Deploys go through ArgoCD, not manual kubectl";
const DAY_MS: i64 = 86_400_000;

fn store_with(memories: &[StoredMemory]) -> (Sandbox, Store) {
    let sandbox = Sandbox::without_model();
    let store = sandbox.store();
    store.put_memories(memories).unwrap();
    (sandbox, store)
}

fn recall_with(memories: &[StoredMemory]) -> (Sandbox, Recall) {
    let (sandbox, store) = store_with(memories);
    let recall = Recall::new(store).unwrap();
    recall.set_embedder(embedder(), None).unwrap();
    (sandbox, recall)
}

fn sample() -> Vec<StoredMemory> {
    let model = embedder();
    let now = now_ms();
    vec![
        memory(
            &model,
            Some(Kind::Preference),
            None,
            "Prefers pnpm over npm",
            now,
        ),
        memory(&model, Some(Kind::Decision), Some(WEB), DEPLOY, now),
    ]
}

#[test]
fn profile_lists_user_then_project_facts() {
    let (_sandbox, recall) = recall_with(&sample());
    assert_eq!(
        recall.profile(WEB, now_ms()).unwrap().unwrap().text,
        "Loremaster memory from past sessions. Newer user instructions override it.\n\
         User:\n\
         - [preference] Prefers pnpm over npm\n\
         Project github.com/acme/web:\n\
         - [decision] Deploys go through ArgoCD, not manual kubectl"
    );
}

#[test]
fn profile_in_another_project_has_only_the_user_section() {
    let (_sandbox, recall) = recall_with(&sample());
    assert_eq!(
        recall
            .profile("github.com/acme/api", now_ms())
            .unwrap()
            .unwrap()
            .text,
        "Loremaster memory from past sessions. Newer user instructions override it.\n\
         User:\n\
         - [preference] Prefers pnpm over npm"
    );
}

#[test]
fn profile_with_no_memories_is_none() {
    let (_sandbox, recall) = recall_with(&[]);
    assert_eq!(recall.profile(WEB, now_ms()).unwrap(), None);
}

fn unembedded_fact(kind: Kind, project: Option<&str>, text: &str, created_at: i64) -> StoredMemory {
    let mut memory = StoredMemory {
        record: serde_json::from_value(serde_json::json!({
            "memory_id": loremaster_core::new_ulid(),
            "hlc": "1761750000123-00000-t",
            "writer": "t",
            "type": "fact",
            "kind": kind,
            "scope": if project.is_some() { "project" } else { "user" },
            "project_id": project,
            "text": text,
            "created_at": created_at,
        }))
        .unwrap(),
        vector: None,
        model_id: Some(MODEL_ID.to_owned()),
    };
    memory.vector = None;
    memory
}

#[test]
fn profile_stays_under_its_cap_with_the_best_fact_first() {
    let now = now_ms();
    let mut memories: Vec<StoredMemory> = (0..199)
        .map(|i| {
            unembedded_fact(
                Kind::Pitfall,
                None,
                &format!("Older pitfall number {i} to remember"),
                now - DAY_MS * (i + 30),
            )
        })
        .collect();
    memories.push(unembedded_fact(
        Kind::Correction,
        None,
        "The best fact",
        now,
    ));
    let (_sandbox, store) = store_with(&memories);
    let recall = Recall::new(store).unwrap();
    let profile = recall.profile(WEB, now).unwrap().unwrap().text;
    assert!(
        estimate_tokens(&profile) <= 300,
        "{}",
        estimate_tokens(&profile)
    );
    assert_eq!(profile.lines().nth(2), Some("- [correction] The best fact"));
}

#[test]
fn profile_reserves_room_for_project_facts_then_fills_with_user_facts() {
    let now = now_ms();
    let mut memories: Vec<StoredMemory> = (0..40)
        .map(|i| {
            unembedded_fact(
                Kind::Correction,
                None,
                &format!("User correction number {i} worth keeping around"),
                now,
            )
        })
        .collect();
    memories.push(unembedded_fact(
        Kind::Pitfall,
        Some(WEB),
        "A project pitfall",
        now - DAY_MS * 400,
    ));
    let (_sandbox, store) = store_with(&memories);
    let profile = Recall::new(store)
        .unwrap()
        .profile(WEB, now)
        .unwrap()
        .unwrap()
        .text;
    assert!(profile.contains("- [pitfall] A project pitfall"));
    assert!(profile.chars().count() > 450, "user facts fill the rest");
    assert!(estimate_tokens(&profile) <= 300);
}

#[test]
fn a_fact_too_long_for_the_budget_is_skipped_and_shorter_ones_still_fit() {
    let now = now_ms();
    let memories = [
        unembedded_fact(Kind::Correction, None, &"x".repeat(1000), now),
        unembedded_fact(Kind::Preference, None, "Short and useful", now),
    ];
    let (_sandbox, store) = store_with(&memories);
    let profile = Recall::new(store)
        .unwrap()
        .profile(WEB, now)
        .unwrap()
        .unwrap()
        .text;
    assert!(profile.contains("Short and useful"));
    assert!(!profile.contains("xxx"));
    assert!(estimate_tokens(&profile) <= 300);
}

#[test]
fn prompt_gets_its_relevant_memory_once_per_session() {
    let (_sandbox, recall) = recall_with(&sample());
    let expected = "Loremaster memories relevant to this prompt:\n\
                    - [decision] Deploys go through ArgoCD, not manual kubectl";
    let now = now_ms();
    let prompt = "how do we deploy?";
    assert_eq!(
        recall
            .for_prompt_delivered("s1", WEB, prompt, now)
            .unwrap()
            .as_deref(),
        Some(expected)
    );
    assert_eq!(
        recall.for_prompt_delivered("s1", WEB, prompt, now).unwrap(),
        None
    );
    assert_eq!(
        recall
            .for_prompt_delivered("s2", WEB, prompt, now)
            .unwrap()
            .as_deref(),
        Some(expected)
    );
}

#[test]
fn unrelated_prompt_gets_nothing() {
    let (_sandbox, recall) = recall_with(&sample());
    assert_eq!(
        recall
            .for_prompt_delivered("s1", WEB, "what's the weather in Oslo?", now_ms())
            .unwrap(),
        None
    );
}

#[test]
fn project_memory_is_not_recalled_in_another_project() {
    let (_sandbox, recall) = recall_with(&sample());
    assert_eq!(
        recall
            .for_prompt_delivered("s1", "github.com/acme/api", "how do we deploy?", now_ms())
            .unwrap(),
        None
    );
}

#[test]
fn episode_shows_its_age() {
    let model = embedder();
    let now = now_ms();
    let text = "Tried upgrading to Vite 6; build failed on the SSR plugin; reverted";
    let episode = memory(&model, None, Some(WEB), text, now - DAY_MS * 12);
    let (_sandbox, recall) = recall_with(&[episode]);
    let output = recall
        .for_prompt_delivered(
            "s1",
            WEB,
            "Tried upgrading to Vite 6 and the SSR plugin build failed",
            now,
        )
        .unwrap()
        .unwrap();
    assert!(
        output.ends_with(&format!("- [episode, 12 days ago] {text}")),
        "{output}"
    );
}

#[test]
fn episode_from_yesterday_uses_the_singular() {
    let model = embedder();
    let now = now_ms();
    let text = "Migrated the billing tables and ran the backfill";
    let episode = memory(&model, None, None, text, now - DAY_MS - 1000);
    let (_sandbox, recall) = recall_with(&[episode]);
    let output = recall
        .for_prompt_delivered("s1", WEB, text, now)
        .unwrap()
        .unwrap();
    assert!(output.contains("[episode, 1 day ago]"), "{output}");
}

#[test]
fn newer_episode_ranks_above_an_older_equal_one_and_old_ones_fade_out() {
    let model = embedder();
    let now = now_ms();
    let text = "Migrated the billing tables and ran the backfill";
    let newer = memory(&model, None, None, text, now - DAY_MS);
    let older = memory(&model, None, None, text, now - DAY_MS * 10);
    let ancient = memory(&model, None, None, text, now - DAY_MS * 500);
    let (_sandbox, recall) = recall_with(&[older, ancient, newer]);
    let output = recall
        .for_prompt_delivered("s1", WEB, text, now)
        .unwrap()
        .unwrap();
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 3, "{output}");
    assert!(lines[1].contains("1 day ago"), "{output}");
    assert!(lines[2].contains("10 days ago"), "{output}");
}

#[test]
fn many_matching_facts_stay_under_the_prompt_cap() {
    let model = embedder();
    let now = now_ms();
    let memories: Vec<StoredMemory> = (0..100)
        .map(|i| {
            memory(
                &model,
                Some(Kind::Decision),
                None,
                &format!(
                    "How we deploy: variant {i} goes through ArgoCD with careful review steps"
                ),
                now,
            )
        })
        .collect();
    let (_sandbox, recall) = recall_with(&memories);
    let output = recall
        .for_prompt_delivered("s1", WEB, "how do we deploy with ArgoCD?", now)
        .unwrap()
        .unwrap();
    assert!(
        estimate_tokens(&output) <= 500,
        "{}",
        estimate_tokens(&output)
    );
    assert!(output.lines().count() > 5);
}

#[test]
fn without_the_embedder_prompts_get_nothing_but_the_profile_works() {
    let (_sandbox, store) = store_with(&sample());
    let recall = Recall::new(store).unwrap();
    assert_eq!(
        recall
            .for_prompt_delivered("s1", WEB, "how do we deploy?", now_ms())
            .unwrap(),
        None
    );
    assert!(recall.profile(WEB, now_ms()).unwrap().is_some());
}

#[test]
fn blank_prompts_get_nothing() {
    let (_sandbox, recall) = recall_with(&sample());
    for prompt in ["", "   \n\t"] {
        assert_eq!(
            recall
                .for_prompt_delivered("s1", WEB, prompt, now_ms())
                .unwrap(),
            None
        );
    }
}

#[test]
fn superseded_and_deleted_memories_are_never_injected() {
    let mut memories = sample();
    let model = embedder();
    let mut superseded = memory(
        &model,
        Some(Kind::Decision),
        None,
        "How we deploy: use FTP uploads",
        now_ms(),
    );
    superseded.record.superseded_by = Some(memories[1].record.memory_id.clone());
    let mut deleted = memory(
        &model,
        Some(Kind::Decision),
        None,
        "How we deploy: use rsync by hand",
        now_ms(),
    );
    deleted.record.deleted = true;
    memories.extend([superseded, deleted]);
    let (_sandbox, recall) = recall_with(&memories);
    let output = recall
        .for_prompt_delivered("s1", WEB, "how do we deploy?", now_ms())
        .unwrap()
        .unwrap();
    assert!(output.contains("ArgoCD"));
    assert!(
        !output.contains("FTP") && !output.contains("rsync"),
        "{output}"
    );
    let profile = recall.profile(WEB, now_ms()).unwrap().unwrap().text;
    assert!(
        !profile.contains("FTP") && !profile.contains("rsync"),
        "{profile}"
    );
}

#[test]
fn memories_written_by_another_process_are_found() {
    let (sandbox, recall) = recall_with(&[]);
    let model = embedder();
    let other = sandbox.store();
    other
        .put_memory(&memory(
            &model,
            Some(Kind::Decision),
            Some(WEB),
            DEPLOY,
            now_ms(),
        ))
        .unwrap();
    let output = recall
        .for_prompt_delivered("s1", WEB, "how do we deploy?", now_ms())
        .unwrap();
    assert!(output.unwrap().contains("ArgoCD"));
}

#[test]
fn memories_from_another_model_are_excluded_until_re_embedded() {
    let model = embedder();
    let mut old = memory(&model, Some(Kind::Decision), Some(WEB), DEPLOY, now_ms());
    old.model_id = Some("older-model".to_owned());
    old.vector = Some(vec![0.0; 10]);
    let mut deleted = memory(&model, Some(Kind::Decision), None, "gone", now_ms());
    deleted.model_id = Some("older-model".to_owned());
    deleted.record.deleted = true;
    let id = old.record.memory_id.clone();
    let (sandbox, store) = store_with(&[old, deleted]);
    let recall = Recall::new(store).unwrap();
    recall.set_embedder(embedder(), None).unwrap();
    let output = recall
        .for_prompt_delivered("s1", WEB, "how do we deploy?", now_ms())
        .unwrap();
    assert!(output.unwrap().contains("ArgoCD"));
    let stored = sandbox.store().get_memory(&id).unwrap().unwrap();
    assert_eq!(stored.model_id.as_deref(), Some(MODEL_ID));
    assert_eq!(stored.vector.unwrap().len(), 384);
}

#[test]
fn stored_vector_of_the_wrong_size_is_skipped() {
    let model = embedder();
    let now = now_ms();
    let mut bad = memory(&model, Some(Kind::Decision), None, "x", now);
    bad.vector = Some(vec![0.0; 10]);
    let good = memory(&model, Some(Kind::Decision), Some(WEB), DEPLOY, now);
    let (_sandbox, recall) = recall_with(&[bad, good]);
    let output = recall
        .for_prompt_delivered("s1", WEB, "how do we deploy?", now)
        .unwrap();
    assert!(output.unwrap().contains("ArgoCD"));
}

#[test]
fn query_text_keeps_the_end_of_a_long_prompt() {
    use loremaster::recall::query_text;
    assert_eq!(query_text("short"), "short");
    let long = format!("{}the question", "é".repeat(2000));
    let query = query_text(&long);
    assert_eq!(query.chars().count(), 500);
    assert!(query.ends_with("the question"));
}

#[test]
fn a_matching_row_with_no_text_or_kind_is_skipped() {
    let model = embedder();
    let now = now_ms();
    let mut no_text = memory(&model, Some(Kind::Decision), Some(WEB), DEPLOY, now);
    no_text.record.text = None;
    let (_sandbox, recall) = recall_with(&[no_text]);
    assert_eq!(
        recall
            .for_prompt_delivered("s1", WEB, "how do we deploy?", now)
            .unwrap(),
        None
    );
}

#[test]
fn memories_stay_eligible_until_their_delivery_is_marked() {
    let (_sandbox, recall) = recall_with(&sample());
    let now = now_ms();
    let first = recall
        .for_prompt("s1", WEB, "how do we deploy?", now)
        .unwrap()
        .unwrap();
    assert_eq!(first.memory_ids.len(), 1);
    let again = recall
        .for_prompt("s1", WEB, "how do we deploy?", now)
        .unwrap();
    assert_eq!(again.unwrap().text, first.text);
    recall.mark_injected("s1", &first.memory_ids).unwrap();
    assert_eq!(
        recall
            .for_prompt("s1", WEB, "how do we deploy?", now)
            .unwrap(),
        None
    );
}

#[test]
fn profile_reports_which_memories_it_used() {
    let memories = sample();
    let mut expected: Vec<String> = memories
        .iter()
        .map(|m| m.record.memory_id.clone())
        .collect();
    expected.sort();
    let (_sandbox, recall) = recall_with(&memories);
    let mut ids = recall.profile(WEB, now_ms()).unwrap().unwrap().memory_ids;
    ids.sort();
    assert_eq!(ids, expected);
}
