//! Local + cloud text-embedding backends for the Wisp notes library.
//!
//! Local models run on the generic [`OrtEmbedder`] (raw ONNX Runtime): each catalog entry carries a
//! [`Recipe`] (ONNX file + pooling + prompts + dim) plus the HuggingFace repo and files to fetch, so
//! a model is just "download these files, load them with this recipe". This one path covers encoder
//! models (CLS / Mean pooling) and decoder models (last-token, e.g. Qwen3). Cloud models call an
//! OpenAI-compatible `/embeddings` endpoint instead.

use std::path::{Path, PathBuf};

use wisp_library::{LibraryError, Result};
use wisp_models::{FileDownloader, HttpDownloader};

mod cloud;
pub use cloud::{cloud_catalog_model, CloudCatalogModel, CloudEmbedder, CLOUD_CATALOG};

mod ort_embed;
pub use ort_embed::{OrtEmbedder, Pooling, Recipe};

/// A vetted, downloadable embedding model the picker can offer.
pub struct CatalogModel {
    /// Stable id persisted as the user's choice and used to look the model back up.
    pub id: &'static str,
    /// Human-facing name for the picker.
    pub label: &'static str,
    /// Provider / family this belongs to (the picker's left-pane grouping), e.g. `"Multilingual E5"`.
    pub group: &'static str,
    /// Approximate download size in MiB (full-precision ONNX), shown before download.
    pub size_mb: u32,
    /// HuggingFace repo the model files download from.
    pub repo: &'static str,
    /// The repo commit the files are fetched from, so an upstream change can't swap them.
    pub revision: &'static str,
    /// Files fetched into the model dir (the ONNX file + any external weights + tokenizer + config),
    /// as repo-relative paths.
    pub files: &'static [&'static str],
    /// SHA-256 of each entry in `files`, in the same order, checked before a download is kept.
    pub sha256: &'static [&'static str],
    /// How to load + run the model (ONNX file within the dir, pooling, prompts, dim).
    pub recipe: Recipe,
}

/// Built-in catalog: small, permissive, multilingual / Chinese models. E5 is multilingual and
/// asymmetric (passage/query prefixes) with Mean pooling; BGE-zh is Chinese-tuned and symmetric
/// (v1.5 needs no instruction) with CLS pooling. The app pairs this with a custom-model slot.
pub const CATALOG: &[CatalogModel] = &[
    CatalogModel {
        id: "e5-small",
        label: "Multilingual E5 small",
        group: "Multilingual E5",
        size_mb: 470,
        repo: "intfloat/multilingual-e5-small",
        revision: "614241f622f53c4eeff9890bdc4f31cfecc418b3",
        files: &[
            "onnx/model.onnx",
            "tokenizer.json",
            "tokenizer_config.json",
            "config.json",
            "special_tokens_map.json",
        ],
        sha256: &[
            "ca456c06b3a9505ddfd9131408916dd79290368331e7d76bb621f1cba6bc8665",
            "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
            "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
            "69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959",
            "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7",
        ],
        recipe: Recipe {
            onnx_file: "onnx/model.onnx",
            pooling: Pooling::Mean,
            passage_prefix: "passage: ",
            query_prefix: "query: ",
            normalize: true,
            dim: 384,
            max_length: 512,
        },
    },
    CatalogModel {
        id: "e5-base",
        label: "Multilingual E5 base",
        group: "Multilingual E5",
        size_mb: 1100,
        repo: "intfloat/multilingual-e5-base",
        revision: "d128750597153bb5987e10b1c3493a34e5a4502a",
        files: &[
            "onnx/model.onnx",
            "tokenizer.json",
            "tokenizer_config.json",
            "config.json",
            "special_tokens_map.json",
        ],
        sha256: &[
            "84a4d426f7e87a6bf5bf195f0bae2c4a7d15f675b23ca96f42fab8326d7a77aa",
            "62c24cdc13d4c9952d63718d6c9fa4c287974249e16b7ade6d5a85e7bbb75626",
            "efb5c0d09722e5fe59a462cd2a9976ee216d55b037597d997cd3fe833216da15",
            "9dab198f24c8c0879e481cf7822005d5ecbceedbacb390ffafa594e28d31bac4",
            "06e405a36dfe4b9604f484f6a1e619af1a7f7d09e34a8555eb0b77b66318067f",
        ],
        recipe: Recipe {
            onnx_file: "onnx/model.onnx",
            pooling: Pooling::Mean,
            passage_prefix: "passage: ",
            query_prefix: "query: ",
            normalize: true,
            dim: 768,
            max_length: 512,
        },
    },
    CatalogModel {
        id: "e5-large",
        label: "Multilingual E5 large",
        group: "Multilingual E5",
        size_mb: 2200,
        // This export keeps weights in an external `model.onnx_data` next to `model.onnx`; ONNX
        // Runtime loads it automatically when both sit in the same dir, so both are in `files`.
        repo: "Qdrant/multilingual-e5-large-onnx",
        revision: "ac6781cd1cf88b8306a536d7c9d18a5bd57cc14b",
        files: &[
            "model.onnx",
            "model.onnx_data",
            "tokenizer.json",
            "tokenizer_config.json",
            "config.json",
            "special_tokens_map.json",
        ],
        sha256: &[
            "1c09780c907c8a91a77a6ab1fd231f79e090d2907ca431223703dfebeed3d36c",
            "0cf1883fee81c63819a44e2ba0efa51d4043d9759685a4ebebbde97e0623d15c",
            "f59925fcb90c92b894cb93e51bb9b4a6105c5c249fe54ce1c704420ac39b81af",
            "f90024142df07163e5e6c5b9a6ad7c8c68b22a9112af11e3db4559a9ff90f737",
            "1de8c3be1f344c0eefa4962480a006f8639f416dfafaa95a770e3cf4bceae6a4",
            "8c785abebea9ae3257b61681b4e6fd8365ceafde980c21970d001e834cf10835",
        ],
        recipe: Recipe {
            onnx_file: "model.onnx",
            pooling: Pooling::Mean,
            passage_prefix: "passage: ",
            query_prefix: "query: ",
            normalize: true,
            dim: 1024,
            max_length: 512,
        },
    },
    CatalogModel {
        id: "bge-small-zh",
        label: "BGE small · Chinese",
        group: "BGE · Chinese",
        size_mb: 95,
        repo: "Xenova/bge-small-zh-v1.5",
        revision: "75c43b069aac4d136ba6bc1122f995fedcfd2781",
        files: &[
            "onnx/model.onnx",
            "tokenizer.json",
            "tokenizer_config.json",
            "config.json",
            "special_tokens_map.json",
        ],
        sha256: &[
            "69a0b846f4f116b5e6aabf9546ea6754d02264f3211a13a1bd69b31b8040749a",
            "48cea5d44424912a6fd1ea647bf4fe50b55ab8b1e5879c3275f80e339e8fae26",
            "e6f3b96db926a37d4039995fbf5ad17de158dfb8f6343d607e4dbaad18d75f5a",
            "d4193ead3a810fd694fa8a31d7fc72fbaebc0668b603e398734bf2f6538ff42f",
            "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
        ],
        recipe: Recipe {
            onnx_file: "onnx/model.onnx",
            pooling: Pooling::Cls,
            passage_prefix: "",
            query_prefix: "",
            normalize: true,
            dim: 512,
            max_length: 512,
        },
    },
    CatalogModel {
        id: "bge-large-zh",
        label: "BGE large · Chinese",
        group: "BGE · Chinese",
        size_mb: 1300,
        repo: "Xenova/bge-large-zh-v1.5",
        revision: "a48549b3259a6165364f226599cd91f39923d5d5",
        files: &[
            "onnx/model.onnx",
            "tokenizer.json",
            "tokenizer_config.json",
            "config.json",
            "special_tokens_map.json",
        ],
        sha256: &[
            "8a78f0b748a6746a0a2ebe0563fddb311762e260abcadaa2b9f19c6964b745fe",
            "7dfbf1966ebf99d471c3796e9b457329d2b2182b817e144f1e904b957745c839",
            "e1790949631401af1bfb6c9c7aeec7fcf612e274d73579d99f704faea40c8ba7",
            "b8a4dce1dfa153b714eb25c75b18238ef2b12e4755f998457f60cd872483be66",
            "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
        ],
        recipe: Recipe {
            onnx_file: "onnx/model.onnx",
            pooling: Pooling::Cls,
            passage_prefix: "",
            query_prefix: "",
            normalize: true,
            dim: 1024,
            max_length: 512,
        },
    },
    CatalogModel {
        id: "bge-m3",
        label: "BGE-M3 · multilingual",
        group: "BGE · Multilingual",
        size_mb: 570,
        repo: "Xenova/bge-m3",
        revision: "4de13258303883538bd53b696b452bf8099f0858",
        files: &[
            "onnx/model_quantized.onnx",
            "tokenizer.json",
            "tokenizer_config.json",
            "config.json",
            "special_tokens_map.json",
        ],
        sha256: &[
            "0826f8c1ab9edf1801db86c61919d4d108e8bfc0b809ec823ad366882ff0b77d",
            "6710678b12670bc442b99edc952c4d996ae309a7020c1fa0096dd245c2faf790",
            "7e4c1cc848840aeccdd763458c18dd525eb0f795c992e00ebe9c28554e7db2d4",
            "734a79bf12d388c1467a4e3ab625f45de7f6906cffcfb93a1eca1787504bed95",
            "8c785abebea9ae3257b61681b4e6fd8365ceafde980c21970d001e834cf10835",
        ],
        recipe: Recipe {
            onnx_file: "onnx/model_quantized.onnx",
            pooling: Pooling::Cls,
            passage_prefix: "",
            query_prefix: "",
            normalize: true,
            dim: 1024,
            max_length: 512,
        },
    },
    CatalogModel {
        id: "gte-multilingual-base",
        label: "GTE multilingual base",
        group: "GTE",
        size_mb: 1220,
        repo: "onnx-community/gte-multilingual-base",
        revision: "2edbf5e672aab465f9ed4c154a8b61791c082c69",
        files: &[
            "onnx/model.onnx",
            "tokenizer.json",
            "tokenizer_config.json",
            "config.json",
            "special_tokens_map.json",
        ],
        sha256: &[
            "5b9f03fdc40350a78fa064b4cfb6bf9a229a7c40aa87736f537e3ebd00aa2b86",
            "3a56def25aa40facc030ea8b0b87f3688e4b3c39eb8b45d5702b3a1300fe2a20",
            "24cebbf2ef20fc317256e03e52ac7b2ca326586f946a8427ecac036332bf0933",
            "6ef2538d4286a7cd18d05225f659d8a1bceca7adb01c186868e53dbd4f822e17",
            "8c785abebea9ae3257b61681b4e6fd8365ceafde980c21970d001e834cf10835",
        ],
        recipe: Recipe {
            onnx_file: "onnx/model.onnx",
            pooling: Pooling::Cls,
            passage_prefix: "",
            query_prefix: "",
            normalize: true,
            dim: 768,
            max_length: 512,
        },
    },
    CatalogModel {
        id: "qwen3-0.6b",
        label: "Qwen3 Embedding 0.6B",
        group: "Qwen3",
        size_mb: 600,
        repo: "onnx-community/Qwen3-Embedding-0.6B-ONNX",
        revision: "c25a394dd583836952667c12f008335071b3f43d",
        files: &[
            "onnx/model_quantized.onnx",
            "tokenizer.json",
            "tokenizer_config.json",
            "config.json",
            "special_tokens_map.json",
        ],
        sha256: &[
            "87cd124e0ef1fd1f223ebc283efccbaeac386d0b08344701c46975d0657b591f",
            "def76fb086971c7867b829c23a26261e38d9d74e02139253b38aeb9df8b4b50a",
            "977648852447cb6587327ff3205b0a84cf2fc9f05621d6c8e88a497caafab2e1",
            "66a10929782f3c9a3cd5dec90e2a95c60e05736134a63cd54479eeae80bed175",
            "76862e765266b85aa9459767e33cbaf13970f327a0e88d1c65846c2ddd3a1ecd",
        ],
        recipe: Recipe {
            onnx_file: "onnx/model_quantized.onnx",
            // Decoder embedder: last-token pooling, and the query carries Qwen3's instruct prompt
            // while stored passages do not (asymmetric).
            pooling: Pooling::LastToken,
            passage_prefix: "",
            query_prefix: "Instruct: Given a web search query, retrieve relevant passages that answer the query\nQuery: ",
            normalize: true,
            dim: 1024,
            max_length: 512,
        },
    },
];

/// Looks up a catalog model by its stable id.
pub fn catalog_model(id: &str) -> Option<&'static CatalogModel> {
    CATALOG.iter().find(|m| m.id == id)
}

/// Downloads a catalog model's files from its HF repo into `dir` (creating it). Each file lands in a
/// sibling `*.part` first and is renamed into place only after the full body is written, so an
/// interrupted download never leaves a half-written file behind; already-present files are skipped,
/// making a re-run resume. Callers observe progress by watching `dir` grow.
pub fn download_model(model: &CatalogModel, dir: &Path) -> Result<()> {
    download_model_with(model, dir, &HttpDownloader::default())
}

/// Downloads with Wisp's shared regional downloader, so the app can apply its live mirror/proxy
/// settings to embedding models as well as transcription models.
pub fn download_model_with(
    model: &CatalogModel,
    dir: &Path,
    downloader: &dyn FileDownloader,
) -> Result<()> {
    for (file, sha256) in model.files.iter().zip(model.sha256) {
        let dest = dir.join(file);
        if dest.exists() {
            continue;
        }

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }

        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{file}",
            model.repo, model.revision
        );
        fetch_atomic(&url, &dest, sha256, downloader)?;
    }
    Ok(())
}

/// Streams `url` into `dest` via a sibling `*.part`, renamed into place only after the whole body is
/// written. A failed transfer retains only the non-final `*.part`, allowing the next attempt to
/// continue with an HTTP Range request.
fn fetch_atomic(
    url: &str,
    dest: &Path,
    sha256: &str,
    downloader: &dyn FileDownloader,
) -> Result<()> {
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);

    downloader
        .download(url, &part)
        .map_err(|error| LibraryError::Embed(error.to_string()))?;

    // A file that doesn't match its pin is deleted, never kept or resumed from.
    if let Err(error) = wisp_models::checksum::verify_file(&part, sha256) {
        let _ = std::fs::remove_file(&part);
        return Err(LibraryError::Embed(error.to_string()));
    }

    std::fs::rename(&part, dest).map_err(io_err)
}

fn io_err(e: std::io::Error) -> LibraryError {
    LibraryError::Embed(format!("io: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use wisp_core::error::Result as ModelResult;
    use wisp_models::FileDownloader;

    /// Serves `body` for every URL and records what was asked for.
    struct RecordingDownloader(Arc<Mutex<Vec<String>>>, &'static [u8]);

    impl FileDownloader for RecordingDownloader {
        fn download(&self, url: &str, dest: &Path) -> ModelResult<()> {
            self.0.lock().unwrap().push(url.to_owned());
            std::fs::write(dest, self.1)?;
            Ok(())
        }
    }

    /// SHA-256 of b"x".
    const X_SHA256: &str = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";

    const TEST_MODEL: CatalogModel = CatalogModel {
        id: "test",
        label: "Test",
        group: "Test",
        size_mb: 1,
        repo: "org/model",
        revision: "0123456789abcdef0123456789abcdef01234567",
        files: &["onnx/model.onnx", "tokenizer.json"],
        sha256: &[X_SHA256, X_SHA256],
        recipe: Recipe {
            onnx_file: "onnx/model.onnx",
            pooling: Pooling::Mean,
            passage_prefix: "",
            query_prefix: "",
            normalize: true,
            dim: 4,
            max_length: 8,
        },
    };

    #[test]
    fn catalog_ids_are_unique_and_findable() {
        let mut seen = std::collections::HashSet::new();
        for m in CATALOG {
            assert!(seen.insert(m.id), "duplicate catalog id {}", m.id);
            assert_eq!(catalog_model(m.id).unwrap().id, m.id);
        }
        assert!(catalog_model("nope").is_none());
    }

    #[test]
    fn catalog_download_can_use_the_shared_regional_downloader() {
        let temp = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let downloader = RecordingDownloader(Arc::clone(&calls), b"x");

        download_model_with(&TEST_MODEL, temp.path(), &downloader).unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), TEST_MODEL.files.len());
        assert_eq!(
            calls[0],
            "https://huggingface.co/org/model/resolve/0123456789abcdef0123456789abcdef01234567/onnx/model.onnx",
            "downloads name the pinned commit"
        );
        for file in TEST_MODEL.files {
            assert!(temp.path().join(file).is_file(), "{file}");
        }
    }

    #[test]
    fn a_download_that_misses_its_pin_is_deleted_not_kept() {
        let temp = tempfile::tempdir().unwrap();
        let downloader = RecordingDownloader(Arc::new(Mutex::new(Vec::new())), b"tampered");

        let err = download_model_with(&TEST_MODEL, temp.path(), &downloader).unwrap_err();

        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        assert!(!temp.path().join("onnx/model.onnx").exists());
        assert!(!temp.path().join("onnx/model.onnx.part").exists());
    }

    #[test]
    fn every_catalog_file_is_pinned() {
        for m in CATALOG {
            assert!(
                m.revision.len() == 40 && m.revision.bytes().all(|b| b.is_ascii_hexdigit()),
                "{}: revision must be a commit",
                m.id
            );
            assert_eq!(
                m.files.len(),
                m.sha256.len(),
                "{}: one sha256 per file",
                m.id
            );
            for sha in m.sha256 {
                assert!(
                    sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{}: malformed sha256 {sha}",
                    m.id
                );
            }
        }
    }

    #[test]
    fn catalog_prefixes_match_family() {
        // E5 is asymmetric (both prefixes set); BGE-zh v1.5 is symmetric (no instruction).
        for m in CATALOG {
            if m.id.starts_with("e5") {
                assert_eq!(m.recipe.passage_prefix, "passage: ", "{}", m.id);
                assert_eq!(m.recipe.query_prefix, "query: ", "{}", m.id);
            } else if m.id.starts_with("bge") {
                assert_eq!(m.recipe.passage_prefix, "", "{}", m.id);
                assert_eq!(m.recipe.query_prefix, "", "{}", m.id);
            }
        }
    }

    #[test]
    fn catalog_files_include_the_recipe_onnx_and_tokenizer() {
        // The downloaded file set must contain the ONNX the recipe loads and the tokenizer the
        // embedder reads, or a download would "succeed" yet fail to load.
        for m in CATALOG {
            assert!(
                m.files.contains(&m.recipe.onnx_file),
                "{} files omit its recipe onnx_file {}",
                m.id,
                m.recipe.onnx_file
            );
            assert!(
                m.files.contains(&"tokenizer.json"),
                "{} omits tokenizer.json",
                m.id
            );
            assert!(m.recipe.dim > 0, "{} has a zero dim", m.id);
        }
    }

    // The real picker path for a catalog model: self-download its files, then load + embed via the
    // recipe — exactly what download_embedding_model + activation do. ~95 MB (bge-small-zh). Opt-in:
    // `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn download_model_then_load_and_embed() {
        use wisp_library::Embedder;

        let model = catalog_model("bge-small-zh").unwrap();
        let dir = std::env::temp_dir().join("wisp-embed-dl-bge-small-zh");
        download_model(model, &dir).unwrap();

        // Every declared file landed and no `*.part` lingered.
        for f in model.files {
            assert!(dir.join(f).exists(), "missing downloaded file {f}");
            let mut part = dir.join(f).into_os_string();
            part.push(".part");
            assert!(!Path::new(&part).exists(), "leftover .part for {f}");
        }

        // A second call is a no-op (files already present) and must still succeed.
        download_model(model, &dir).unwrap();

        let emb = OrtEmbedder::load(&dir, &model.recipe).unwrap();
        assert_eq!(emb.dim(), model.recipe.dim);
        let v = emb.embed_query("预算").unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "embedding not normalized: {norm}"
        );
    }
}
