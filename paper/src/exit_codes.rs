use std::process::ExitCode;

use hs_common::exit_codes::*;

pub fn from_error(err: &anyhow::Error) -> ExitCode {
    for cause in err.chain() {
        if let Some(pfe) = cause.downcast_ref::<crate::error::PaperError>() {
            use crate::error::{OutcomeKind, PaperError::*};
            return ExitCode::from(match pfe {
                InvalidInput(_) | NoDownloadUrl(_) => USAGE_ERROR,
                NotFound(_) | ParseError(_) | ProviderRejected(_) => GENERAL_ERROR,
                Http(_)
                | RateLimited { .. }
                | ProviderUnavailable(_)
                | CircuitBreakerOpen(_)
                | ProvidersFailed { .. } => NETWORK_ERROR,
                Io(_) | Storage(_) => GENERAL_ERROR,
                UnsafeUrl { .. } | TooLarge { .. } | NotPdf { .. } => GENERAL_ERROR,
                NoSourceYielded { sources, .. } => {
                    if sources.iter().any(|s| s.kind == OutcomeKind::Failed) {
                        NETWORK_ERROR
                    } else {
                        GENERAL_ERROR
                    }
                }
            });
        }
    }
    ExitCode::from(GENERAL_ERROR)
}
