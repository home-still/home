use std::process::ExitCode;

use hs_style::exit_codes::*;

pub fn from_error(err: &anyhow::Error) -> ExitCode {
    for cause in err.chain() {
        if let Some(pfe) = cause.downcast_ref::<crate::error::PaperError>() {
            use crate::error::PaperError::*;
            return ExitCode::from(match pfe {
                InvalidInput(_) | NoDownloadUrl(_) => USAGE_ERROR,
                NotFound(_) | ParseError(_) => GENERAL_ERROR,
                Http(_) | RateLimited { .. } | ProviderUnavailable(_) | CircuitBreakerOpen(_) => {
                    NETWORK_ERROR
                }
                Io(_) => GENERAL_ERROR,
            });
        }
    }
    ExitCode::from(GENERAL_ERROR)
}
