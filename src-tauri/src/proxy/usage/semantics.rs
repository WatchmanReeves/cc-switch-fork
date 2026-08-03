//! Input-token semantics carried with every live proxy request.
//!
//! `app_type` is a product/UI ownership dimension. It must not decide whether
//! an upstream's input count already contains cache buckets: Pi can route the
//! same logical app through four different wire families.

use crate::pi_config::gateway::PiGatewayApiFamily;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum InputTokenSemantics {
    /// OpenAI Responses/Completions and Google usage totals include cached
    /// input. Fresh input is total minus the reported cache buckets.
    TotalIncludesCacheBuckets = 1,
    /// Anthropic reports fresh input separately from cache reads/creation.
    FreshExcludesCache = 2,
}

impl InputTokenSemantics {
    pub const fn stored_value(self) -> i64 {
        self as i64
    }

    pub const fn for_pi_family(family: PiGatewayApiFamily) -> Self {
        match family {
            PiGatewayApiFamily::AnthropicMessages => Self::FreshExcludesCache,
            PiGatewayApiFamily::OpenAiCompletions
            | PiGatewayApiFamily::OpenAiResponses
            | PiGatewayApiFamily::GoogleGenerativeAi => Self::TotalIncludesCacheBuckets,
        }
    }
}
