//! Canonical model + variant → real model ID + real params.
//!
//! OpenCode 配置 canonical 模型（如 `gemini-3.5-flash`）带 variants（low/medium/high）。
//! 收到请求后，AM 根据 canonical + 档位映射到抓包验证可用的目标模型 ID
//! （如 `gemini-3.8-flash-high`），并保留现代模型的 `thinkingLevel` / 安全限额。

/// Variant tier inferred from the client's `thinking.budget_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantTier {
    Low,
    Medium,
    High,
}

impl VariantTier {
    pub const fn as_effort_str(&self) -> &'static str {
        match self {
            VariantTier::Low => "low",
            VariantTier::Medium => "medium",
            VariantTier::High => "high",
        }
    }
}

/// How an alias selects a variant tier within its canonical Gemini family.
#[derive(Debug, Clone, Copy)]
pub enum AliasPolicy {
    HonorTier,
    Fixed(VariantTier),
}

/// Static metadata and tier routing for one canonical Gemini model family.
pub struct CanonicalFamily {
    pub canonical_id: &'static str,
    pub display_name: &'static str,
    pub context_limit: u32,
    pub output_limit: u32,
    pub input_modalities: &'static [&'static str],
    pub output_modalities: &'static [&'static str],
    pub reasoning: bool,
    pub tiers: &'static [(VariantTier, RealModelSpec)],
    pub aliases: &'static [(&'static str, AliasPolicy)],
}

/// A resolved real model with its verified request params.
#[derive(Debug, Clone, Copy)]
pub struct RealModelSpec {
    /// The real model ID to put in the upstream `model` field.
    pub id: &'static str,
    /// verified thinkingBudget (0 means no thinking).
    pub thinking_budget: u32,
    /// verified maxOutputTokens.
    pub max_output_tokens: u32,
    /// Whether to include thoughts in the response.
    pub include_thoughts: bool,
    /// Whether a Claude client-selected thinking budget may pass through.
    pub preserve_client_budget: bool,
}

impl RealModelSpec {
    /// Select the upstream thinking budget for this resolved model.
    pub const fn effective_thinking_budget(&self, client_budget: Option<u32>) -> u32 {
        if self.preserve_client_budget {
            match client_budget {
                Some(budget) => budget,
                None => self.thinking_budget,
            }
        } else {
            self.thinking_budget
        }
    }
}

/// Infer the variant tier from the client-sent `thinking.budget_tokens`.
///
/// OpenCode derives budgetTokens from its model capabilities (max = budget-1,
/// high = floor(budget/2)), so the exact value is not the real budget —
/// we only use its magnitude to guess which tier the user selected. When no budget
/// is present we default to High (the most capable tier).
pub fn infer_tier(budget_tokens: Option<u32>) -> VariantTier {
    match budget_tokens {
        Some(b) if b < 2000 => VariantTier::Low,
        Some(b) if b < 7000 => VariantTier::Medium,
        _ => VariantTier::High,
    }
}

/// Parse a supported Anthropic SDK output effort into a Gemini variant tier.
pub fn tier_from_effort(effort: Option<&str>) -> Option<VariantTier> {
    match effort {
        Some("low") => Some(VariantTier::Low),
        Some("medium") => Some(VariantTier::Medium),
        Some("high") => Some(VariantTier::High),
        _ => None,
    }
}

/// Resolve a canonical family + tier, returning both the real model spec and the resolved tier.
pub fn resolve_family_variant(
    canonical: &str,
    tier: VariantTier,
) -> Option<(RealModelSpec, VariantTier)> {
    let key = canonical.to_lowercase();
    for family in GEMINI_FAMILIES {
        let resolved_tier = if family.canonical_id == key.as_str() {
            tier
        } else if let Some((_, policy)) = family
            .aliases
            .iter()
            .find(|(alias, _)| *alias == key.as_str())
        {
            match *policy {
                AliasPolicy::HonorTier => tier,
                AliasPolicy::Fixed(fixed_tier) => fixed_tier,
            }
        } else {
            continue;
        };

        return family
            .tiers
            .iter()
            .find(|(candidate_tier, _)| *candidate_tier == resolved_tier)
            .map(|(_, spec)| (*spec, resolved_tier));
    }

    None
}

/// Resolve a canonical model + tier to the real model + params.
///
/// Returns None for models that have no variant split (use
/// [`resolve_non_variant_model`] for those).
pub fn resolve_real_model(canonical: &str, tier: VariantTier) -> Option<RealModelSpec> {
    resolve_family_variant(canonical, tier).map(|(spec, _)| spec)
}

/// Resolve a canonical model that has NO variant split — use real ID + params directly.
///
/// Covers models the user may configure without variants (3.1 flash-lite, claude, gpt-oss).
/// Note: `gemini-2.5-flash*` and `gemini-3-flash` are standalone active models and must NOT
/// be hijacked into `gemini-3.1-flash-lite` or legacy `-agent` names.
pub fn resolve_non_variant_model(model: &str) -> Option<RealModelSpec> {
    let key = model.to_lowercase();
    if key == "gemini-3.1-flash-lite" {
        return Some(SPEC_31_FLASH_LITE);
    }
    if key == "claude-sonnet-4-6" {
        return Some(SPEC_CLAUDE_SONNET_46);
    }
    if matches!(key.as_str(), "claude-opus-4-6-thinking" | "claude-opus-4-6") {
        return Some(SPEC_CLAUDE_OPUS_46);
    }
    if key == "gpt-oss-120b-medium" {
        return Some(SPEC_GPT_OSS_120B);
    }
    None
}

/// Resolve a canonical Gemini family model with an optional explicit variant tier.
pub fn resolve_family_variant_with_tier(
    canonical: &str,
    explicit_tier: Option<VariantTier>,
    budget_tokens: Option<u32>,
) -> Option<(RealModelSpec, VariantTier)> {
    let tier = explicit_tier.unwrap_or_else(|| infer_tier(budget_tokens));
    resolve_family_variant(canonical, tier)
}

/// Resolve a model with an optional explicit variant tier.
///
/// An explicit tier takes precedence over the client thinking budget.
pub fn resolve_with_tier(
    canonical: &str,
    explicit_tier: Option<VariantTier>,
    budget_tokens: Option<u32>,
) -> Option<RealModelSpec> {
    let tier = explicit_tier.unwrap_or_else(|| infer_tier(budget_tokens));
    resolve_real_model(canonical, tier).or_else(|| resolve_non_variant_model(canonical))
}

/// Top-level compatibility resolver using the client thinking budget.
pub fn resolve(canonical: &str, budget_tokens: Option<u32>) -> Option<RealModelSpec> {
    resolve_with_tier(canonical, None, budget_tokens)
}

// ── verified real model specs (aligned with official captures) ──
// gemini-3.8-flash family (modern thinkingLevel models replacing deprecated -agent/-extra-low)
const SPEC_38_FLASH_LOW: RealModelSpec = RealModelSpec {
    id: "gemini-3.8-flash-low",
    thinking_budget: 0,
    max_output_tokens: 65536,
    include_thoughts: true,
    preserve_client_budget: false,
};
const SPEC_38_FLASH_MEDIUM: RealModelSpec = RealModelSpec {
    id: "gemini-3.8-flash-medium",
    thinking_budget: 0,
    max_output_tokens: 65536,
    include_thoughts: true,
    preserve_client_budget: false,
};
const SPEC_38_FLASH_HIGH: RealModelSpec = RealModelSpec {
    id: "gemini-3.8-flash-high",
    thinking_budget: 0,
    max_output_tokens: 65536,
    include_thoughts: true,
    preserve_client_budget: false,
};

// gemini-3.1-pro family (maxOutputTokens = 65535; account-level fallback to gemini-pro-agent
// happens dynamically in TokenManager::resolve_dynamic_model_for_account when needed)
const SPEC_31_PRO_LOW: RealModelSpec = RealModelSpec {
    id: "gemini-3.1-pro-low",
    thinking_budget: 1001,
    max_output_tokens: 65535,
    include_thoughts: true,
    preserve_client_budget: false,
};
const SPEC_31_PRO_PREVIEW: RealModelSpec = RealModelSpec {
    id: "gemini-3.1-pro-preview",
    thinking_budget: 10001,
    max_output_tokens: 65535,
    include_thoughts: true,
    preserve_client_budget: false,
};

// Non-variant models
const SPEC_31_FLASH_LITE: RealModelSpec = RealModelSpec {
    id: "gemini-3.1-flash-lite",
    thinking_budget: 0,
    max_output_tokens: 65535,
    include_thoughts: true,
    preserve_client_budget: false,
};
const SPEC_CLAUDE_SONNET_46: RealModelSpec = RealModelSpec {
    id: "claude-sonnet-4-6",
    thinking_budget: 1024,
    max_output_tokens: 64000,
    include_thoughts: true,
    preserve_client_budget: true,
};
const SPEC_CLAUDE_OPUS_46: RealModelSpec = RealModelSpec {
    id: "claude-opus-4-6-thinking",
    thinking_budget: 1024,
    max_output_tokens: 64000,
    include_thoughts: true,
    preserve_client_budget: true,
};
const SPEC_GPT_OSS_120B: RealModelSpec = RealModelSpec {
    id: "gpt-oss-120b-medium",
    thinking_budget: 8192,
    max_output_tokens: 32768,
    include_thoughts: true,
    preserve_client_budget: false,
};

pub static GEMINI_FAMILIES: &[CanonicalFamily] = &[
    CanonicalFamily {
        canonical_id: "gemini-3.5-flash",
        display_name: "Gemini 3.5 Flash",
        context_limit: 1_000_000,
        output_limit: 65_536,
        input_modalities: &["text", "image", "audio", "video", "pdf"],
        output_modalities: &["text"],
        reasoning: true,
        tiers: &[
            (VariantTier::Low, SPEC_38_FLASH_LOW),
            (VariantTier::Medium, SPEC_38_FLASH_MEDIUM),
            (VariantTier::High, SPEC_38_FLASH_HIGH),
        ],
        aliases: &[
            ("gemini-3.5-flash-high", AliasPolicy::HonorTier),
            (
                "gemini-3.5-flash-medium",
                AliasPolicy::Fixed(VariantTier::Medium),
            ),
            ("gemini-3.5-flash-low", AliasPolicy::Fixed(VariantTier::Low)),
        ],
    },
    CanonicalFamily {
        canonical_id: "gemini-3.1-pro",
        display_name: "Gemini 3.1 Pro",
        context_limit: 1_048_576,
        output_limit: 65_535,
        input_modalities: &["text", "image", "audio", "video", "pdf"],
        output_modalities: &["text"],
        reasoning: true,
        tiers: &[
            (VariantTier::Low, SPEC_31_PRO_LOW),
            (VariantTier::Medium, SPEC_31_PRO_PREVIEW),
            (VariantTier::High, SPEC_31_PRO_PREVIEW),
        ],
        aliases: &[
            ("gemini-3.1-pro-high", AliasPolicy::HonorTier),
            ("gemini-pro", AliasPolicy::HonorTier),
            ("gemini-3.1-pro-low", AliasPolicy::Fixed(VariantTier::Low)),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_specs_preserve_the_client_budget() {
        assert!(SPEC_CLAUDE_SONNET_46.preserve_client_budget);
        assert!(SPEC_CLAUDE_OPUS_46.preserve_client_budget);
        assert_eq!(
            SPEC_CLAUDE_OPUS_46.effective_thinking_budget(Some(32_768)),
            32_768
        );
        assert_eq!(SPEC_CLAUDE_OPUS_46.effective_thinking_budget(None), 1_024);
    }

    #[test]
    fn non_claude_specs_keep_the_server_budget() {
        assert!(!SPEC_38_FLASH_LOW.preserve_client_budget);
        assert!(!SPEC_38_FLASH_MEDIUM.preserve_client_budget);
        assert!(!SPEC_38_FLASH_HIGH.preserve_client_budget);
        assert!(!SPEC_31_PRO_LOW.preserve_client_budget);
        assert!(!SPEC_31_PRO_PREVIEW.preserve_client_budget);
        assert!(!SPEC_31_FLASH_LITE.preserve_client_budget);
        assert!(!SPEC_GPT_OSS_120B.preserve_client_budget);
        assert_eq!(
            SPEC_31_PRO_PREVIEW.effective_thinking_budget(Some(32_768)),
            10_001
        );
    }

    #[test]
    fn test_infer_tier_ranges() {
        assert_eq!(infer_tier(None), VariantTier::High);
        assert_eq!(infer_tier(Some(0)), VariantTier::Low);
        assert_eq!(infer_tier(Some(1999)), VariantTier::Low);
        assert_eq!(infer_tier(Some(2000)), VariantTier::Medium);
        assert_eq!(infer_tier(Some(6999)), VariantTier::Medium);
        assert_eq!(infer_tier(Some(7000)), VariantTier::High);
        assert_eq!(infer_tier(Some(50000)), VariantTier::High);
    }

    #[test]
    fn test_resolve_35_flash_variants() {
        // High (default)
        let s = resolve("gemini-3.5-flash", None).unwrap();
        assert_eq!(s.id, "gemini-3.8-flash-high");
        assert_eq!(s.thinking_budget, 0);
        assert_eq!(s.max_output_tokens, 65536);

        // Medium
        let s = resolve("gemini-3.5-flash", Some(4000)).unwrap();
        assert_eq!(s.id, "gemini-3.8-flash-medium");
        assert_eq!(s.thinking_budget, 0);

        // Low
        let s = resolve("gemini-3.5-flash", Some(1000)).unwrap();
        assert_eq!(s.id, "gemini-3.8-flash-low");
        assert_eq!(s.thinking_budget, 0);
    }

    #[test]
    fn tier_from_effort_accepts_only_anthropic_sdk_values() {
        assert_eq!(tier_from_effort(Some("low")), Some(VariantTier::Low));
        assert_eq!(tier_from_effort(Some("medium")), Some(VariantTier::Medium));
        assert_eq!(tier_from_effort(Some("high")), Some(VariantTier::High));
        assert_eq!(tier_from_effort(Some("max")), None);
        assert_eq!(tier_from_effort(None), None);
    }

    #[test]
    fn resolve_with_tier_prefers_explicit_effort_over_budget() {
        for (canonical, tier, budget, expected_id) in [
            (
                "gemini-3.5-flash",
                VariantTier::Low,
                10_000,
                "gemini-3.8-flash-low",
            ),
            (
                "gemini-3.5-flash",
                VariantTier::Medium,
                1_000,
                "gemini-3.8-flash-medium",
            ),
            (
                "gemini-3.5-flash",
                VariantTier::High,
                1_000,
                "gemini-3.8-flash-high",
            ),
            (
                "gemini-3.1-pro",
                VariantTier::Low,
                10_000,
                "gemini-3.1-pro-low",
            ),
            (
                "gemini-3.1-pro",
                VariantTier::Medium,
                1_000,
                "gemini-3.1-pro-preview",
            ),
            (
                "gemini-3.1-pro",
                VariantTier::High,
                1_000,
                "gemini-3.1-pro-preview",
            ),
        ] {
            let spec = resolve_with_tier(canonical, Some(tier), Some(budget))
                .expect("canonical Gemini tier must resolve");
            assert_eq!(spec.id, expected_id);
        }
    }

    #[test]
    fn standalone_models_pass_through_without_variant_hijack() {
        for model in [
            "gemini-3-flash",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
            "gemini-2.5-flash-thinking",
        ] {
            assert!(
                resolve(model, None).is_none(),
                "{model} must pass through untouched"
            );
        }
    }

    #[test]
    fn test_resolve_31_pro_variants() {
        let s = resolve("gemini-3.1-pro", None).unwrap();
        assert_eq!(s.id, "gemini-3.1-pro-preview");
        assert_eq!(s.thinking_budget, 10001);
        assert_eq!(s.max_output_tokens, 65535); // Pro uses 65535, not 65536

        let s = resolve("gemini-3.1-pro", Some(1001)).unwrap();
        assert_eq!(s.id, "gemini-3.1-pro-low");
        assert_eq!(s.thinking_budget, 1001);
    }

    #[test]
    fn test_resolve_non_variant_models() {
        let s = resolve("gemini-3.1-flash-lite", None).unwrap();
        assert_eq!(s.id, "gemini-3.1-flash-lite");
        assert_eq!(s.thinking_budget, 0);
        assert_eq!(s.max_output_tokens, 65535);
        assert!(s.include_thoughts);

        let s = resolve("claude-sonnet-4-6", None).unwrap();
        assert_eq!(s.id, "claude-sonnet-4-6");
        assert_eq!(s.thinking_budget, 1024);
        assert_eq!(s.max_output_tokens, 64000);
    }

    #[test]
    fn test_unknown_model_returns_none() {
        assert!(resolve("some-unknown-model", None).is_none());
    }

    #[test]
    fn test_case_insensitive() {
        let s = resolve("GEMINI-3.5-FLASH", None).unwrap();
        assert_eq!(s.id, "gemini-3.8-flash-high");
    }

    /// Helper: call `resolve(canonical, budget)` and assert id / tb / mot.
    fn check(canonical: &str, budget: Option<u32>, id: &str, tb: u32, mot: u32) {
        let s = resolve(canonical, budget).unwrap_or_else(|| {
            panic!("resolve({canonical:?}, {budget:?}) unexpectedly returned None")
        });
        assert_eq!(s.id, id, "resolve({canonical:?}, {budget:?}).id");
        assert_eq!(
            s.thinking_budget, tb,
            "resolve({canonical:?}, {budget:?}).thinking_budget"
        );
        assert_eq!(
            s.max_output_tokens, mot,
            "resolve({canonical:?}, {budget:?}).max_output_tokens"
        );
    }

    #[test]
    fn baseline_resolve_35_flash_variants() {
        // ── gemini-3.5-flash (canonical) ───────────────────────────────
        check(
            "gemini-3.5-flash",
            None,
            "gemini-3.8-flash-high",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash",
            Some(0),
            "gemini-3.8-flash-low",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash",
            Some(4000),
            "gemini-3.8-flash-medium",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash",
            Some(7000),
            "gemini-3.8-flash-high",
            0,
            65536,
        );

        // ── gemini-3.5-flash-high ──────────────────────────────────────
        check(
            "gemini-3.5-flash-high",
            None,
            "gemini-3.8-flash-high",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-high",
            Some(0),
            "gemini-3.8-flash-low",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-high",
            Some(4000),
            "gemini-3.8-flash-medium",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-high",
            Some(7000),
            "gemini-3.8-flash-high",
            0,
            65536,
        );

        // ── gemini-3.5-flash-medium (fixed → SPEC_38_FLASH_MEDIUM) ─────
        check(
            "gemini-3.5-flash-medium",
            None,
            "gemini-3.8-flash-medium",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-medium",
            Some(0),
            "gemini-3.8-flash-medium",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-medium",
            Some(4000),
            "gemini-3.8-flash-medium",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-medium",
            Some(7000),
            "gemini-3.8-flash-medium",
            0,
            65536,
        );

        // ── gemini-3.5-flash-low (fixed → SPEC_38_FLASH_LOW) ───────────
        check(
            "gemini-3.5-flash-low",
            None,
            "gemini-3.8-flash-low",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-low",
            Some(0),
            "gemini-3.8-flash-low",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-low",
            Some(4000),
            "gemini-3.8-flash-low",
            0,
            65536,
        );
        check(
            "gemini-3.5-flash-low",
            Some(7000),
            "gemini-3.8-flash-low",
            0,
            65536,
        );
    }

    #[test]
    fn baseline_resolve_31_pro_variants() {
        // ── gemini-3.1-pro (canonical) ─────────────────────────────────
        check("gemini-3.1-pro", None, "gemini-3.1-pro-preview", 10001, 65535);
        check("gemini-3.1-pro", Some(0), "gemini-3.1-pro-low", 1001, 65535);
        check(
            "gemini-3.1-pro",
            Some(4000),
            "gemini-3.1-pro-preview",
            10001,
            65535,
        );
        check(
            "gemini-3.1-pro",
            Some(7000),
            "gemini-3.1-pro-preview",
            10001,
            65535,
        );

        // ── gemini-3.1-pro-high ────────────────────────────────────────
        check(
            "gemini-3.1-pro-high",
            None,
            "gemini-3.1-pro-preview",
            10001,
            65535,
        );
        check(
            "gemini-3.1-pro-high",
            Some(0),
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
        check(
            "gemini-3.1-pro-high",
            Some(4000),
            "gemini-3.1-pro-preview",
            10001,
            65535,
        );
        check(
            "gemini-3.1-pro-high",
            Some(7000),
            "gemini-3.1-pro-preview",
            10001,
            65535,
        );

        // ── gemini-pro ─────────────────────────────────────────────────
        check("gemini-pro", None, "gemini-3.1-pro-preview", 10001, 65535);
        check("gemini-pro", Some(0), "gemini-3.1-pro-low", 1001, 65535);
        check("gemini-pro", Some(4000), "gemini-3.1-pro-preview", 10001, 65535);
        check("gemini-pro", Some(7000), "gemini-3.1-pro-preview", 10001, 65535);

        // ── gemini-3.1-pro-low (fixed → SPEC_31_PRO_LOW) ──────────────
        check(
            "gemini-3.1-pro-low",
            None,
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
        check(
            "gemini-3.1-pro-low",
            Some(0),
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
        check(
            "gemini-3.1-pro-low",
            Some(4000),
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
        check(
            "gemini-3.1-pro-low",
            Some(7000),
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
    }

    #[test]
    fn baseline_resolve_non_variant_models() {
        check("gemini-3.1-flash-lite", None, "gemini-3.1-flash-lite", 0, 65535);
        check("gemini-3.1-flash-lite", Some(0), "gemini-3.1-flash-lite", 0, 65535);
        check("gemini-3.1-flash-lite", Some(7000), "gemini-3.1-flash-lite", 0, 65535);

        // ── Claude family ──────────────────────────────────────────────
        check("claude-sonnet-4-6", None, "claude-sonnet-4-6", 1024, 64000);
        check(
            "claude-opus-4-6-thinking",
            None,
            "claude-opus-4-6-thinking",
            1024,
            64000,
        );
        check(
            "claude-opus-4-6",
            None,
            "claude-opus-4-6-thinking",
            1024,
            64000,
        );

        // ── GPT OSS ────────────────────────────────────────────────────
        check(
            "gpt-oss-120b-medium",
            None,
            "gpt-oss-120b-medium",
            8192,
            32768,
        );
    }

    #[test]
    fn baseline_unknown_model() {
        assert!(resolve("some-unknown-model", None).is_none());
        assert!(resolve("gemini-4.0-super", Some(4000)).is_none());
        assert!(resolve("", None).is_none());
    }

    #[test]
    fn baseline_case_insensitive() {
        check(
            "GEMINI-3.5-FLASH",
            None,
            "gemini-3.8-flash-high",
            0,
            65536,
        );
        check(
            "Gemini-3.5-Flash",
            None,
            "gemini-3.8-flash-high",
            0,
            65536,
        );
        check("GEMINI-3.1-PRO", None, "gemini-3.1-pro-preview", 10001, 65535);
        check("CLAUDE-SONNET-4-6", None, "claude-sonnet-4-6", 1024, 64000);
        check(
            "GPT-OSS-120B-MEDIUM",
            None,
            "gpt-oss-120b-medium",
            8192,
            32768,
        );
    }

    #[test]
    fn old_catalog_alias_fallback() {
        // ── gemini-3.1-pro-high (HonorTier → same as gemini-3.1-pro) ────
        check(
            "gemini-3.1-pro-high",
            None,
            "gemini-3.1-pro-preview",
            10001,
            65535,
        );
        check(
            "gemini-3.1-pro-high",
            Some(0),
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
        check(
            "gemini-3.1-pro-high",
            Some(4000),
            "gemini-3.1-pro-preview",
            10001,
            65535,
        );
        check(
            "gemini-3.1-pro-high",
            Some(7000),
            "gemini-3.1-pro-preview",
            10001,
            65535,
        );

        // ── gemini-3.1-pro-low (Fixed(Low) → always gemini-3.1-pro-low)
        check(
            "gemini-3.1-pro-low",
            None,
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
        check(
            "gemini-3.1-pro-low",
            Some(0),
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
        check(
            "gemini-3.1-pro-low",
            Some(4000),
            "gemini-3.1-pro-low",
            1001,
            65535,
        );
        check(
            "gemini-3.1-pro-low",
            Some(7000),
            "gemini-3.1-pro-low",
            1001,
            65535,
        );

        // ── gemini-pro (HonorTier → same as gemini-3.1-pro) ────────────
        check("gemini-pro", None, "gemini-3.1-pro-preview", 10001, 65535);
        check("gemini-pro", Some(0), "gemini-3.1-pro-low", 1001, 65535);
        check("gemini-pro", Some(4000), "gemini-3.1-pro-preview", 10001, 65535);
        check("gemini-pro", Some(7000), "gemini-3.1-pro-preview", 10001, 65535);
    }
}
