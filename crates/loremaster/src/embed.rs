//! Local embedding model: its download and its use.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Stored with each vector, because vectors from different models are not comparable.
pub const MODEL_ID: &str = "bge-small-en-v1.5-q";
/// Where the pinned model revision is served from.
pub const MODEL_URL: &str = "https://huggingface.co/Qdrant/bge-small-en-v1.5-onnx-Q/resolve/aa8f8b060edb00e03bfdd08813a2949946c8ba55";
/// Overrides [`MODEL_URL`], to fetch the same files from a mirror.
pub const MODEL_URL_ENV: &str = "LOREMASTER_MODEL_URL";

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const ONNX_FILE: &str = "model_optimized.onnx";

/// A file of the pinned model.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub name: &'static str,
    pub sha256: &'static str,
}

/// The pinned model's files.
pub const MODEL_FILES: [ModelFile; 5] = [
    ModelFile {
        name: ONNX_FILE,
        sha256: "51f1bd0addd6e859e42c2c8021a5e5461385bb676a649f4b269aa445449f2431",
    },
    ModelFile {
        name: "tokenizer.json",
        sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    },
    ModelFile {
        name: "config.json",
        sha256: "13582bcf2effc85b7bf3d3f5532e686bc1c9ce86bb009d10f0ec33cbe92299dd",
    },
    ModelFile {
        name: "special_tokens_map.json",
        sha256: "5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a",
    },
    ModelFile {
        name: "tokenizer_config.json",
        sha256: "0b29c7bfc889e53b36d9dd3e686dd4300f6525110eaa98c76a5dafceb2029f53",
    },
];

/// Why the model could not be fetched, loaded or run.
#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("cannot download {file}: {reason}")]
    Download { file: String, reason: String },
    #[error("{file} does not match its pinned hash")]
    HashMismatch { file: String },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("embedding model error: {0}")]
    Model(String),
}

/// Where to fetch the model from.
pub fn base_url() -> String {
    std::env::var(MODEL_URL_ENV).unwrap_or_else(|_| MODEL_URL.to_owned())
}

/// Directory of the pinned model under a models root.
pub fn model_dir(models: &Path) -> std::path::PathBuf {
    models.join(MODEL_ID)
}

/// Fetches any missing file into `dir`. A file appears there only once complete and verified.
pub fn ensure_model(dir: &Path, base_url: &str, files: &[ModelFile]) -> Result<(), EmbedError> {
    fs::create_dir_all(dir)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .into();
    let missing: Vec<&ModelFile> = files
        .iter()
        .filter(|file| !dir.join(file.name).exists())
        .collect();
    if !missing.is_empty() {
        remove_stale_downloads(dir)?;
    }
    for file in missing {
        download(&agent, dir, base_url, file)?;
    }
    Ok(())
}

/// A process killed mid-download cannot clean up its temp file.
fn remove_stale_downloads(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(".tmp") {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn download(
    agent: &ureq::Agent,
    dir: &Path,
    base_url: &str,
    file: &ModelFile,
) -> Result<(), EmbedError> {
    let failed = |reason: String| EmbedError::Download {
        file: file.name.to_owned(),
        reason,
    };
    let response = agent
        .get(format!("{base_url}/{}", file.name))
        .call()
        .map_err(|e| failed(e.to_string()))?;
    let mut body = response.into_body().into_reader();
    // Dropping the temp file on any early return deletes the partial download.
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = body.read(&mut buffer).map_err(|e| failed(e.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        temp.write_all(&buffer[..read])?;
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if digest != file.sha256 {
        return Err(EmbedError::HashMismatch {
            file: file.name.to_owned(),
        });
    }
    temp.as_file().sync_all()?;
    temp.persist(dir.join(file.name)).map_err(|e| e.error)?;
    Ok(())
}

/// The loaded model. One instance is shared; ONNX inference needs exclusive access.
pub struct Embedder {
    model: Mutex<TextEmbedding>,
}

impl Embedder {
    /// Loads the pinned model from `dir`.
    pub fn load(dir: &Path) -> Result<Self, EmbedError> {
        let read = |name: &str| fs::read(dir.join(name));
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let model = UserDefinedEmbeddingModel::new(read(ONNX_FILE)?, tokenizer_files)
            .with_pooling(Pooling::Cls);
        let model =
            TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::default())
                .map_err(|e| EmbedError::Model(e.to_string()))?;
        let embedder = Self {
            model: Mutex::new(model),
        };
        // The first inference is by far the slowest. Taking it here keeps it from using up the
        // deadline of the user's first prompt.
        embedder.embed(&["warm up"])?;
        Ok(embedder)
    }

    /// Embeds each text as a normalized vector. Text over the model's input size is truncated.
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self
            .model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .embed(texts, None)
            .map_err(|e| EmbedError::Model(e.to_string()));
        // The lock is not fair: a caller that embeds in a loop would take it back before a waiting
        // prompt wakes up. Yielding lets the prompt in between texts.
        std::thread::yield_now();
        vectors
    }
}
