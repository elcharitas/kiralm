use std::fmt;
use std::path::PathBuf;

use crate::gguf::GgufMetadata;

use super::EngineCapabilities;

const MIB: u64 = 1024 * 1024;
const DEFAULT_KV_BYTES_PER_TOKEN: u64 = 256 * 1024;
const FULL_GPU_OFFLOAD: u32 = 999;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizationProfile {
    Balanced,
    Throughput,
    Latency,
    Memory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuHardware {
    pub memory_bytes: u64,
    pub unified_memory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareProfile {
    pub total_ram_bytes: u64,
    pub available_ram_bytes: u64,
    pub physical_cpu_cores: usize,
    pub logical_cpu_cores: usize,
    pub gpu: Option<GpuHardware>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOptimizationMetadata {
    pub architecture: String,
    pub size_bytes: u64,
    pub layer_count: Option<u32>,
    pub trained_context: Option<u32>,
    pub kv_bytes_per_token_f16: Option<u64>,
    pub is_moe: bool,
    pub moe_layer_count: Option<u32>,
    pub expert_weight_bytes: Option<u64>,
    pub tokenizer_fingerprint: Option<String>,
}

impl ModelOptimizationMetadata {
    pub fn from_gguf(metadata: &GgufMetadata, size_bytes: u64) -> Self {
        let is_moe = metadata.expert_count_total.is_some();
        Self {
            architecture: metadata.architecture.clone(),
            size_bytes,
            layer_count: metadata.block_count.and_then(to_u32),
            trained_context: metadata.context_length.and_then(to_u32),
            kv_bytes_per_token_f16: kv_bytes_per_token_f16(metadata),
            is_moe,
            moe_layer_count: if is_moe {
                metadata.block_count.and_then(to_u32)
            } else {
                None
            },
            expert_weight_bytes: None,
            tokenizer_fingerprint: metadata.tokenizer_fingerprint.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftModelMetadata {
    pub path: PathBuf,
    pub architecture: String,
    pub size_bytes: u64,
    pub trained_context: Option<u32>,
    pub tokenizer_fingerprint: Option<String>,
}

impl DraftModelMetadata {
    pub fn from_gguf(path: PathBuf, metadata: &GgufMetadata, size_bytes: u64) -> Self {
        Self {
            path,
            architecture: metadata.architecture.clone(),
            size_bytes,
            trained_context: metadata.context_length.and_then(to_u32),
            tokenizer_fingerprint: metadata.tokenizer_fingerprint.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationRequest {
    pub profile: OptimizationProfile,
    pub hardware: HardwareProfile,
    pub model: ModelOptimizationMetadata,
    pub ram_headroom_bytes: u64,
    pub requested_context: Option<u32>,
    pub draft_model: Option<DraftModelMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionOutcome {
    Applied,
    Reduced,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationDecision {
    pub feature: &'static str,
    pub outcome: DecisionOutcome,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvCacheTier {
    F16,
    Q8,
    Q4,
}

impl KvCacheTier {
    fn llama_value(self) -> &'static str {
        match self {
            Self::F16 => "f16",
            Self::Q8 => "q8_0",
            Self::Q4 => "q4_0",
        }
    }

    fn scaled_bytes(self, f16_bytes: u64) -> u64 {
        match self {
            Self::F16 => f16_bytes,
            Self::Q8 => f16_bytes.saturating_add(1) / 2,
            Self::Q4 => f16_bytes.saturating_add(3) / 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationPlan {
    args: Vec<String>,
    pub decisions: Vec<OptimizationDecision>,
    pub context_size: u32,
    pub gpu_layers: Option<u32>,
    pub kv_cache_tier: KvCacheTier,
    pub estimated_ram_bytes: u64,
    pub memory_budget_bytes: u64,
}

impl OptimizationPlan {
    pub fn spawn_args(&self) -> &[String] {
        &self.args
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptimizationError {
    MissingRam,
    MissingModelSize,
    MissingContextMetadata,
    InsufficientMemory {
        required_bytes: u64,
        budget_bytes: u64,
    },
}

impl fmt::Display for OptimizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRam => formatter.write_str("total RAM must be greater than zero"),
            Self::MissingModelSize => formatter.write_str("model size must be greater than zero"),
            Self::MissingContextMetadata => {
                formatter.write_str("trained context is required without context control")
            }
            Self::InsufficientMemory {
                required_bytes,
                budget_bytes,
            } => write!(
                formatter,
                "optimization needs {required_bytes} bytes with a {budget_bytes} byte budget"
            ),
        }
    }
}

impl std::error::Error for OptimizationError {}

pub struct OptimizationPlanner<'a> {
    capabilities: &'a EngineCapabilities,
}

impl<'a> OptimizationPlanner<'a> {
    pub fn new(capabilities: &'a EngineCapabilities) -> Self {
        Self { capabilities }
    }

    pub fn plan(
        &self,
        request: &OptimizationRequest,
    ) -> Result<OptimizationPlan, OptimizationError> {
        if request.hardware.total_ram_bytes == 0 {
            return Err(OptimizationError::MissingRam);
        }
        if request.model.size_bytes == 0 {
            return Err(OptimizationError::MissingModelSize);
        }

        let memory_budget = memory_budget(request);
        let mut builder = PlanBuilder::new(self.capabilities);
        let cpu_expert_bytes = self.plan_moe(request, &mut builder);
        let gpu = self.plan_gpu(request, cpu_expert_bytes, &mut builder);
        self.plan_flash_attention(&mut builder);
        let kv_cache_tier = self.plan_kv_cache(request.profile, &mut builder);

        let model_ram = model_ram_bytes(request, gpu.offloaded_bytes);
        let kv_bytes = request
            .model
            .kv_bytes_per_token_f16
            .unwrap_or(DEFAULT_KV_BYTES_PER_TOKEN)
            .max(1);
        let kv_bytes = kv_cache_tier.scaled_bytes(kv_bytes).max(1);
        let target_context = target_context(request);
        let remaining_for_kv = memory_budget.saturating_sub(model_ram);
        let max_context = (remaining_for_kv / kv_bytes).min(u32::MAX as u64) as u32;
        if max_context == 0 {
            return Err(OptimizationError::InsufficientMemory {
                required_bytes: model_ram.saturating_add(kv_bytes),
                budget_bytes: memory_budget,
            });
        }
        let can_set_context = self.capabilities.supports("--ctx-size");
        let context_size = if can_set_context {
            target_context.min(max_context)
        } else {
            request
                .model
                .trained_context
                .ok_or(OptimizationError::MissingContextMetadata)?
                .max(1)
        };
        if context_size > max_context {
            return Err(OptimizationError::InsufficientMemory {
                required_bytes: model_ram
                    .saturating_add(kv_bytes.saturating_mul(context_size as u64)),
                budget_bytes: memory_budget,
            });
        }
        let (context_outcome, context_reason) = if !can_set_context {
            (
                DecisionOutcome::Skipped,
                format!("the engine requires its {context_size} token trained default"),
            )
        } else if context_size < target_context {
            (
                DecisionOutcome::Reduced,
                format!("selected {context_size} tokens from a {target_context} token target"),
            )
        } else {
            (
                DecisionOutcome::Applied,
                format!("selected {context_size} tokens from a {target_context} token target"),
            )
        };
        builder.option("--ctx-size", context_size.to_string());
        builder.decision("context", context_outcome, context_reason);

        self.plan_threads(request, &mut builder);
        self.plan_batch(request.profile, context_size, &mut builder);
        self.plan_mapping(request, model_ram, memory_budget, &mut builder);

        let base_estimate = model_ram.saturating_add(kv_bytes.saturating_mul(context_size as u64));
        let draft_bytes = self.plan_draft(
            request,
            context_size,
            base_estimate,
            memory_budget,
            &mut builder,
        );
        let estimated_ram_bytes = base_estimate.saturating_add(draft_bytes);

        Ok(OptimizationPlan {
            args: builder.args,
            decisions: builder.decisions,
            context_size,
            gpu_layers: gpu.layers,
            kv_cache_tier,
            estimated_ram_bytes,
            memory_budget_bytes: memory_budget,
        })
    }

    fn plan_moe(&self, request: &OptimizationRequest, builder: &mut PlanBuilder<'_>) -> u64 {
        let model = &request.model;
        let Some(gpu) = request.hardware.gpu else {
            builder.decision(
                "moe_offload",
                DecisionOutcome::Skipped,
                "no GPU memory needs expert offload".to_string(),
            );
            return 0;
        };
        if !model.is_moe {
            builder.decision(
                "moe_offload",
                DecisionOutcome::Skipped,
                "the model is dense".to_string(),
            );
            return 0;
        }

        let constrained = gpu.memory_bytes < model.size_bytes;
        let should_offload = match request.profile {
            OptimizationProfile::Latency => false,
            OptimizationProfile::Throughput => gpu.memory_bytes < model.size_bytes / 2,
            OptimizationProfile::Balanced => constrained,
            OptimizationProfile::Memory => true,
        };
        if !should_offload {
            builder.decision(
                "moe_offload",
                DecisionOutcome::Skipped,
                "the selected profile keeps experts on the GPU".to_string(),
            );
            return 0;
        }

        let expert_bytes = model.expert_weight_bytes.unwrap_or(0).min(model.size_bytes);
        if builder.flag("--cpu-moe") {
            builder.decision(
                "moe_offload",
                DecisionOutcome::Applied,
                "kept all expert weights on the CPU".to_string(),
            );
            return expert_bytes;
        }

        if let Some(layers) = model.moe_layer_count.filter(|layers| *layers > 0) {
            if builder.option("--n-cpu-moe", layers.to_string()) {
                builder.decision(
                    "moe_offload",
                    DecisionOutcome::Applied,
                    format!("kept experts from {layers} MoE layers on the CPU"),
                );
                return expert_bytes;
            }
        }

        builder.decision(
            "moe_offload",
            DecisionOutcome::Skipped,
            "the engine has no compatible MoE offload flag".to_string(),
        );
        0
    }

    fn plan_gpu(
        &self,
        request: &OptimizationRequest,
        cpu_expert_bytes: u64,
        builder: &mut PlanBuilder<'_>,
    ) -> GpuPlan {
        let Some(gpu) = request.hardware.gpu else {
            builder.decision(
                "gpu_offload",
                DecisionOutcome::Skipped,
                "no GPU was reported".to_string(),
            );
            return GpuPlan::default();
        };
        if !self.capabilities.supports("--n-gpu-layers") {
            builder.decision(
                "gpu_offload",
                DecisionOutcome::Skipped,
                "the engine does not support GPU layer selection".to_string(),
            );
            return GpuPlan::default();
        }

        let payload = request
            .model
            .size_bytes
            .saturating_sub(cpu_expert_bytes)
            .max(1);
        let reserve = (gpu.memory_bytes / 10).max(256 * MIB);
        let usable_gpu = gpu.memory_bytes.saturating_sub(reserve);
        let profile_share = match request.profile {
            OptimizationProfile::Balanced => 75,
            OptimizationProfile::Throughput => 90,
            OptimizationProfile::Latency => 100,
            OptimizationProfile::Memory => 50,
        };
        let target_bytes = payload.saturating_mul(profile_share) / 100;
        let selected_bytes = target_bytes.min(usable_gpu);

        let (layers, offloaded_bytes) = match request.model.layer_count {
            Some(total_layers) if total_layers > 0 => {
                let layers = selected_bytes
                    .saturating_mul(total_layers as u64)
                    .checked_div(payload)
                    .unwrap_or(0)
                    .min(total_layers as u64) as u32;
                let bytes = payload.saturating_mul(layers as u64) / total_layers as u64;
                (layers, bytes)
            }
            _ if selected_bytes >= payload => (FULL_GPU_OFFLOAD, payload),
            _ => (0, 0),
        };

        if layers == 0 {
            builder.decision(
                "gpu_offload",
                DecisionOutcome::Skipped,
                "GPU capacity is too small for a safe layer estimate".to_string(),
            );
            return GpuPlan::default();
        }

        builder.option("--n-gpu-layers", layers.to_string());
        let outcome = if layers == FULL_GPU_OFFLOAD || request.model.layer_count == Some(layers) {
            DecisionOutcome::Applied
        } else {
            DecisionOutcome::Reduced
        };
        builder.decision(
            "gpu_offload",
            outcome,
            format!("offloaded {layers} layers within the GPU reserve"),
        );
        GpuPlan {
            layers: Some(layers),
            offloaded_bytes: if gpu.unified_memory {
                0
            } else {
                offloaded_bytes
            },
        }
    }

    fn plan_flash_attention(&self, builder: &mut PlanBuilder<'_>) {
        if builder.option("--flash-attn", "on") {
            builder.decision(
                "flash_attention",
                DecisionOutcome::Applied,
                "enabled supported fused attention".to_string(),
            );
        } else {
            builder.decision(
                "flash_attention",
                DecisionOutcome::Skipped,
                "the engine does not advertise flash attention".to_string(),
            );
        }
    }

    fn plan_kv_cache(
        &self,
        profile: OptimizationProfile,
        builder: &mut PlanBuilder<'_>,
    ) -> KvCacheTier {
        let cache_flags = self.capabilities.supports("--cache-type-k")
            && self.capabilities.supports("--cache-type-v");
        if !cache_flags {
            builder.decision(
                "kv_cache",
                DecisionOutcome::Skipped,
                "the engine cannot set both KV cache types".to_string(),
            );
            return KvCacheTier::F16;
        }

        let tier = match profile {
            OptimizationProfile::Latency => KvCacheTier::F16,
            OptimizationProfile::Balanced | OptimizationProfile::Throughput => KvCacheTier::Q8,
            OptimizationProfile::Memory if self.capabilities.supports("--flash-attn") => {
                KvCacheTier::Q4
            }
            OptimizationProfile::Memory => KvCacheTier::Q8,
        };
        if tier == KvCacheTier::F16 {
            builder.decision(
                "kv_cache",
                DecisionOutcome::Skipped,
                "kept the native f16 cache for latency".to_string(),
            );
            return tier;
        }

        builder.option("--cache-type-k", tier.llama_value());
        builder.option("--cache-type-v", tier.llama_value());
        builder.decision(
            "kv_cache",
            DecisionOutcome::Applied,
            format!("selected {} KV storage", tier.llama_value()),
        );
        tier
    }

    fn plan_threads(&self, request: &OptimizationRequest, builder: &mut PlanBuilder<'_>) {
        let physical = request.hardware.physical_cpu_cores.max(1);
        let logical = request.hardware.logical_cpu_cores.max(physical);
        let generation_threads = match request.profile {
            OptimizationProfile::Memory => physical.div_ceil(2),
            _ => physical,
        };
        let batch_threads = match request.profile {
            OptimizationProfile::Latency | OptimizationProfile::Memory => physical,
            OptimizationProfile::Balanced | OptimizationProfile::Throughput => logical,
        };

        let generation_applied = builder.option("--threads", generation_threads.to_string());
        let batch_applied = builder.option("--threads-batch", batch_threads.to_string());
        let outcome = if generation_applied || batch_applied {
            DecisionOutcome::Applied
        } else {
            DecisionOutcome::Skipped
        };
        builder.decision(
            "threads",
            outcome,
            format!("selected {generation_threads} generation and {batch_threads} batch threads"),
        );
    }

    fn plan_batch(
        &self,
        profile: OptimizationProfile,
        context_size: u32,
        builder: &mut PlanBuilder<'_>,
    ) {
        let (batch_target, micro_target, parallel) = match profile {
            OptimizationProfile::Balanced => (1024, 256, 2),
            OptimizationProfile::Throughput => (2048, 512, 4),
            OptimizationProfile::Latency => (512, 256, 1),
            OptimizationProfile::Memory => (256, 64, 1),
        };
        let batch = floor_power_of_two(batch_target.min(context_size).max(1));
        let micro_batch = floor_power_of_two(micro_target.min(batch).max(1));

        let batch_applied = builder.option("--batch-size", batch.to_string());
        let micro_applied = builder.option("--ubatch-size", micro_batch.to_string());
        let parallel_applied = builder.option("--parallel", parallel.to_string());
        let outcome = if batch_applied || micro_applied || parallel_applied {
            DecisionOutcome::Applied
        } else {
            DecisionOutcome::Skipped
        };
        builder.decision(
            "batching",
            outcome,
            format!("selected batch {batch}, micro-batch {micro_batch}, and {parallel} slots"),
        );
    }

    fn plan_mapping(
        &self,
        request: &OptimizationRequest,
        model_ram: u64,
        memory_budget: u64,
        builder: &mut PlanBuilder<'_>,
    ) {
        if builder.flag("--mmap") {
            builder.decision(
                "mmap",
                DecisionOutcome::Applied,
                "mapped model pages for demand loading".to_string(),
            );
        } else {
            builder.decision(
                "mmap",
                DecisionOutcome::Skipped,
                "the engine does not advertise mmap control".to_string(),
            );
        }

        let wants_lock = matches!(
            request.profile,
            OptimizationProfile::Latency | OptimizationProfile::Throughput
        );
        let has_margin = model_ram.saturating_mul(5) <= memory_budget.saturating_mul(4);
        if wants_lock && has_margin && builder.flag("--mlock") {
            builder.decision(
                "mlock",
                DecisionOutcome::Applied,
                "locked model pages with at least 20 percent RAM margin".to_string(),
            );
        } else {
            let reason = if !wants_lock {
                "the selected profile favors reclaimable pages"
            } else if !has_margin {
                "the model lacks the RAM margin required for mlock"
            } else {
                "the engine does not advertise mlock"
            };
            builder.decision("mlock", DecisionOutcome::Skipped, reason.to_string());
        }
    }

    fn plan_draft(
        &self,
        request: &OptimizationRequest,
        context_size: u32,
        base_estimate: u64,
        memory_budget: u64,
        builder: &mut PlanBuilder<'_>,
    ) -> u64 {
        let Some(draft) = request.draft_model.as_ref() else {
            builder.decision(
                "draft_model",
                DecisionOutcome::Skipped,
                "no draft model was requested".to_string(),
            );
            return 0;
        };
        if request.profile == OptimizationProfile::Memory {
            builder.decision(
                "draft_model",
                DecisionOutcome::Skipped,
                "the memory profile reserves RAM instead of drafting".to_string(),
            );
            return 0;
        }
        if !self.capabilities.supports("--model-draft") {
            builder.decision(
                "draft_model",
                DecisionOutcome::Skipped,
                "the engine does not support draft models".to_string(),
            );
            return 0;
        }
        if draft.architecture != request.model.architecture {
            builder.decision(
                "draft_model",
                DecisionOutcome::Skipped,
                "draft and target architectures differ".to_string(),
            );
            return 0;
        }
        let tokenizers_match = draft.tokenizer_fingerprint.is_some()
            && draft.tokenizer_fingerprint == request.model.tokenizer_fingerprint;
        if !tokenizers_match {
            builder.decision(
                "draft_model",
                DecisionOutcome::Skipped,
                "draft and target tokenizers are not proven identical".to_string(),
            );
            return 0;
        }
        if draft.size_bytes >= request.model.size_bytes {
            builder.decision(
                "draft_model",
                DecisionOutcome::Skipped,
                "the draft model is not smaller than the target".to_string(),
            );
            return 0;
        }
        if draft.trained_context.unwrap_or(0) < context_size {
            builder.decision(
                "draft_model",
                DecisionOutcome::Skipped,
                "the draft context is shorter than the selected context".to_string(),
            );
            return 0;
        }
        if base_estimate.saturating_add(draft.size_bytes) > memory_budget {
            builder.decision(
                "draft_model",
                DecisionOutcome::Skipped,
                "the draft model would exceed the RAM budget".to_string(),
            );
            return 0;
        }

        builder.option("--model-draft", draft.path.display().to_string());
        builder.decision(
            "draft_model",
            DecisionOutcome::Applied,
            "paired a smaller model with matching tokenizer metadata".to_string(),
        );
        draft.size_bytes
    }
}

#[derive(Default)]
struct GpuPlan {
    layers: Option<u32>,
    offloaded_bytes: u64,
}

struct PlanBuilder<'a> {
    capabilities: &'a EngineCapabilities,
    args: Vec<String>,
    decisions: Vec<OptimizationDecision>,
}

impl<'a> PlanBuilder<'a> {
    fn new(capabilities: &'a EngineCapabilities) -> Self {
        Self {
            capabilities,
            args: Vec::new(),
            decisions: Vec::new(),
        }
    }

    fn flag(&mut self, flag: &'static str) -> bool {
        if !self.capabilities.supports(flag) {
            return false;
        }
        self.args.push(flag.to_string());
        true
    }

    fn option(&mut self, flag: &'static str, value: impl Into<String>) -> bool {
        if !self.capabilities.supports(flag) {
            return false;
        }
        self.args.push(flag.to_string());
        self.args.push(value.into());
        true
    }

    fn decision(&mut self, feature: &'static str, outcome: DecisionOutcome, reason: String) {
        self.decisions.push(OptimizationDecision {
            feature,
            outcome,
            reason,
        });
    }
}

fn memory_budget(request: &OptimizationRequest) -> u64 {
    let hard_limit = request
        .hardware
        .total_ram_bytes
        .saturating_sub(request.ram_headroom_bytes);
    if request.hardware.available_ram_bytes == 0 {
        hard_limit
    } else {
        hard_limit.min(request.hardware.available_ram_bytes)
    }
}

fn model_ram_bytes(request: &OptimizationRequest, offloaded_bytes: u64) -> u64 {
    if request.hardware.gpu.is_some_and(|gpu| !gpu.unified_memory) {
        request.model.size_bytes.saturating_sub(offloaded_bytes)
    } else {
        request.model.size_bytes
    }
}

fn target_context(request: &OptimizationRequest) -> u32 {
    let profile_target = match request.profile {
        OptimizationProfile::Balanced => 8192,
        OptimizationProfile::Throughput => 16384,
        OptimizationProfile::Latency => 4096,
        OptimizationProfile::Memory => 2048,
    };
    request
        .requested_context
        .unwrap_or(profile_target)
        .max(1)
        .min(
            request
                .model
                .trained_context
                .unwrap_or(profile_target)
                .max(1),
        )
}

fn floor_power_of_two(value: u32) -> u32 {
    1 << (31 - value.max(1).leading_zeros())
}

fn to_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok()
}

fn kv_bytes_per_token_f16(metadata: &GgufMetadata) -> Option<u64> {
    let blocks = metadata.block_count? as u128;
    let embedding = metadata.embedding_length? as u128;
    let heads = metadata.attention_head_count? as u128;
    let kv_heads = metadata.attention_head_count_kv.unwrap_or(heads as u64) as u128;
    if heads == 0 || kv_heads == 0 {
        return None;
    }
    let bytes = blocks
        .saturating_mul(embedding)
        .saturating_mul(kv_heads)
        .saturating_mul(4)
        / heads;
    u64::try_from(bytes).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const GIB: u64 = 1024 * MIB;

    fn capabilities(flags: &[&str]) -> EngineCapabilities {
        EngineCapabilities {
            version: "test".to_string(),
            flags: flags.iter().map(|flag| (*flag).to_string()).collect(),
        }
    }

    fn all_capabilities() -> EngineCapabilities {
        capabilities(&[
            "--batch-size",
            "--cache-type-k",
            "--cache-type-v",
            "--cpu-moe",
            "--ctx-size",
            "--flash-attn",
            "--mlock",
            "--mmap",
            "--model-draft",
            "--n-cpu-moe",
            "--n-gpu-layers",
            "--parallel",
            "--threads",
            "--threads-batch",
            "--ubatch-size",
        ])
    }

    fn request(profile: OptimizationProfile) -> OptimizationRequest {
        OptimizationRequest {
            profile,
            hardware: HardwareProfile {
                total_ram_bytes: 32 * GIB,
                available_ram_bytes: 24 * GIB,
                physical_cpu_cores: 8,
                logical_cpu_cores: 16,
                gpu: Some(GpuHardware {
                    memory_bytes: 12 * GIB,
                    unified_memory: false,
                }),
            },
            model: ModelOptimizationMetadata {
                architecture: "llama".to_string(),
                size_bytes: 8 * GIB,
                layer_count: Some(32),
                trained_context: Some(32768),
                kv_bytes_per_token_f16: Some(512 * 1024),
                is_moe: false,
                moe_layer_count: None,
                expert_weight_bytes: None,
                tokenizer_fingerprint: Some("tokenizer-a".to_string()),
            },
            ram_headroom_bytes: 2 * GIB,
            requested_context: None,
            draft_model: None,
        }
    }

    #[test]
    fn gguf_metadata_builds_runtime_estimates() {
        let metadata = GgufMetadata {
            architecture: "llama".to_string(),
            context_length: Some(32768),
            block_count: Some(32),
            embedding_length: Some(4096),
            attention_head_count: Some(32),
            attention_head_count_kv: Some(8),
            expert_count_total: None,
            expert_count_active: None,
            tokenizer_fingerprint: Some("tokens".to_string()),
        };
        let model = ModelOptimizationMetadata::from_gguf(&metadata, 8 * GIB);
        let draft = DraftModelMetadata::from_gguf(PathBuf::from("draft.gguf"), &metadata, GIB);

        assert_eq!(model.layer_count, Some(32));
        assert_eq!(model.trained_context, Some(32768));
        assert_eq!(model.kv_bytes_per_token_f16, Some(128 * 1024));
        assert_eq!(model.tokenizer_fingerprint.as_deref(), Some("tokens"));
        assert_eq!(draft.trained_context, Some(32768));
        assert_eq!(draft.tokenizer_fingerprint.as_deref(), Some("tokens"));
    }

    fn option_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .map(String::as_str)
    }

    #[test]
    fn balanced_plan_covers_supported_runtime_controls() {
        let caps = all_capabilities();
        let plan = OptimizationPlanner::new(&caps)
            .plan(&request(OptimizationProfile::Balanced))
            .unwrap();

        assert_eq!(
            option_value(plan.spawn_args(), "--n-gpu-layers"),
            Some("24")
        );
        assert_eq!(
            option_value(plan.spawn_args(), "--cache-type-k"),
            Some("q8_0")
        );
        assert_eq!(option_value(plan.spawn_args(), "--ctx-size"), Some("8192"));
        assert_eq!(option_value(plan.spawn_args(), "--threads"), Some("8"));
        assert_eq!(
            option_value(plan.spawn_args(), "--threads-batch"),
            Some("16")
        );
        assert!(plan.spawn_args().iter().any(|arg| arg == "--mmap"));
        assert!(!plan.spawn_args().iter().any(|arg| arg == "--mlock"));
    }

    #[test]
    fn memory_plan_uses_moe_and_q4_when_supported() {
        let mut input = request(OptimizationProfile::Memory);
        input.model.is_moe = true;
        input.model.moe_layer_count = Some(24);
        input.model.expert_weight_bytes = Some(5 * GIB);
        let caps = all_capabilities();
        let plan = OptimizationPlanner::new(&caps).plan(&input).unwrap();

        assert!(plan.spawn_args().iter().any(|arg| arg == "--cpu-moe"));
        assert_eq!(plan.kv_cache_tier, KvCacheTier::Q4);
        assert_eq!(
            option_value(plan.spawn_args(), "--cache-type-v"),
            Some("q4_0")
        );
        assert!(!plan.spawn_args().iter().any(|arg| arg == "--mlock"));
    }

    #[test]
    fn moe_layer_fallback_uses_only_the_advertised_flag() {
        let mut input = request(OptimizationProfile::Balanced);
        input.hardware.gpu.as_mut().unwrap().memory_bytes = 4 * GIB;
        input.model.is_moe = true;
        input.model.moe_layer_count = Some(24);
        input.model.expert_weight_bytes = Some(4 * GIB);
        let caps = capabilities(&["--ctx-size", "--n-cpu-moe", "--n-gpu-layers"]);
        let plan = OptimizationPlanner::new(&caps).plan(&input).unwrap();

        assert_eq!(option_value(plan.spawn_args(), "--n-cpu-moe"), Some("24"));
        assert!(!plan.spawn_args().iter().any(|arg| arg == "--cpu-moe"));
    }

    #[test]
    fn identical_inputs_produce_identical_latency_plans() {
        let input = request(OptimizationProfile::Latency);
        let caps = all_capabilities();
        let planner = OptimizationPlanner::new(&caps);
        let first = planner.plan(&input).unwrap();
        let second = planner.plan(&input).unwrap();

        assert_eq!(first, second);
        assert!(first.spawn_args().iter().any(|arg| arg == "--mmap"));
        assert!(first.spawn_args().iter().any(|arg| arg == "--mlock"));
    }

    #[test]
    fn context_shrinks_to_stay_inside_headroom() {
        let mut input = request(OptimizationProfile::Throughput);
        input.hardware.gpu = None;
        input.hardware.total_ram_bytes = 10 * GIB;
        input.hardware.available_ram_bytes = 10 * GIB;
        input.ram_headroom_bytes = GIB;
        input.requested_context = Some(16384);
        input.model.kv_bytes_per_token_f16 = Some(MIB);
        let caps = all_capabilities();
        let plan = OptimizationPlanner::new(&caps).plan(&input).unwrap();

        assert_eq!(plan.context_size, 2048);
        assert!(plan.estimated_ram_bytes <= plan.memory_budget_bytes);
        assert_eq!(
            plan.decisions
                .iter()
                .find(|decision| decision.feature == "context")
                .unwrap()
                .outcome,
            DecisionOutcome::Reduced
        );
    }

    #[test]
    fn draft_requires_matching_metadata_and_memory() {
        let mut input = request(OptimizationProfile::Latency);
        input.draft_model = Some(DraftModelMetadata {
            path: PathBuf::from("draft.gguf"),
            architecture: "llama".to_string(),
            size_bytes: GIB,
            trained_context: Some(8192),
            tokenizer_fingerprint: Some("tokenizer-a".to_string()),
        });
        let caps = all_capabilities();
        let plan = OptimizationPlanner::new(&caps).plan(&input).unwrap();
        assert_eq!(
            option_value(plan.spawn_args(), "--model-draft"),
            Some("draft.gguf")
        );

        input.draft_model.as_mut().unwrap().tokenizer_fingerprint = None;
        let plan = OptimizationPlanner::new(&caps).plan(&input).unwrap();
        assert!(option_value(plan.spawn_args(), "--model-draft").is_none());
    }

    #[test]
    fn every_emitted_flag_was_advertised() {
        let candidates = [
            "--batch-size",
            "--cache-type-k",
            "--cache-type-v",
            "--cpu-moe",
            "--ctx-size",
            "--flash-attn",
            "--mlock",
            "--mmap",
            "--model-draft",
            "--n-cpu-moe",
            "--n-gpu-layers",
            "--parallel",
            "--threads",
            "--threads-batch",
            "--ubatch-size",
        ];
        for mask in 0u32..(1 << candidates.len()) {
            let flags: BTreeSet<String> = candidates
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, flag)| (*flag).to_string())
                .collect();
            let caps = EngineCapabilities {
                version: "test".to_string(),
                flags,
            };
            let plan = OptimizationPlanner::new(&caps)
                .plan(&request(OptimizationProfile::Balanced))
                .unwrap();
            for argument in plan.spawn_args() {
                if argument.starts_with("--") {
                    assert!(caps.supports(argument), "emitted {argument}");
                }
            }
        }
    }

    #[test]
    fn context_never_exceeds_training_or_request_boundaries() {
        let caps = all_capabilities();
        for trained in [1, 512, 4096, 32768] {
            for requested in [1, 256, 2048, 65536] {
                let mut input = request(OptimizationProfile::Throughput);
                input.model.trained_context = Some(trained);
                input.requested_context = Some(requested);
                let plan = OptimizationPlanner::new(&caps).plan(&input).unwrap();
                assert!(plan.context_size <= trained);
                assert!(plan.context_size <= requested);
                assert!(plan.estimated_ram_bytes <= plan.memory_budget_bytes);
            }
        }
    }

    #[test]
    fn missing_context_flag_uses_only_a_safe_trained_default() {
        let caps = capabilities(&[]);
        let mut input = request(OptimizationProfile::Memory);
        input.model.trained_context = Some(4096);
        let plan = OptimizationPlanner::new(&caps).plan(&input).unwrap();
        assert_eq!(plan.context_size, 4096);
        assert!(option_value(plan.spawn_args(), "--ctx-size").is_none());

        input.hardware.available_ram_bytes = input.model.size_bytes + MIB;
        assert!(matches!(
            OptimizationPlanner::new(&caps).plan(&input),
            Err(OptimizationError::InsufficientMemory { .. })
        ));

        input.model.trained_context = None;
        assert_eq!(
            OptimizationPlanner::new(&caps).plan(&input),
            Err(OptimizationError::MissingContextMetadata)
        );
    }

    #[test]
    fn insufficient_memory_returns_a_boundary_error() {
        let mut input = request(OptimizationProfile::Memory);
        input.hardware.gpu = None;
        input.hardware.total_ram_bytes = input.model.size_bytes;
        input.hardware.available_ram_bytes = input.model.size_bytes;
        input.ram_headroom_bytes = 1;
        let caps = all_capabilities();

        assert!(matches!(
            OptimizationPlanner::new(&caps).plan(&input),
            Err(OptimizationError::InsufficientMemory { .. })
        ));
    }
}
