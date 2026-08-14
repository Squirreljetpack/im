use std::env;
use std::fs::File;
use std::io::copy;
use std::path::{Path, PathBuf};

/// Which model is embedded is driven by the `EMBED_MODEL` environment variable
/// (default `nomic`; see `model/quantize_qdq.py`'s `EMBED_ALIASES`). The choice is
/// recorded in `assets/model/.embed_model_stamp`; a mismatch between the stamp and
/// `EMBED_MODEL` regenerates `embed.onnx`, `tokenizer.json`, and
/// `saliency_adaptor.onnx` via `pixi run --manifest-path model/pixi.toml python`
/// (falling back to a bare `python3` when pixi is unavailable).
const EMBED_MODEL_NAME: &str = "embed";
const DEFAULT_EMBED_MODEL: &str = "nomic";
const MODEL_MIN_BYTES: u64 = 100_000_000;

/// Raw FP32 cache filename for an embedding alias, matching the `safe_name`
/// `model/quantize_qdq.py` computes for it (nomic -> `nomic_embed_v1_5`,
/// bge -> `bge_small`), so the `target/cache` download is reused across runs.
fn raw_fp32_name(alias: &str) -> String {
    let safe = match alias {
        "nomic" => "nomic_embed_v1_5",
        "bge" => "bge_small",
        other => other,
    };
    format!("embed_{safe}_fp32.onnx")
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/help.txt");
    let profile = env::var("PROFILE").unwrap_or_default();
    if profile == "release" {
        println!("cargo:rerun-if-changed=assets/config.toml");
        println!("cargo:rerun-if-changed=assets/moods.toml");
    } else {
        println!("cargo:rerun-if-changed=assets/dev.toml");
        println!("cargo:rerun-if-changed=assets/moods.dev.toml");
    }
    println!("cargo:rerun-if-env-changed=EMBED_MODEL");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-lib=dylib=stdc++");

    update_readme_usage();

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let cache = cache_dir();

    let embed_model = env::var("EMBED_MODEL").unwrap_or_else(|_| DEFAULT_EMBED_MODEL.to_string());

    let model_filename = format!("{EMBED_MODEL_NAME}.onnx");
    // Raw FP32 cache uses the safe alias name to avoid colliding with other model caches
    let raw_filename = raw_fp32_name(&embed_model);

    let vendored_model = manifest_dir
        .join("assets")
        .join("model")
        .join(&model_filename);
    println!("cargo:rerun-if-changed={}", vendored_model.display());

    // `.embed_model_stamp` records which embedding model the vendored
    // embed.onnx / tokenizer.json / saliency_adaptor.onnx were generated for.
    // It is committed to the repo so a clean checkout skips generation.
    let embed_stamp_file = manifest_dir.join("assets/model/.embed_model_stamp");
    let current_embed_stamp = std::fs::read_to_string(&embed_stamp_file).unwrap_or_default();
    let embed_mismatch = current_embed_stamp.trim() != embed_model.trim();
    let needs_embed_gen =
        embed_mismatch || file_len(&vendored_model).is_none_or(|len| len < MODEL_MIN_BYTES);

    if needs_embed_gen {
        // A vendored model built for a different embedding model must not be reused.
        if vendored_model.exists() && embed_mismatch {
            let _ = std::fs::remove_file(&vendored_model);
        }

        let script = manifest_dir.join("model/quantize_qdq.py");
        let pixi_toml = manifest_dir.join("model/pixi.toml");
        let raw_path = cache.join(&raw_filename);
        let generated = cache.join(&model_filename);

        let status = std::process::Command::new("pixi")
            .arg("run")
            .arg("--manifest-path")
            .arg(&pixi_toml)
            .arg("python")
            .arg(&script)
            .arg("embedding")
            .arg(&embed_model)
            .arg(&raw_path)
            .arg(&generated)
            .status()
            .or_else(|_| {
                std::process::Command::new("python3")
                    .arg(&script)
                    .arg("embedding")
                    .arg(&embed_model)
                    .arg(&raw_path)
                    .arg(&generated)
                    .status()
            })
            .expect("Failed to execute model/quantize_qdq.py (pixi run or python3)");

        if !status.success() || file_len(&generated).is_none_or(|l| l < MODEL_MIN_BYTES) {
            panic!(
                "Quantization script failed to generate quantized model at {}",
                generated.display()
            );
        }
        let _ = std::fs::copy(&generated, &vendored_model);
        let _ = std::fs::write(&embed_stamp_file, embed_model.trim());
    }

    // The model, tokenizer, and saliency adaptor are bundled as
    // `assets/model/{embed.onnx, tokenizer.json, saliency_adaptor.onnx}`
    // (refreshed by the quantize script whenever the embedding model is
    // regenerated) and are compiled into the binary via `include_bytes!` in
    // src/embed.rs directly from the manifest dir — nothing is downloaded or
    // copied at build time, and no ONNX codegen runs (ort loads the ONNX at
    // runtime).
}

/// `CARGO_MANIFEST_DIR/target/cache` — shared download cache for build-time
/// artifacts. The quantization script downloads the raw FP32 model here
/// (`embed_<safe_name>_fp32.onnx`) and writes the INT8 QDQ graph here
/// (`embed.onnx`) before it is copied into `assets/model/`.
fn cache_dir() -> PathBuf {
    PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("target")
        .join("cache")
}

fn file_len(path: &Path) -> Option<u64> {
    path.metadata().ok().map(|m| m.len())
}

/// The tokenizer is bundled as `assets/model/tokenizer.json` (refreshed by
/// `model/quantize_qdq.py`), so nothing is downloaded at build time anymore.
/// `download` is kept for manual/offline rebuilds of a non-default embedding
/// model.
#[allow(dead_code)]
const TOKENIZER_URL: &str =
    "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/main/tokenizer.json";

/// Download `url` to `dest` (creating parent dirs), ignoring failures: a
/// failed download leaves the file absent, and the caller falls back to
/// whatever is already on disk.
#[allow(dead_code)]
fn download(url: &str, dest: &Path) {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut resp) = ureq::get(url).call() else {
        return;
    };
    let Ok(mut file) = File::create(dest) else {
        return;
    };
    let _ = copy(&mut resp.body_mut().as_reader(), &mut file);
}

fn update_readme_usage() {
    let help_path = Path::new("assets/help.txt");
    let readme_path = Path::new("README.md");

    if !help_path.exists() || !readme_path.exists() {
        return;
    }

    let help_content = match std::fs::read_to_string(help_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let readme_content = match std::fs::read_to_string(readme_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let start_marker = "<!-- HELP_START -->";
    let end_marker = "<!-- HELP_END -->";

    if let (Some(start_idx), Some(end_idx)) = (
        readme_content.find(start_marker),
        readme_content.find(end_marker),
    ) {
        let before = &readme_content[..start_idx + start_marker.len()];
        let after = &readme_content[end_idx..];
        let new_block = format!("\n```\n{}\n```\n", help_content.trim());
        let updated_readme = format!("{}{}{}", before, new_block, after);

        if updated_readme != readme_content {
            let _ = std::fs::write(readme_path, updated_readme);
        }
    }
}
