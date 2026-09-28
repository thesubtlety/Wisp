//! Optional Core ML (Apple Neural Engine) encoder assets for the whisper.cpp models.
//!
//! whisper.cpp auto-loads a `<base>-encoder.mlmodelc` directory sitting next to a model and runs the
//! encoder on the Neural Engine. These archives are large (~1.1 GB) and only help Apple-Silicon
//! Macs, so they're downloaded on demand, never bundled.

use wisp_core::model::{ModelDescriptor, ModelFamily};

use crate::catalog::WHISPER_CPP_BASE;

/// A downloadable Core ML encoder for a whisper.cpp model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoremlAsset {
    /// URL of the `.mlmodelc.zip` archive.
    pub url: String,
    /// Directory the archive unzips to — the exact name whisper.cpp derives from the model and
    /// auto-loads next to it.
    pub dir_name: String,
    /// Archive size in bytes, for the download progress bar.
    pub size_bytes: u64,
    /// SHA-256 of the archive, checked before it is unpacked.
    pub sha256: String,
}

/// The Core ML encoder asset for `descriptor`, if it's a whisper.cpp model that has one.
///
/// SenseVoice and sherpa-Whisper models run on ONNX and have no Core ML encoder, so they return
/// `None`. So does a whisper.cpp model whose encoder size we haven't pinned (no asset offered rather
/// than a progress bar with an unknown total).
pub fn coreml_asset(descriptor: &ModelDescriptor) -> Option<CoremlAsset> {
    if descriptor.family != ModelFamily::WhisperCpp {
        return None;
    }
    let bin = descriptor.files.iter().find(|f| f.name.ends_with(".bin"))?;
    let dir_name = coreml_encoder_dir_name(&bin.name);
    let (size_bytes, sha256) = coreml_pin(&dir_name)?;
    Some(CoremlAsset {
        url: format!("{WHISPER_CPP_BASE}/{dir_name}.zip"),
        dir_name,
        size_bytes,
        sha256: sha256.to_owned(),
    })
}

/// The Core ML encoder directory name whisper.cpp derives from a model `.bin` filename.
///
/// Mirrors `whisper_get_coreml_path_encoder` in `vendor/whisper.cpp/src/whisper.cpp`: drop the
/// extension, drop a trailing `-qX_Y` quantization tag, append `-encoder.mlmodelc`. Quantization is
/// the decoder's; the Core ML encoder is shared across a model's quants. (Drift: pinned by tests
/// against the catalog's real `.bin` names — update both together if whisper.cpp changes.)
fn coreml_encoder_dir_name(bin_filename: &str) -> String {
    let mut s = bin_filename
        .rsplit_once('.')
        .map(|(stem, _ext)| stem)
        .unwrap_or(bin_filename)
        .to_owned();

    if let Some(pos) = s.rfind('-') {
        let tag = &s.as_bytes()[pos..];
        if tag.len() == 5 && tag[1] == b'q' && tag[3] == b'_' {
            s.truncate(pos);
        }
    }

    s.push_str("-encoder.mlmodelc");
    s
}

/// Pinned size (for a real progress bar before the download starts) and SHA-256 (checked before
/// unpacking) of each encoder archive we offer, at the commit [`WHISPER_CPP_BASE`] names. An
/// unknown name means no asset is offered.
fn coreml_pin(dir_name: &str) -> Option<(u64, &'static str)> {
    Some(match dir_name {
        "ggml-large-v3-turbo-encoder.mlmodelc" => (
            1_173_393_014,
            "84bedfe895bd7b5de6e8e89a0803dfc5addf8c0c5bc4c937451716bf7cf7988a",
        ),
        "ggml-large-v3-encoder.mlmodelc" => (
            1_175_711_232,
            "47837be7594a29429ec08620043390c4d6d467f8bd362df09e9390ace76a55a4",
        ),
        "ggml-tiny-encoder.mlmodelc" => (
            15_037_446,
            "c88cbd2648e1f5415092bcf5256add463a0f19943e6938f46e8d4ffdebd47739",
        ),
        "ggml-base-encoder.mlmodelc" => (
            37_922_638,
            "7e6ab77041942572f239b5b602f8aaa1c3ed29d73e3d8f20abea03a773541089",
        ),
        "ggml-small-encoder.mlmodelc" => (
            163_083_239,
            "de43fb9fed471e95c19e60ae67575c2bf09e8fb607016da171b06ddad313988b",
        ),
        "ggml-medium-encoder.mlmodelc" => (
            567_829_413,
            "79b0b8d436d47d3f24dd3afc91f19447dd686a4f37521b2f6d9c30a642133fbd",
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_catalog;
    use wisp_core::model::ModelId;

    #[test]
    fn encoder_dir_name_mirrors_whisper_cpp_derivation() {
        // turbo q8 and q5 share one encoder (quantization is the decoder's).
        assert_eq!(
            coreml_encoder_dir_name("ggml-large-v3-turbo-q8_0.bin"),
            "ggml-large-v3-turbo-encoder.mlmodelc"
        );
        assert_eq!(
            coreml_encoder_dir_name("ggml-large-v3-turbo-q5_0.bin"),
            "ggml-large-v3-turbo-encoder.mlmodelc"
        );
        assert_eq!(
            coreml_encoder_dir_name("ggml-large-v3-q5_0.bin"),
            "ggml-large-v3-encoder.mlmodelc"
        );
        // A non-quantized name keeps its stem.
        assert_eq!(
            coreml_encoder_dir_name("ggml-base.bin"),
            "ggml-base-encoder.mlmodelc"
        );
    }

    #[test]
    fn every_whisper_cpp_catalog_model_has_a_pinned_coreml_asset() {
        // Drift guard: each shipped whisper.cpp model resolves to an asset (URL + pinned size), and
        // non-whisper.cpp models never do.
        for d in builtin_catalog() {
            let asset = coreml_asset(&d);
            match d.family {
                ModelFamily::WhisperCpp => {
                    let asset = asset.expect("whisper.cpp model must offer a Core ML asset");
                    assert!(asset.url.ends_with(".mlmodelc.zip"));
                    assert!(asset.size_bytes > 0);
                }
                _ => assert!(
                    asset.is_none(),
                    "only whisper.cpp models have Core ML encoders"
                ),
            }
        }
    }

    #[test]
    fn turbo_quants_resolve_to_the_same_encoder_url() {
        let by_id = |id: &str| {
            builtin_catalog()
                .into_iter()
                .find(|d| d.id == ModelId(id.to_owned()))
                .and_then(|d| coreml_asset(&d))
                .unwrap()
        };
        assert_eq!(
            by_id("whisper-large-v3-turbo-q8").url,
            by_id("whisper-large-v3-turbo-q5").url
        );
    }
}
