use aici_abi::bytes::TokRxInfo;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct TokenInfo {
    pub hf_model: String,
    pub eos_token: u32,
    pub vocab_size: Option<u32>,
    pub special: BTreeMap<String, u32>,
    pub binary: BTreeMap<String, u32>,
    pub text: BTreeMap<String, u32>,
}

pub struct Tokenizer {
    pub name: String,
    pub description: String,
    pub info: Option<TokenInfo>,
    info_bytes: &'static [u8],
    pub hf_bytes: &'static [u8],
    pub add_tokens: u32,
}

macro_rules! tok {
    ($name:literal, $desc:literal) => {
        Tokenizer {
            name: $name.into(),
            description: $desc.into(),
            info_bytes: include_bytes!(concat!("tokenizers/", $name, ".json")),
            hf_bytes: include_bytes!(concat!("hf-tokenizers/", $name, ".json")),
            info: None,
            add_tokens: 0,
        }
    };
    ($username:literal, $name:literal, $desc:literal, $add:literal) => {
        Tokenizer {
            name: $username.into(),
            description: $desc.into(),
            info_bytes: include_bytes!(concat!("tokenizers/", $name, ".json")),
            hf_bytes: include_bytes!(concat!("hf-tokenizers/", $name, ".json")),
            info: None,
            add_tokens: $add,
        }
    };
}

pub fn tokenizers() -> Vec<Tokenizer> {
    vec![
        tok!("gpt4", "cl100k_base, used by GPT-4 and GPT-3.5"),
        tok!("llama", "used by Llama, CodeLlama, etc."),
        tok!("codellama", "llama", "used by Llama, CodeLlama, etc.", 16),
        tok!("falcon", "used by Falcon 7b, 40b, etc."),
        tok!("mpt", "MPT"),
        tok!("phi", "Phi 1.5"),
        tok!("gpt2", "GPT-2"),
    ]
}

pub fn find_tokenizer(name: &str) -> Result<Tokenizer> {
    for mut t in tokenizers() {
        if t.name == name {
            t.load();
            return Ok(t);
        }
    }

    println!("unknown tokenizer: {}", name);
    println!("available tokenizers:");
    for t in tokenizers() {
        println!("  {:20} {}", t.name, t.description);
    }
    return Err(anyhow!("unknown tokenizer: {}", name));
}

fn from_hex(hex_str: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for i in (0..hex_str.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex_str[i..(i + 2)], 16)?);
    }
    Ok(bytes)
}

impl Tokenizer {
    fn load(&mut self) {
        if self.info.is_none() {
            let mut info = serde_json::from_slice::<TokenInfo>(self.info_bytes).unwrap();
            let max = vec![
                info.binary.values().max(),
                info.special.values().max(),
                info.text.values().max(),
            ]
            .iter()
            .filter_map(|x| *x)
            .max()
            .unwrap();
            assert!(*max < 1_000_000);
            info.vocab_size = Some(max + 1);
            let vocab_size = info.vocab_size.unwrap();
            for off in 0..self.add_tokens {
                info.special
                    .insert(format!("<extra_id_{}>", off), vocab_size + off);
            }
            info.vocab_size = Some(vocab_size + self.add_tokens);
            self.info = Some(info);
        }
    }
    pub fn tokrx_info(&self) -> TokRxInfo {
        let info = self.info.as_ref().unwrap();
        TokRxInfo {
            vocab_size: info.vocab_size.unwrap(),
            tok_eos: info.eos_token,
        }
    }
    pub fn token_bytes(&self) -> Vec<Vec<u8>> {
        let tinfo = self.tokrx_info();
        let mut r = Vec::with_capacity(tinfo.vocab_size as usize);
        r.resize_with(tinfo.vocab_size as usize, Vec::new);

        let info = self.info.as_ref().unwrap();

        for (k, v) in &info.text {
            let idx = *v as usize;
            r[idx] = k.as_bytes().to_vec();
        }

        for (k, v) in &info.binary {
            let idx = *v as usize;
            r[idx] = from_hex(k).unwrap();
        }

        r
    }
}
