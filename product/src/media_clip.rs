//! Real vector/semantic media search (WP-047): CLIP embeddings via tract.
//!
//! Operator-provisioned CLIP ONNX encoders (image + text, projection heads,
//! 512-d output — reference export documented in the Manual) run on the
//! existing pure-Rust `tract-onnx` runtime; no Python, no external services.
//! Text is tokenized with the vendored OpenAI CLIP BPE vocabulary
//! (`product/assets/clip/bpe_simple_vocab_16e6.txt`, MIT).
//!
//! Embeddings are cached in the shared embedded SurrealDB store keyed by
//! canonical media key with mtime/size invalidation. Rows remain regenerable
//! and are isolated in the `clip_embeddings` table.
//!
//! When the models are absent the caller falls back to the local metadata
//! scorer (`media_search::RankMode::Metadata`) with a visible status line —
//! never a panic, never silent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::surreal_kv::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use tract_onnx::prelude::*;

/// key = canonical media key; value = mtime(u64 LE) + size(u64 LE) + dim(u32
/// LE) + dim * f32 LE (L2-normalized embedding).
const EMBEDDINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("clip_embeddings");

pub const EMBED_DIM_EXPECTED: usize = 512;
const CONTEXT_LEN: usize = 77;
const SOT: i64 = 49406;
const EOT: i64 = 49407;
const IMAGE_EDGE: usize = 224;
const CLIP_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const CLIP_STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

// ---------------------------------------------------------------------------
// Model resolution
// ---------------------------------------------------------------------------

/// Where the CLIP models were found (or why not).
#[derive(Clone, Debug)]
pub struct ClipStatus {
    pub image_model: Option<PathBuf>,
    pub text_model: Option<PathBuf>,
    pub vocab: Option<PathBuf>,
    pub detail: String,
}

impl ClipStatus {
    pub fn ready(&self) -> bool {
        self.image_model.is_some() && self.text_model.is_some() && self.vocab.is_some()
    }
}

/// Resolve model + vocab paths: config/env first, then the conventional
/// `product/models/clip-vit-b32-{visual,textual}.onnx` drop location.
pub fn resolve(config: &crate::config::AppConfig) -> ClipStatus {
    let conventional = |name: &str| -> Option<PathBuf> {
        let path = config.repo_root.join("product").join("models").join(name);
        path.exists().then_some(path)
    };
    let env_path = |key: &str| -> Option<PathBuf> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .filter(|p| p.exists())
    };
    let image_model =
        env_path("FACIAL_CLIP_IMAGE_MODEL").or_else(|| conventional("clip-vit-b32-visual.onnx"));
    let text_model =
        env_path("FACIAL_CLIP_TEXT_MODEL").or_else(|| conventional("clip-vit-b32-textual.onnx"));
    let vocab_path = config
        .repo_root
        .join("product")
        .join("assets")
        .join("clip")
        .join("bpe_simple_vocab_16e6.txt");
    let vocab = vocab_path.exists().then_some(vocab_path);
    let detail = match (&image_model, &text_model, &vocab) {
        (Some(_), Some(_), Some(_)) => "clip models present".to_string(),
        (i, t, v) => {
            let mut missing = Vec::new();
            if i.is_none() {
                missing.push("clip-vit-b32-visual.onnx");
            }
            if t.is_none() {
                missing.push("clip-vit-b32-textual.onnx");
            }
            if v.is_none() {
                missing.push("assets/clip/bpe_simple_vocab_16e6.txt");
            }
            format!(
                "semantic search: local fallback (missing {}; drop models into product/models/)",
                missing.join(", ")
            )
        }
    };
    ClipStatus {
        image_model,
        text_model,
        vocab,
        detail,
    }
}

// ---------------------------------------------------------------------------
// CLIP BPE tokenizer (port of openai/CLIP simple_tokenizer, sans ftfy)
// ---------------------------------------------------------------------------

pub struct ClipTokenizer {
    encoder: HashMap<String, i64>,
    bpe_ranks: HashMap<(String, String), usize>,
    byte_map: [char; 256],
    pattern: regex::Regex,
}

/// GPT-2-style byte -> printable unicode mapping. Returns the byte-indexed
/// map (for encoding) AND the insertion-ordered char list — CLIP's vocabulary
/// is built in `bs` insertion order (printables first, remapped bytes after),
/// NOT in byte-value order, and token ids depend on that order.
fn bytes_to_unicode() -> ([char; 256], Vec<char>) {
    let mut bs: Vec<u16> = (b'!' as u16..=b'~' as u16)
        .chain(0xA1..=0xAC)
        .chain(0xAE..=0xFF)
        .collect();
    let mut cs: Vec<u32> = bs.iter().map(|&b| b as u32).collect();
    let mut n: u32 = 0;
    for b in 0u16..256 {
        if !bs.contains(&b) {
            bs.push(b);
            cs.push(256 + n);
            n += 1;
        }
    }
    let mut map = ['\0'; 256];
    let mut ordered = Vec::with_capacity(256);
    for (b, c) in bs.iter().zip(cs.iter()) {
        let ch = char::from_u32(*c).unwrap_or('\u{FFFD}');
        map[*b as usize] = ch;
        ordered.push(ch);
    }
    (map, ordered)
}

impl ClipTokenizer {
    pub fn load(vocab_path: &Path) -> Result<Self, String> {
        let raw =
            std::fs::read_to_string(vocab_path).map_err(|e| format!("read clip vocab: {e}"))?;
        // CLIP: merges = lines[1 : 49152-256-2+1]
        let merges: Vec<(String, String)> = raw
            .lines()
            .skip(1)
            .take(49_152 - 256 - 2 + 1 - 1)
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                Some((parts.next()?.to_string(), parts.next()?.to_string()))
            })
            .collect();
        let (byte_map, ordered_chars) = bytes_to_unicode();
        // Vocabulary order matches CLIP: bs-ordered chars, same +</w>,
        // merges, specials.
        let mut vocab: Vec<String> = ordered_chars.iter().map(|c| c.to_string()).collect();
        vocab.extend(ordered_chars.iter().map(|c| format!("{c}</w>")));
        for (a, b) in &merges {
            vocab.push(format!("{a}{b}"));
        }
        vocab.push("<|startoftext|>".to_string());
        vocab.push("<|endoftext|>".to_string());
        let encoder: HashMap<String, i64> = vocab
            .into_iter()
            .enumerate()
            .map(|(i, tok)| (tok, i as i64))
            .collect();
        let bpe_ranks: HashMap<(String, String), usize> = merges
            .into_iter()
            .enumerate()
            .map(|(i, pair)| (pair, i))
            .collect();
        let pattern = regex::Regex::new(
            r"(?i)<\|startoftext\|>|<\|endoftext\|>|'s|'t|'re|'ve|'m|'ll|'d|[\p{L}]+|[\p{N}]|[^\s\p{L}\p{N}]+",
        )
        .map_err(|e| format!("clip tokenizer regex: {e}"))?;
        Ok(Self {
            encoder,
            bpe_ranks,
            byte_map,
            pattern,
        })
    }

    fn bpe(&self, token: &str) -> Vec<String> {
        let mapped: Vec<String> = token
            .bytes()
            .map(|b| self.byte_map[b as usize].to_string())
            .collect();
        if mapped.is_empty() {
            return Vec::new();
        }
        let mut word: Vec<String> = mapped;
        let last = word.len() - 1;
        word[last] = format!("{}</w>", word[last]);

        loop {
            if word.len() < 2 {
                break;
            }
            let mut best: Option<(usize, usize)> = None; // (rank, position)
            for i in 0..word.len() - 1 {
                if let Some(&rank) = self.bpe_ranks.get(&(word[i].clone(), word[i + 1].clone())) {
                    if best.map_or(true, |(br, _)| rank < br) {
                        best = Some((rank, i));
                    }
                }
            }
            let Some((_, pos)) = best else { break };
            let merged = format!("{}{}", word[pos], word[pos + 1]);
            word.splice(pos..=pos + 1, [merged]);
        }
        word
    }

    /// Encode text into fixed-length id + attention arrays (truncating).
    pub fn encode(&self, text: &str) -> ([i64; CONTEXT_LEN], [i64; CONTEXT_LEN]) {
        let cleaned = text.trim().to_lowercase();
        let mut ids: Vec<i64> = vec![SOT];
        for token in self.pattern.find_iter(&cleaned) {
            for piece in self.bpe(token.as_str()) {
                if let Some(&id) = self.encoder.get(&piece) {
                    ids.push(id);
                }
            }
            if ids.len() >= CONTEXT_LEN - 1 {
                break;
            }
        }
        ids.truncate(CONTEXT_LEN - 1);
        ids.push(EOT);
        let mut out = [0i64; CONTEXT_LEN];
        let mut attn = [0i64; CONTEXT_LEN];
        for (i, id) in ids.iter().enumerate() {
            out[i] = *id;
            attn[i] = 1;
        }
        (out, attn)
    }
}

// ---------------------------------------------------------------------------
// Encoders (tract)
// ---------------------------------------------------------------------------

type RunnableClip = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

/// What the text encoder's second input (if any) actually is. HF exports
/// vary: `attention_mask` and `position_ids` share the identical i64 [1,77]
/// signature, so shape alone cannot tell them apart — the NODE NAME decides
/// (review round 3, finding 5).
#[derive(Clone, Copy, PartialEq, Debug)]
enum TextSecondInput {
    None,
    Mask,
    Positions,
}

pub struct ClipEngine {
    text_plan: RunnableClip,
    text_second: TextSecondInput,
    text_preferred_output: Option<usize>,
    image_plan: RunnableClip,
    image_preferred_output: Option<usize>,
    tokenizer: ClipTokenizer,
    pub dim: usize,
    /// Human-readable description of which tensors were selected — surfaced
    /// in the status line so a wrong export is VISIBLE, not silent garbage.
    pub picks: String,
}

impl ClipEngine {
    /// Load both encoders + tokenizer and run a self-check text embedding.
    /// Any failure returns a human-readable reason (surfaced as the fallback
    /// status line); nothing panics.
    pub fn load(status: &ClipStatus) -> Result<Self, String> {
        let (Some(image_path), Some(text_path), Some(vocab_path)) = (
            status.image_model.as_ref(),
            status.text_model.as_ref(),
            status.vocab.as_ref(),
        ) else {
            return Err(status.detail.clone());
        };
        let tokenizer = ClipTokenizer::load(vocab_path)?;

        let mut text_model = tract_onnx::onnx()
            .model_for_path(text_path)
            .map_err(|e| format!("clip text model load: {e}"))?;
        // Second-input identification by NODE NAME (mask vs position_ids).
        let text_input_names: Vec<String> = text_model
            .input_outlets()
            .map(|outlets| {
                outlets
                    .iter()
                    .map(|o| text_model.node(o.node).name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let text_second = match text_input_names.get(1) {
            None => TextSecondInput::None,
            Some(name) if name.to_lowercase().contains("position") => TextSecondInput::Positions,
            Some(name) if name.to_lowercase().contains("mask") => TextSecondInput::Mask,
            // Unknown name with the right signature: assume mask, but the
            // picks string surfaces the assumption.
            Some(_) => TextSecondInput::Mask,
        };
        // Output preference by OUTLET LABEL: projection-head exports name
        // their result `text_embeds` / `image_embeds`.
        let text_output_labels: Vec<String> = output_labels(&text_model);
        let text_preferred_output = text_output_labels
            .iter()
            .position(|l| l.to_lowercase().contains("embeds"));
        text_model
            .set_input_fact(
                0,
                InferenceFact::dt_shape(i64::datum_type(), tvec!(1, CONTEXT_LEN as i64)),
            )
            .map_err(|e| format!("clip text input fact: {e}"))?;
        if text_second != TextSecondInput::None {
            text_model
                .set_input_fact(
                    1,
                    InferenceFact::dt_shape(i64::datum_type(), tvec!(1, CONTEXT_LEN as i64)),
                )
                .map_err(|e| format!("clip text second-input fact: {e}"))?;
        }
        let text_plan = text_model
            .into_optimized()
            .map_err(|e| format!("clip text optimize: {e}"))?
            .into_runnable()
            .map_err(|e| format!("clip text runnable: {e}"))?;

        let image_model = tract_onnx::onnx()
            .model_for_path(image_path)
            .map_err(|e| format!("clip image model load: {e}"))?;
        let image_output_labels: Vec<String> = output_labels(&image_model);
        let image_preferred_output = image_output_labels
            .iter()
            .position(|l| l.to_lowercase().contains("embeds"));
        let image_model = image_model
            .with_input_fact(
                0,
                InferenceFact::dt_shape(
                    f32::datum_type(),
                    tvec!(1, 3, IMAGE_EDGE as i64, IMAGE_EDGE as i64),
                ),
            )
            .map_err(|e| format!("clip image input fact: {e}"))?;
        let image_plan = image_model
            .into_optimized()
            .map_err(|e| format!("clip image optimize: {e}"))?
            .into_runnable()
            .map_err(|e| format!("clip image runnable: {e}"))?;

        let mut engine = Self {
            text_plan,
            text_second,
            text_preferred_output,
            image_plan,
            image_preferred_output,
            tokenizer,
            dim: 0,
            picks: String::new(),
        };
        // Self-check BOTH encoders end-to-end before anything trusts them:
        // a probe phrase and a zero image establish dims + picked tensors.
        let (text_probe, text_pick) = engine.run_text("a photo")?;
        if text_probe.len() < 64 {
            return Err(format!(
                "clip text encoder produced {} dims (expected ~{EMBED_DIM_EXPECTED}); wrong export?",
                text_probe.len()
            ));
        }
        let zero_image = vec![0f32; 3 * IMAGE_EDGE * IMAGE_EDGE];
        let (image_probe, image_pick) = engine.run_image_chw(zero_image)?;
        if image_probe.len() != text_probe.len() {
            return Err(format!(
                "clip encoder dim mismatch: text {} vs image {} — mixed exports?",
                text_probe.len(),
                image_probe.len()
            ));
        }
        engine.dim = text_probe.len();
        let second_desc = match text_second {
            TextSecondInput::None => "ids-only".to_string(),
            TextSecondInput::Mask => "mask".to_string(),
            TextSecondInput::Positions => "position_ids".to_string(),
        };
        engine.picks = format!(
            "text→{} ({}), image→{}",
            text_output_labels
                .get(text_pick)
                .cloned()
                .unwrap_or_else(|| format!("out{text_pick}")),
            second_desc,
            image_output_labels
                .get(image_pick)
                .cloned()
                .unwrap_or_else(|| format!("out{image_pick}")),
        );
        Ok(engine)
    }

    /// Pick the embedding vector from model outputs. The preferred (named)
    /// output wins when it has a plausible shape; otherwise the last output
    /// whose element count is a plausible flat [1, D] embedding. Returns the
    /// picked output index alongside the normalized vector.
    fn extract_embedding(
        outputs: TVec<TValue>,
        preferred: Option<usize>,
    ) -> Result<(Vec<f32>, usize), String> {
        let plausible = |value: &TValue| -> bool {
            let shape = value.shape();
            let len: usize = shape.iter().product();
            (64..=4096).contains(&len) && shape.first() == Some(&1) && shape.len() <= 2
        };
        let read = |value: &TValue| -> Result<Vec<f32>, String> {
            let slice = value
                .to_array_view::<f32>()
                .map_err(|e| format!("clip output dtype: {e}"))?;
            let mut v: Vec<f32> = slice.iter().copied().collect();
            l2_normalize(&mut v);
            Ok(v)
        };
        if let Some(index) = preferred {
            if let Some(value) = outputs.get(index) {
                if plausible(value) {
                    return Ok((read(value)?, index));
                }
            }
        }
        for (index, value) in outputs.iter().enumerate().rev() {
            if plausible(value) {
                return Ok((read(value)?, index));
            }
        }
        Err("clip output: no [1, D] embedding tensor found (wrong export — use the projection-head models)".to_string())
    }

    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, String> {
        self.run_text(text).map(|(v, _)| v)
    }

    fn run_text(&self, text: &str) -> Result<(Vec<f32>, usize), String> {
        let (ids, attn) = self.tokenizer.encode(text);
        let ids_tensor = tract_ndarray::Array2::from_shape_vec((1, CONTEXT_LEN), ids.to_vec())
            .map_err(|e| format!("clip ids shape: {e}"))?
            .into_tensor();
        let mut inputs: TVec<TValue> = tvec!(ids_tensor.into());
        match self.text_second {
            TextSecondInput::None => {}
            TextSecondInput::Mask => {
                let mask_tensor =
                    tract_ndarray::Array2::from_shape_vec((1, CONTEXT_LEN), attn.to_vec())
                        .map_err(|e| format!("clip mask shape: {e}"))?
                        .into_tensor();
                inputs.push(mask_tensor.into());
            }
            TextSecondInput::Positions => {
                // position_ids export: feed 0..77, NOT the attention array.
                let positions: Vec<i64> = (0..CONTEXT_LEN as i64).collect();
                let pos_tensor = tract_ndarray::Array2::from_shape_vec((1, CONTEXT_LEN), positions)
                    .map_err(|e| format!("clip positions shape: {e}"))?
                    .into_tensor();
                inputs.push(pos_tensor.into());
            }
        }
        let outputs = self
            .text_plan
            .run(inputs)
            .map_err(|e| format!("clip text run: {e}"))?;
        Self::extract_embedding(outputs, self.text_preferred_output)
    }

    pub fn embed_image_bytes(&self, bytes: &[u8]) -> Result<Vec<f32>, String> {
        let img = image::load_from_memory(bytes).map_err(|e| format!("decode: {e}"))?;
        // Shorter side -> 224, center crop 224x224 (CLIP preprocessing).
        let (w, h) = (img.width(), img.height());
        let scale = IMAGE_EDGE as f32 / w.min(h).max(1) as f32;
        let (nw, nh) = (
            ((w as f32 * scale).round() as u32).max(IMAGE_EDGE as u32),
            ((h as f32 * scale).round() as u32).max(IMAGE_EDGE as u32),
        );
        let resized = img.resize_exact(nw, nh, image::imageops::FilterType::Triangle);
        let x0 = (nw - IMAGE_EDGE as u32) / 2;
        let y0 = (nh - IMAGE_EDGE as u32) / 2;
        let cropped = resized.crop_imm(x0, y0, IMAGE_EDGE as u32, IMAGE_EDGE as u32);
        let rgb = cropped.to_rgb8();
        let mut chw = vec![0f32; 3 * IMAGE_EDGE * IMAGE_EDGE];
        for (x, y, pixel) in rgb.enumerate_pixels() {
            for c in 0..3 {
                chw[c * IMAGE_EDGE * IMAGE_EDGE + (y as usize) * IMAGE_EDGE + x as usize] =
                    (pixel[c] as f32 / 255.0 - CLIP_MEAN[c]) / CLIP_STD[c];
            }
        }
        self.run_image_chw(chw).map(|(v, _)| v)
    }

    fn run_image_chw(&self, chw: Vec<f32>) -> Result<(Vec<f32>, usize), String> {
        let tensor = tract_ndarray::Array4::from_shape_vec((1, 3, IMAGE_EDGE, IMAGE_EDGE), chw)
            .map_err(|e| format!("clip image shape: {e}"))?
            .into_tensor();
        let outputs = self
            .image_plan
            .run(tvec!(tensor.into()))
            .map_err(|e| format!("clip image run: {e}"))?;
        Self::extract_embedding(outputs, self.image_preferred_output)
    }

    pub fn embed_image_path(&self, path: &str) -> Result<Vec<f32>, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
        self.embed_image_bytes(&bytes)
    }
}

/// Output outlet labels of an inference model ("out<N>" when unlabeled).
fn output_labels(model: &tract_onnx::prelude::InferenceModel) -> Vec<String> {
    model
        .output_outlets()
        .map(|outlets| {
            outlets
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    model
                        .outlet_label(*o)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("out{i}"))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// ---------------------------------------------------------------------------
// Embedding index (SurrealDB cache table)
// ---------------------------------------------------------------------------

pub struct ClipIndex {
    db: Database,
}

impl ClipIndex {
    pub fn index_path(workspace_root: &Path) -> PathBuf {
        crate::media_db::MediaDb::db_path(workspace_root)
    }

    pub fn open(workspace_root: &Path) -> Result<Self, String> {
        let path = Self::index_path(workspace_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let db = Database::create(&path).map_err(|e| format!("clip index open: {e}"))?;
        Ok(Self { db })
    }

    /// Cached embedding for `key` when mtime+size still match.
    pub fn get(&self, key: &str, mtime: u64, size: u64) -> Option<Vec<f32>> {
        let txn = self.db.begin_read().ok()?;
        let table = txn.open_table(EMBEDDINGS).ok()?;
        let value = table.get(key).ok().flatten()?;
        decode_row(value.value()).and_then(|(m, s, v)| {
            if m == mtime && s == size {
                Some(v)
            } else {
                None
            }
        })
    }

    pub fn put(&self, key: &str, mtime: u64, size: u64, embedding: &[f32]) -> Result<(), String> {
        let row = encode_row(mtime, size, embedding);
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut table = txn.open_table(EMBEDDINGS).map_err(|e| e.to_string())?;
            table
                .insert(key, row.as_slice())
                .map_err(|e| e.to_string())?;
        }
        txn.commit().map_err(|e| e.to_string())
    }

    /// Number of stored embeddings (settings/status display).
    pub fn len(&self) -> usize {
        let Ok(txn) = self.db.begin_read() else {
            return 0;
        };
        let Ok(table) = txn.open_table(EMBEDDINGS) else {
            return 0;
        };
        table.iter().map(|iter| iter.flatten().count()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn encode_row(mtime: u64, size: u64, embedding: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + embedding.len() * 4);
    out.extend_from_slice(&mtime.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(embedding.len() as u32).to_le_bytes());
    for value in embedding {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn decode_row(raw: &[u8]) -> Option<(u64, u64, Vec<f32>)> {
    if raw.len() < 20 {
        return None;
    }
    let mtime = u64::from_le_bytes(raw[0..8].try_into().ok()?);
    let size = u64::from_le_bytes(raw[8..16].try_into().ok()?);
    let dim = u32::from_le_bytes(raw[16..20].try_into().ok()?) as usize;
    if raw.len() != 20 + dim * 4 {
        return None;
    }
    let mut v = Vec::with_capacity(dim);
    for i in 0..dim {
        let start = 20 + i * 4;
        v.push(f32::from_le_bytes(raw[start..start + 4].try_into().ok()?));
    }
    Some((mtime, size, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("clip")
            .join("bpe_simple_vocab_16e6.txt")
    }

    #[test]
    fn tokenizer_matches_reference_clip_ids() {
        let tok = ClipTokenizer::load(&vocab_path()).expect("vocab loads");
        // Canonical reference: "a photo of a cat"
        // -> [49406, 320, 1125, 539, 320, 2368, 49407]
        let (ids, attn) = tok.encode("a photo of a cat");
        assert_eq!(&ids[..7], &[49406, 320, 1125, 539, 320, 2368, 49407]);
        assert_eq!(ids[7], 0, "zero padded");
        assert_eq!(&attn[..7], &[1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(attn[7], 0);
        // Case-insensitive.
        let (upper, _) = tok.encode("A PHOTO OF A CAT");
        assert_eq!(upper, ids);
        // Empty text still frames sot/eot.
        let (empty, _) = tok.encode("");
        assert_eq!(&empty[..2], &[SOT, EOT]);
    }

    #[test]
    fn embedding_row_round_trips_and_invalidates() {
        let ws = std::env::temp_dir().join(format!("facial-clip-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let index = ClipIndex::open(&ws).unwrap();
        let mut v = vec![0.5f32, -0.25, 0.8];
        l2_normalize(&mut v);
        index.put("shoot/a.jpg", 100, 2000, &v).unwrap();
        let hit = index.get("shoot/a.jpg", 100, 2000).expect("cache hit");
        assert_eq!(hit.len(), 3);
        assert!((cosine(&hit, &v) - 1.0).abs() < 1e-6);
        assert!(
            index.get("shoot/a.jpg", 101, 2000).is_none(),
            "mtime change invalidates"
        );
        assert!(
            index.get("shoot/a.jpg", 100, 2001).is_none(),
            "size change invalidates"
        );
        assert_eq!(index.len(), 1);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn cosine_and_normalize_behave() {
        let mut a = vec![3.0f32, 4.0];
        l2_normalize(&mut a);
        assert!((a[0] - 0.6).abs() < 1e-6 && (a[1] - 0.8).abs() < 1e-6);
        let b = vec![0.6f32, 0.8];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
        let orthogonal = vec![-0.8f32, 0.6];
        assert!(cosine(&a, &orthogonal).abs() < 1e-6);
    }

    #[test]
    fn resolve_reports_missing_models_as_fallback_detail() {
        let mut config = crate::config::load_config();
        // Parallel config tests deliberately mutate FACIAL_REPO_ROOT. This
        // assertion is about the vocabulary vendored with this build, so bind
        // it to Cargo's immutable manifest location instead of process-global
        // environment state.
        config.repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("product manifest has a repository parent")
            .to_path_buf();
        let status = resolve(&config);
        // Vocab is vendored in-repo, so it must resolve regardless of models.
        assert!(status.vocab.is_some(), "vendored vocab present");
        if !status.ready() {
            assert!(status.detail.contains("local fallback"));
        }
    }
}
