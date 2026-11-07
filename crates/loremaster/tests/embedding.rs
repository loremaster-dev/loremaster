mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;

use common::{embedder, shared_models};
use loremaster::embed::{self, EmbedError, Embedder, MODEL_FILES, ModelFile};

/// Serves `body` to every request, cutting the connection short after `send` bytes.
fn serve(body: &'static [u8], send: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body[..send]);
        }
    });
    format!("http://127.0.0.1:{port}")
}

const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

fn files_in(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn download_keeps_a_verified_file() {
    let dir = tempfile::tempdir().unwrap();
    let url = serve(b"hello", 5);
    let file = ModelFile {
        name: "a.bin",
        sha256: HELLO_SHA256,
    };
    embed::ensure_model(dir.path(), &url, &[file]).unwrap();
    assert_eq!(fs::read(dir.path().join("a.bin")).unwrap(), b"hello");
}

#[test]
fn partial_file_from_a_killed_download_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".tmpXhCXFs"), "partial").unwrap();
    fs::write(dir.path().join("other.bin"), "kept").unwrap();
    let url = serve(b"hello", 5);
    let file = ModelFile {
        name: "a.bin",
        sha256: HELLO_SHA256,
    };
    embed::ensure_model(dir.path(), &url, &[file]).unwrap();
    let mut files = files_in(dir.path());
    files.sort();
    assert_eq!(files, ["a.bin", "other.bin"]);
}

#[test]
fn existing_file_is_not_downloaded_again() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.bin"), "kept").unwrap();
    let file = ModelFile {
        name: "a.bin",
        sha256: HELLO_SHA256,
    };
    embed::ensure_model(dir.path(), "http://127.0.0.1:1", &[file]).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("a.bin")).unwrap(),
        "kept"
    );
}

#[test]
fn wrong_hash_is_rejected_and_not_kept() {
    let dir = tempfile::tempdir().unwrap();
    let url = serve(b"tampered", 8);
    let file = ModelFile {
        name: "a.bin",
        sha256: HELLO_SHA256,
    };
    let error = embed::ensure_model(dir.path(), &url, &[file]).unwrap_err();
    assert!(matches!(error, EmbedError::HashMismatch { file } if file == "a.bin"));
    assert!(files_in(dir.path()).is_empty());
}

#[test]
fn no_network_is_a_download_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = ModelFile {
        name: "a.bin",
        sha256: HELLO_SHA256,
    };
    let error = embed::ensure_model(dir.path(), "http://127.0.0.1:1", &[file]).unwrap_err();
    assert!(matches!(error, EmbedError::Download { .. }), "{error}");
}

#[test]
fn connection_dropped_mid_download_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let url = serve(b"hello", 2);
    let file = ModelFile {
        name: "a.bin",
        sha256: HELLO_SHA256,
    };
    let error = embed::ensure_model(dir.path(), &url, &[file]).unwrap_err();
    assert!(matches!(error, EmbedError::Download { .. }), "{error}");
    assert!(files_in(dir.path()).is_empty());
}

#[test]
fn pinned_model_has_the_expected_files_and_sizes() {
    let dir = embed::model_dir(&shared_models());
    for (name, size) in [
        ("model_optimized.onnx", 66_465_124),
        ("tokenizer.json", 711_396),
        ("config.json", 706),
        ("special_tokens_map.json", 695),
        ("tokenizer_config.json", 1_242),
    ] {
        assert_eq!(fs::metadata(dir.join(name)).unwrap().len(), size, "{name}");
    }
    assert_eq!(MODEL_FILES.len(), 5);
}

#[test]
fn loading_from_an_empty_directory_fails() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(Embedder::load(dir.path()), Err(EmbedError::Io(_))));
}

#[test]
fn corrupt_model_is_a_model_error() {
    let dir = tempfile::tempdir().unwrap();
    let source = embed::model_dir(&shared_models());
    for file in MODEL_FILES {
        if file.name == "model_optimized.onnx" {
            fs::write(dir.path().join(file.name), "not onnx").unwrap();
        } else {
            fs::copy(source.join(file.name), dir.path().join(file.name)).unwrap();
        }
    }
    assert!(matches!(
        Embedder::load(dir.path()),
        Err(EmbedError::Model(_))
    ));
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn vectors_are_normalized_with_384_dimensions() {
    let vectors = embedder().embed(&["Prefers pnpm over npm"]).unwrap();
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].len(), 384);
    let norm: f32 = vectors[0].iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() <= 1e-5, "{norm}");
}

#[test]
fn similar_sentences_score_higher_than_unrelated_ones() {
    let vectors = embedder()
        .embed(&[
            "Prefers pnpm over npm",
            "Use pnpm instead of npm",
            "The capital of France is Paris",
        ])
        .unwrap();
    assert!(cosine(&vectors[0], &vectors[1]) > cosine(&vectors[0], &vectors[2]));
}

#[test]
fn no_texts_give_no_vectors() {
    assert!(embedder().embed(&[]).unwrap().is_empty());
}

#[test]
fn text_over_the_model_input_is_truncated() {
    let long = "word ".repeat(2000);
    let vectors = embedder().embed(&[&long]).unwrap();
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].len(), 384);
}

#[test]
fn a_caller_waiting_for_the_model_gets_it_between_another_caller_s_texts() {
    let embedder = std::sync::Arc::new(embedder());
    let text = "Deploys go through ArgoCD, not manual kubectl. ".repeat(20);
    let started = std::sync::Barrier::new(2);
    let (waiter_done, loop_done) = thread::scope(|scope| {
        let looping = scope.spawn(|| {
            started.wait();
            for _ in 0..100 {
                embedder.embed(&[&text]).unwrap();
            }
            std::time::Instant::now()
        });
        started.wait();
        thread::sleep(std::time::Duration::from_millis(20));
        embedder.embed(&["how do we deploy?"]).unwrap();
        let waiter_done = std::time::Instant::now();
        (waiter_done, looping.join().unwrap())
    });
    assert!(
        waiter_done < loop_done,
        "the waiting caller got the model only after the whole loop"
    );
}
