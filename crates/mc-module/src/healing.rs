//! Serializer healing profiles and the module residuals they leave behind.
//!
//! The serializer profile is the operational key for provider-specific cleanup that
//! happens before a request reaches the provider. The module only performs residual
//! work that the selected serializer does not already cover.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SerializerProfile {
    OwnedLlmRunner,
    ClaudeCodeAnthropic,
    OpencodeAiSdk,
    Pi,
}

const ALL_PROFILES: [SerializerProfile; 4] = [
    SerializerProfile::OwnedLlmRunner,
    SerializerProfile::ClaudeCodeAnthropic,
    SerializerProfile::OpencodeAiSdk,
    SerializerProfile::Pi,
];

impl SerializerProfile {
    pub const fn wire_id(self) -> &'static str {
        match self {
            Self::OwnedLlmRunner => "owned-llmrunner",
            Self::ClaudeCodeAnthropic => "claude-code-anthropic",
            Self::OpencodeAiSdk => "opencode-aisdk",
            Self::Pi => "pi",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owned-llmrunner" => Some(Self::OwnedLlmRunner),
            "claude-code-anthropic" => Some(Self::ClaudeCodeAnthropic),
            "opencode-aisdk" => Some(Self::OpencodeAiSdk),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }

    pub const fn all() -> &'static [Self] {
        &ALL_PROFILES
    }
}

impl Serialize for SerializerProfile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.wire_id())
    }
}

impl<'de> Deserialize<'de> for SerializerProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            serde::de::Error::custom(format!("unknown serializer profile {value:?}"))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealingCoverage {
    /// The serializer removes empty text and reasoning blocks before provider dispatch.
    pub drops_empty_content: bool,
    /// The serializer supplies an empty reasoning field for providers that require one.
    pub autofills_reasoning: bool,
    /// The serializer coalesces adjacent assistant messages before provider dispatch.
    pub merges_consecutive_assistants: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuirkResidual {
    /// Empty blocks can still reach a provider that rejects them, so module reductions must
    /// use non-empty placeholders rather than relying on the serializer to drop empties.
    pub requires_non_anthropic_empty_sentinels: bool,
    /// Adjacent assistants can be merged downstream, so only one reasoning block can survive
    /// a consecutive-assistant run.
    pub strips_reasoning_from_merged_assistants: bool,
}

pub const fn coverage(profile: SerializerProfile) -> HealingCoverage {
    match profile {
        SerializerProfile::OwnedLlmRunner | SerializerProfile::Pi => HealingCoverage {
            drops_empty_content: true,
            autofills_reasoning: true,
            merges_consecutive_assistants: false,
        },
        SerializerProfile::ClaudeCodeAnthropic => HealingCoverage {
            drops_empty_content: false,
            autofills_reasoning: false,
            merges_consecutive_assistants: false,
        },
        SerializerProfile::OpencodeAiSdk => HealingCoverage {
            drops_empty_content: false,
            autofills_reasoning: false,
            merges_consecutive_assistants: true,
        },
    }
}

/// Whether the consumer applies the module's TAIL mutations to the real context.
///
/// A byte-splice consumer (the Claude Code leg) replaces only the summary PREFIX in
/// the original request and keeps the tail bytes verbatim, so any module-side tail
/// mutation would be silently discarded wire-side while the module freezes it
/// durably: pressure accounting would believe tokens were reclaimed that the real
/// context still carries (phantom reclaim). For such profiles every tail mutator is
/// disabled — reduction selection, agent-drop DRAIN (append stays accepted; the
/// queue is durable and drains if the session ever runs under a tail-reclaim
/// profile, where the profile change is itself a HARD bust), synthetic-todo
/// injection, and the emergency tail-eviction arms. The emergency FOLD arm stays
/// on: prefix compaction is the byte-splice consumer's designed operation and its
/// only >=95% reclaim.
pub const fn tail_reclaim(profile: SerializerProfile) -> bool {
    match profile {
        SerializerProfile::ClaudeCodeAnthropic => false,
        SerializerProfile::OwnedLlmRunner
        | SerializerProfile::Pi
        | SerializerProfile::OpencodeAiSdk => true,
    }
}

pub const fn quirk_residual(profile: SerializerProfile) -> QuirkResidual {
    match profile {
        SerializerProfile::OpencodeAiSdk => QuirkResidual {
            requires_non_anthropic_empty_sentinels: true,
            strips_reasoning_from_merged_assistants: true,
        },
        SerializerProfile::OwnedLlmRunner
        | SerializerProfile::ClaudeCodeAnthropic
        | SerializerProfile::Pi => QuirkResidual {
            requires_non_anthropic_empty_sentinels: false,
            strips_reasoning_from_merged_assistants: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_profiles_parse_and_round_trip_wire_ids() {
        for profile in SerializerProfile::all() {
            assert_eq!(SerializerProfile::parse(profile.wire_id()), Some(*profile));
            assert_eq!(serde_json::to_value(profile).unwrap(), profile.wire_id());
        }
        assert_eq!(SerializerProfile::parse(""), None);
        assert_eq!(SerializerProfile::parse("unknown"), None);
    }

    #[test]
    fn coverage_table_is_pinned_per_serializer_profile() {
        assert_eq!(
            coverage(SerializerProfile::OwnedLlmRunner),
            HealingCoverage {
                drops_empty_content: true,
                autofills_reasoning: true,
                merges_consecutive_assistants: false,
            }
        );
        assert_eq!(
            coverage(SerializerProfile::Pi),
            coverage(SerializerProfile::OwnedLlmRunner)
        );
        assert_eq!(
            coverage(SerializerProfile::ClaudeCodeAnthropic),
            HealingCoverage {
                drops_empty_content: false,
                autofills_reasoning: false,
                merges_consecutive_assistants: false,
            }
        );
        assert_eq!(
            coverage(SerializerProfile::OpencodeAiSdk),
            HealingCoverage {
                drops_empty_content: false,
                autofills_reasoning: false,
                merges_consecutive_assistants: true,
            }
        );
    }

    #[test]
    fn residual_table_is_pinned_per_serializer_profile() {
        let empty = QuirkResidual {
            requires_non_anthropic_empty_sentinels: false,
            strips_reasoning_from_merged_assistants: false,
        };
        assert_eq!(quirk_residual(SerializerProfile::OwnedLlmRunner), empty);
        assert_eq!(quirk_residual(SerializerProfile::Pi), empty);
        assert_eq!(
            quirk_residual(SerializerProfile::ClaudeCodeAnthropic),
            empty
        );
        assert_eq!(
            quirk_residual(SerializerProfile::OpencodeAiSdk),
            QuirkResidual {
                requires_non_anthropic_empty_sentinels: true,
                strips_reasoning_from_merged_assistants: true,
            }
        );
    }
}
