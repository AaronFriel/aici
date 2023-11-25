mod kernels;
pub mod llama;
mod logits;
pub mod seq;

pub use logits::LogitsProcessor;
use seq::{BatchInfo, SeqId, SeqPhase, Sequance};

use std::{collections::HashSet, fmt::Display, path::PathBuf};

use anyhow::{anyhow, Error as E, Result};
use candle::{DType, Device, IndexOp};
use candle_nn::VarBuilder;
use hf_hub::{
    api::sync::{Api, ApiRepo},
    RepoType,
};
use llama::{Llama, LlamaConfig};
use tokenizers::Tokenizer;

use candle_transformers::models::llama as llama_ref;

#[derive(Default)]
pub struct LoaderArgs {
    pub model_id: Option<String>,
    pub revision: Option<String>,
    pub local_weights: Option<String>,
    pub use_reference: bool,
}

enum Repo {
    Api(ApiRepo),
    Local(String),
}

impl Repo {
    fn from(args: &LoaderArgs) -> Result<Repo> {
        match &args.local_weights {
            Some(path) => Ok(Repo::Local(path.to_owned())),
            None => {
                let api = Api::new()?;
                let model_id = args
                    .model_id
                    .clone()
                    .unwrap_or_else(|| "NousResearch/Llama-2-7b-hf".to_string());
                let revision = args.revision.clone().unwrap_or("main".to_string());
                let api = api.repo(hf_hub::Repo::with_revision(
                    model_id,
                    RepoType::Model,
                    revision,
                ));
                Ok(Repo::Api(api))
            }
        }
    }

    fn get(&self, filename: &str) -> Result<PathBuf> {
        match self {
            Repo::Api(api) => api.get(filename).map_err(E::msg),
            Repo::Local(path) => Ok((path.to_owned() + filename).into()),
        }
    }

    fn read(&self, filename: &str) -> Result<Vec<u8>> {
        std::fs::read(self.get(filename)?).map_err(E::msg)
    }
}

impl Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Repo::Api(api) => write!(f, "{}", api.url("")),
            Repo::Local(path) => write!(f, "{}", path),
        }
    }
}

pub enum Model {
    Llama(Llama),
    Reference(llama_ref::Llama),
}

pub struct LlamaInfer {
    pub tokenizer: Tokenizer,
    pub model: Model,
    seq_id: SeqId,
    cache: Option<llama::Cache>,
    pub device: Device,
    pub eos_token_id: u32,
}

impl LlamaInfer {
    pub fn load(args: LoaderArgs) -> Result<LlamaInfer> {
        let device = Device::new_cuda(0)?;
        let dtype = DType::BF16;

        let repo = Repo::from(&args)?;
        println!("loading the model weights from {}", repo);

        let tokenizer_filename = repo.get("tokenizer.json")?;

        let config: LlamaConfig = serde_json::from_slice(&repo.read("config.json")?)?;
        let config = config.into_config();

        let st_index: serde_json::Value =
            serde_json::from_slice(&repo.read("model.safetensors.index.json")?)?;

        let entries = st_index["weight_map"]
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_str().unwrap().to_owned());

        let h = HashSet::<String>::from_iter(entries);
        let mut filenames = h.iter().collect::<Vec<_>>();
        filenames.sort();
        let filenames = filenames
            .iter()
            .map(|f| repo.get(f))
            .collect::<Result<Vec<_>>>()?;

        println!("building the model");

        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&filenames, dtype, &device)? };
        let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(anyhow::Error::msg)?;

        let eos_token_id = tokenizer
            .token_to_id("</s>")
            .ok_or(anyhow!("</s> not found"))?;

        let (model, cache) = if args.use_reference {
            let config: llama_ref::LlamaConfig =
                serde_json::from_slice(&repo.read("config.json")?)?;
            let use_flash_attn = true;
            let config = config.into_config(use_flash_attn);
            let use_kv_cache = true;
            let cache = llama_ref::Cache::new(use_kv_cache, dtype, &config, &device)?;
            let llama = llama_ref::Llama::load(vb, &cache, &config)?;
            (Model::Reference(llama), None)
        } else {
            let cache = llama::Cache::new(dtype, &config, &device)?;
            let llama = Llama::load(vb, &cache, &config)?;
            (Model::Llama(llama), Some(cache))
        };

        Ok(LlamaInfer {
            tokenizer,
            model,
            cache,
            seq_id: 1,
            device,
            eos_token_id,
        })
    }

    pub fn new_seq(&mut self, prompt: &str) -> Result<Sequance> {
        let tokens = self
            .tokenizer
            .encode(prompt, true)
            .map_err(anyhow::Error::msg)?
            .get_ids()
            .to_vec();
        let prompt_len = tokens.len();
        let seq = Sequance {
            seq_id: self.seq_id,
            phase: SeqPhase::Prompt,
            tokens,
            prompt_len,
        };
        self.seq_id += 1;
        Ok(seq)
    }

    pub fn decode_seq(&self, seq: &Sequance) -> Result<String> {
        let tokens = &seq.tokens[seq.prompt_len..];
        let generated = self
            .tokenizer
            .decode(tokens, true)
            .map_err(anyhow::Error::msg)?;
        Ok(generated)
    }

    pub fn generate(
        &mut self,
        prompt: &str,
        sample_len: usize,
        logits_processor: &mut LogitsProcessor,
    ) -> Result<String> {
        self.cache.as_ref().map(|x| x.clear());

        let seq = self.new_seq(prompt)?;
        let mut seqs = vec![seq];
        // seqs.push(self.new_seq(prompt)?);
        // seqs.push(self.new_seq(prompt)?);

        for _idx in 0..sample_len {
            let info = BatchInfo::from_seqs(&seqs, &self.device)?;
            // println!("batch_info #{_idx}: {:?}", info);
            let logits = match &self.model {
                Model::Llama(llama) => llama.forward(&info)?,
                Model::Reference(llama) => {
                    let index_pos = info.positions.i(0..1)?.to_vec1::<i64>()?[0];
                    let input = info.tokens.unsqueeze(0)?;
                    llama.forward(&input, index_pos as usize)?
                }
            };
            // println!("logits: {}", logits);
            for idx in 0..seqs.len() {
                let logits = logits.i((idx, ..))?;
                let next_token = logits_processor.sample(&logits)?;
                seqs[idx].tokens.push(next_token);
                seqs[idx].phase = SeqPhase::Gen;
                // if next_token == self.eos_token_id {
                //     break;
                // }
            }
        }

        Ok(self.decode_seq(&seqs[0])?)
    }
}