//! Built-in provider presets.
//!
//! A preset only pre-fills `{protocol, base_url}` for the edit form — every
//! field stays editable after selection, and `preset_id: "custom"` means the
//! profile was created from scratch (just a base URL + protocol + key, no
//! endpoint guessing).
//!
//! Base URLs below reflect vendor documentation as of 2026-08. They are
//! defaults, not contracts: a relay moving its endpoint is a normal edit.

use crate::api::schema::{ProviderPresetCategory, ProviderPresetInfo, ProviderProtocol};

pub(crate) const CUSTOM_PRESET_ID: &str = "custom";

pub(crate) struct ProviderPreset {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) category: ProviderPresetCategory,
    pub(crate) protocol: ProviderProtocol,
    pub(crate) base_url: &'static str,
}

pub(crate) const PRESETS: &[ProviderPreset] = &[
    // -- official ------------------------------------------------------------
    ProviderPreset {
        id: "zhipu-glm",
        name: "Zhipu GLM",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://open.bigmodel.cn/api/anthropic",
    },
    ProviderPreset {
        id: "zhipu-glm-openai",
        name: "Zhipu GLM (OpenAI compat)",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: "https://open.bigmodel.cn/api/paas/v4",
    },
    ProviderPreset {
        id: "kimi",
        name: "Kimi (Anthropic compat)",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://api.moonshot.cn/anthropic",
    },
    ProviderPreset {
        id: "kimi-openai",
        name: "Kimi (OpenAI compat)",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: "https://api.moonshot.cn/v1",
    },
    ProviderPreset {
        id: "gemini",
        name: "Gemini",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::Gemini,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
    },
    ProviderPreset {
        id: "anthropic",
        name: "Anthropic",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://api.anthropic.com",
    },
    ProviderPreset {
        id: "xai-grok",
        name: "xAI Grok",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: "https://api.x.ai/v1",
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: "https://api.openai.com/v1",
    },
    ProviderPreset {
        id: "deepseek",
        name: "DeepSeek (Anthropic compat)",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://api.deepseek.com/anthropic",
    },
    ProviderPreset {
        id: "deepseek-openai",
        name: "DeepSeek (OpenAI compat)",
        category: ProviderPresetCategory::Official,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: "https://api.deepseek.com/v1",
    },
    // -- CN official ---------------------------------------------------------
    ProviderPreset {
        id: "zhipu-open-platform",
        name: "Zhipu Open Platform",
        category: ProviderPresetCategory::CnOfficial,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: "https://open.bigmodel.cn/api/paas/v4",
    },
    ProviderPreset {
        id: "volcengine-ark",
        name: "Volcengine Ark",
        category: ProviderPresetCategory::CnOfficial,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: "https://ark.cn-beijing.volces.com/api/v3",
    },
    ProviderPreset {
        id: "volcengine-ark-anthropic",
        name: "Volcengine Ark Coding Plan (Anthropic compat)",
        category: ProviderPresetCategory::CnOfficial,
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://ark.cn-beijing.volces.com/api/anthropic",
    },
    ProviderPreset {
        id: "siliconflow",
        name: "SiliconFlow",
        category: ProviderPresetCategory::CnOfficial,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: "https://api.siliconflow.cn/v1",
    },
    ProviderPreset {
        id: "siliconflow-anthropic",
        name: "SiliconFlow (Anthropic compat)",
        category: ProviderPresetCategory::CnOfficial,
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://api.siliconflow.cn/anthropic",
    },
    ProviderPreset {
        id: "minimax-anthropic",
        name: "MiniMax (Anthropic compat)",
        category: ProviderPresetCategory::CnOfficial,
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://api.minimax.chat/v1/anthropic",
    },
    ProviderPreset {
        id: "aliyun-bailian-anthropic",
        name: "Aliyun Bailian Coding Plan (Anthropic compat)",
        category: ProviderPresetCategory::CnOfficial,
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://dashscope.aliyuncs.com/api/v2/anthropic",
    },
];

pub(crate) fn find(preset_id: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|preset| preset.id == preset_id)
}

/// Wire form for `provider.presets` — the custom "preset" is appended by the
/// handler, not stored here, so the table only holds real presets.
pub(crate) fn preset_infos() -> Vec<ProviderPresetInfo> {
    PRESETS
        .iter()
        .map(|preset| ProviderPresetInfo {
            id: preset.id.to_string(),
            name: preset.name.to_string(),
            category: preset.category,
            protocol: preset.protocol,
            base_url: preset.base_url.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_ids_are_unique() {
        let mut ids: Vec<&str> = PRESETS.iter().map(|preset| preset.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate preset ids in table");
    }

    #[test]
    fn preset_count_matches_requirement_doc() {
        // 10 official + 7 CN official = 17 built-in presets (需求文档 FR-1.2).
        assert_eq!(PRESETS.len(), 17);
        let official = PRESETS
            .iter()
            .filter(|preset| preset.category == ProviderPresetCategory::Official)
            .count();
        let cn_official = PRESETS
            .iter()
            .filter(|preset| preset.category == ProviderPresetCategory::CnOfficial)
            .count();
        assert_eq!(official, 10);
        assert_eq!(cn_official, 7);
    }

    #[test]
    fn presets_use_https_urls_without_trailing_slash() {
        for preset in PRESETS {
            assert!(preset.base_url.starts_with("https://"), "{}", preset.id);
            assert!(!preset.base_url.ends_with('/'), "{}", preset.id);
        }
    }

    #[test]
    fn find_returns_registered_preset() {
        assert!(find("anthropic").is_some());
        assert!(find(CUSTOM_PRESET_ID).is_none());
        assert!(find("does-not-exist").is_none());
    }
}
