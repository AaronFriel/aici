use super::{cache_engine::CacheEngine, scheduler::SchedulerOutputs};
use crate::{
    config::RllmConfig,
    llm::kernels::to_offsets,
    seq::{SchedulingPhase, SeqId},
};
use aicirt::api::Token;
use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};
use tch::Tensor;

pub struct BatchInfo {
    pub tokens: Tensor,         // u32, [num_tokens]
    pub positions: Tensor,      // i64, [num_tokens]
    pub seqlens_q: Tensor,      // u32, [batch_size + 1]; points to tokens/positions
    pub seqlens_k: Tensor,      // u32, [batch_size + 1]; can go outside tokens/positions
    pub gather_mapping: Tensor, // u32, [sum(context_len + prompt_len)]
    pub slot_mapping: Tensor,   // u32, [num_tokens]
    pub max_seqlen_q: usize,
    pub max_seqlen_k: usize,
    pub seq_id_to_idx: HashMap<SeqId, usize>, // seq_id -> index into seqlens_*
    pub kv_cache: Vec<(Tensor, Tensor)>,

    pub infer_log: Mutex<Vec<(String, Tensor)>>,
    pub step_no: usize,
}

impl BatchInfo {
    pub fn log_tensor(&self, key: &str, value: &Tensor) {
        if false {
            self.infer_log
                .lock()
                .unwrap()
                .push((key.to_string(), value.copy()));
        }
    }

    pub fn save_log(&self, filename: &str) {
        let mut lck = self.infer_log.lock().unwrap();
        if lck.len() == 0 {
            return;
        }
        let tensors = lck
            .iter()
            .enumerate()
            .map(|(i, (k, v))| (format!("{:0>4}_{}", i, k), v.copy()))
            .collect::<Vec<_>>();
        lck.clear();
        Tensor::write_safetensors(&tensors, filename).unwrap();
    }
}

impl Debug for BatchInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchInfo")
            .field("tokens", &self.tokens)
            .field("positions", &self.positions)
            .field("seqlens_q", &self.seqlens_q)
            .field("seqlens_k", &self.seqlens_k)
            .field("gather_mapping", &self.gather_mapping.numel())
            .field("slot_mapping", &self.slot_mapping.numel())
            .field("max_seqlen_q", &self.max_seqlen_q)
            .field("max_seqlen_k", &self.max_seqlen_k)
            .finish()
    }
}

pub struct BatchInfoBuilder {
    entries: Vec<BatchEntry>,
    config: Arc<RllmConfig>,
}

struct BatchEntry {
    seq_id: SeqId,
    query_pos_token: Vec<(usize, Token)>,
    kv_slots: Vec<usize>,
}

impl BatchInfoBuilder {
    pub fn new(config: Arc<RllmConfig>) -> Self {
        Self {
            entries: Vec::new(),
            config,
        }
    }

    pub fn sched_out(&mut self, sched_out: &mut SchedulerOutputs) -> &mut Self {
        assert!(sched_out.next_seq_groups.len() > 0);
        for sg in sched_out.next_seq_groups.iter_mut() {
            for seq in sg.seqs.iter_mut() {
                if seq.sched_phase != SchedulingPhase::Running {
                    continue;
                }

                let seq_len = seq.get_len();
                let k_len = seq_len;
                log::trace!("seq: {seq:?}");
                let mut q_len = seq.get_len() - seq.num_kv_computed;
                if q_len == 0 {
                    // just re-compute the last token
                    q_len = 1;
                }
                sg.usage.gen_tokens += 1;
                sg.usage.prompt_tokens += q_len;

                let off = k_len - q_len;
                self.entries.push(BatchEntry {
                    seq_id: seq.seq_id,
                    query_pos_token: (off..off + q_len)
                        .map(|idx| (idx, seq.get_token(idx)))
                        .collect(),
                    kv_slots: (0..k_len).map(|idx| seq.get_gpu_slot(idx)).collect(),
                });
            }
        }

        self
    }

    pub fn profile_run(&mut self) -> BatchInfo {
        let sch_cfg = &self.config.clone().scheduler;
        let seq_len = sch_cfg.max_model_len;
        let max_num_seqs = sch_cfg.max_num_seqs;
        let avg_len = sch_cfg.max_num_kv_tokens / max_num_seqs;

        let fake_token = 12;
        let fake_slot = 0; // has to be 0 - we only have 1 slot in our fake kv cache
        let seq_id = 424242;

        for idx in 0..max_num_seqs {
            self.entries.push(BatchEntry {
                seq_id,
                query_pos_token: (0..1).map(|_| (idx, fake_token)).collect(),
                kv_slots: (0..avg_len).map(|_| fake_slot).collect(),
            });
        }

        let mut left = sch_cfg.max_num_batched_tokens - max_num_seqs;
        while left > 0 {
            let seq_len = std::cmp::min(seq_len, left);
            left -= seq_len;
            self.entries.push(BatchEntry {
                seq_id,
                query_pos_token: (0..seq_len).map(|idx| (idx, fake_token)).collect(),
                kv_slots: (0..seq_len).map(|_| fake_slot).collect(),
            });
        }

        let res = self.fake_finish();

        log::info!("profile: {res:?}");

        res
    }

    fn fake_finish(&self) -> BatchInfo {
        let (k, v) = CacheEngine::alloc_gpu_cache_layer(&self.config, 1);
        let num_layers = self.config.get_num_layers_parallel();
        let kv_cache = (0..num_layers)
            .map(|_| (k.shallow_clone(), v.shallow_clone()))
            .collect();
        self.finish(0, kv_cache)
    }

    pub fn finish(&self, step_no: usize, kv_cache: Vec<(Tensor, Tensor)>) -> BatchInfo {
        let mut positions: Vec<i64> = Vec::new();
        let mut tokens: Vec<i32> = Vec::new();
        let mut seqlens_q: Vec<usize> = Vec::new();
        let mut seqlens_k: Vec<usize> = Vec::new();
        let mut gather_mapping: Vec<i32> = Vec::new();
        let mut slot_mapping: Vec<i32> = Vec::new();
        let mut seq_id_to_idx: HashMap<SeqId, usize> = HashMap::new();

        let max_seq = self.config.scheduler.max_model_len;
        for e in &self.entries {
            seq_id_to_idx.insert(e.seq_id, seqlens_q.len());
            let query = &e.query_pos_token;
            let off = e.kv_slots.len() - query.len();
            for (qidx, (tpos, token)) in query.iter().enumerate() {
                assert!(*tpos < max_seq);
                positions.push(*tpos as i64);
                tokens.push(*token as i32);
                slot_mapping.push(e.kv_slots[off + qidx] as i32);
            }
            for slot in e.kv_slots.iter() {
                gather_mapping.push(*slot as i32);
            }
            seqlens_q.push(query.len());
            seqlens_k.push(e.kv_slots.len());
        }

        assert!(seqlens_q.len() > 0);

        let device = self.config.device;
        let (max_seqlen_q, seqlens_q) = to_offsets(seqlens_q.into_iter(), device);
        let (max_seqlen_k, seqlens_k) = to_offsets(seqlens_k.into_iter(), device);

        // TODO positions, tokens should be padded to 8? see worker.py, search for multiple_of=8
        let positions = Tensor::from_slice(positions.as_slice()).to(device);
        let tokens = Tensor::from_slice(tokens.as_slice()).to(device);
        let slot_mapping = Tensor::from_slice(slot_mapping.as_slice()).to(device);
        let gather_mapping = Tensor::from_slice(gather_mapping.as_slice()).to(device);

        BatchInfo {
            tokens,
            positions,
            seqlens_q,
            seqlens_k,
            slot_mapping,
            gather_mapping,
            max_seqlen_q,
            max_seqlen_k,
            kv_cache,
            seq_id_to_idx,
            infer_log: Mutex::new(Vec::new()),
            step_no,
        }
    }
}
