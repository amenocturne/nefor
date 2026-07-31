use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompileProfile {
    pub phases: PhaseDurations,
    pub counters: OperationCounters,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhaseDurations {
    pub entry_read_ns: u64,
    pub entry_lex_ns: u64,
    pub entry_parse_ns: u64,
    pub entry_evaluate_ns: u64,
    pub module_resolve_ns: u64,
    pub module_read_ns: u64,
    pub module_lex_ns: u64,
    pub module_parse_ns: u64,
    pub module_evaluate_ns: u64,
    pub checking_ns: u64,
    pub artifact_conversion_ns: u64,
    pub artifact_serialize_hash_ns: u64,
    pub resident_rule_validation_ns: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationCounters {
    pub evaluator_steps: u64,
    pub function_calls: u64,
    pub builtin_calls: u64,
    pub environment_snapshots: u64,
    pub environment_snapshot_bindings: u64,
    pub module_requests: u64,
    pub module_cache_hits: u64,
    pub modules_loaded: u64,
    pub file_read_requests: u64,
    pub file_read_cache_hits: u64,
    pub file_read_cache_misses: u64,
    pub memoized_call_hits: u64,
    pub memoized_call_misses: u64,
    pub memoized_call_stores: u64,
    pub resident_rules_validated: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CompileProfiler {
    inner: Arc<Mutex<CompileProfile>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Phase {
    EntryRead,
    EntryLex,
    EntryParse,
    EntryEvaluate,
    ModuleResolve,
    ModuleRead,
    ModuleLex,
    ModuleParse,
    ModuleEvaluate,
    Checking,
    ArtifactConversion,
    ArtifactSerializeHash,
    ResidentRuleValidation,
}

impl CompileProfiler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> CompileProfile {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn reset(&self) {
        *self.inner.lock().unwrap_or_else(|error| error.into_inner()) = CompileProfile::default();
    }

    pub(crate) fn add_phase(&self, phase: Phase, elapsed: Duration) {
        let ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        let mut profile = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let target = match phase {
            Phase::EntryRead => &mut profile.phases.entry_read_ns,
            Phase::EntryLex => &mut profile.phases.entry_lex_ns,
            Phase::EntryParse => &mut profile.phases.entry_parse_ns,
            Phase::EntryEvaluate => &mut profile.phases.entry_evaluate_ns,
            Phase::ModuleResolve => &mut profile.phases.module_resolve_ns,
            Phase::ModuleRead => &mut profile.phases.module_read_ns,
            Phase::ModuleLex => &mut profile.phases.module_lex_ns,
            Phase::ModuleParse => &mut profile.phases.module_parse_ns,
            Phase::ModuleEvaluate => &mut profile.phases.module_evaluate_ns,
            Phase::Checking => &mut profile.phases.checking_ns,
            Phase::ArtifactConversion => &mut profile.phases.artifact_conversion_ns,
            Phase::ArtifactSerializeHash => &mut profile.phases.artifact_serialize_hash_ns,
            Phase::ResidentRuleValidation => &mut profile.phases.resident_rule_validation_ns,
        };
        *target = target.saturating_add(ns);
    }

    pub(crate) fn update_counters(&self, update: impl FnOnce(&mut OperationCounters)) {
        update(
            &mut self
                .inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .counters,
        );
    }
}
